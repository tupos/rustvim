use super::{
    GlyphMapping, Highlight, LineBuffer, TextGlyph, TextGlyphContents, VimColNrT,
    VirtualTextHighlightMode, mbyte, tabstops::TabStops,
};

/// One owned virtual-text chunk with its resolved highlight.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct VirtualTextChunk {
    text: Box<[u8]>,
    highlight: Highlight,
}

impl VirtualTextChunk {
    /// Creates a chunk from owned text and its final highlight.
    pub(super) fn new(text: Vec<u8>, highlight: Highlight) -> Self {
        Self {
            text: text.into_boxed_slice(),
            highlight,
        }
    }
}

/// Owned virtual text ready to be rendered into a line buffer.
pub(super) struct VirtualText {
    chunks: Box<[VirtualTextChunk]>,
}

impl VirtualText {
    /// Creates virtual text from copied and validated chunks.
    pub(super) fn new(chunks: Vec<VirtualTextChunk>) -> Self {
        Self {
            chunks: chunks.into_boxed_slice(),
        }
    }

    /// Renders this complete virtual-text item into `line_buffer`.
    ///
    /// The returned column is the first cell following the rendered text.
    pub(super) fn render(
        &self,
        line_buffer: &mut LineBuffer,
        glyph_mapping: &mut GlyphMapping,
        options: VirtualTextRenderOptions<'_>,
    ) -> usize {
        if options.start_col > options.max_col || options.max_col > line_buffer.len() {
            return options.start_col;
        }

        let extension_highlight = if options.extend_highlight {
            self.chunks
                .last()
                .filter(|chunk| chunk.text.is_empty())
                .map(|chunk| chunk.highlight)
        } else {
            None
        };
        let mut renderer = VirtualTextRenderer {
            line_buffer,
            glyph_mapping,
            tab_stops: options.tab_stops,
            highlight_mode: options.highlight_mode,
            col: options.start_col,
            max_col: options.max_col,
            virtual_column: options.virtual_column,
            skip_cells: options.skip_cells,
        };

        for chunk in &self.chunks {
            renderer.render_chunk(chunk);
            if renderer.is_full() {
                break;
            }
        }

        if let Some(highlight) = extension_highlight {
            renderer.skip_cells = 0;
            renderer.fill_spaces(highlight);
        }

        renderer.col
    }
}

/// Parameters controlling the placement and appearance of virtual text.
pub(super) struct VirtualTextRenderOptions<'tabs> {
    start_col: usize,
    max_col: usize,
    virtual_column: VimColNrT,
    skip_cells: usize,
    extend_highlight: bool,
    tab_stops: &'tabs TabStops,
    highlight_mode: VirtualTextHighlightMode,
}

impl<'tabs> VirtualTextRenderOptions<'tabs> {
    /// Creates options for one complete virtual-text rendering operation.
    pub(super) fn new(
        start_col: usize,
        max_col: usize,
        virtual_column: VimColNrT,
        skip_cells: usize,
        extend_highlight: bool,
        tab_stops: &'tabs TabStops,
        highlight_mode: VirtualTextHighlightMode,
    ) -> Self {
        Self {
            start_col,
            max_col,
            virtual_column,
            skip_cells,
            extend_highlight,
            tab_stops,
            highlight_mode,
        }
    }
}

/// Mutable state for one safe virtual-text rendering operation.
struct VirtualTextRenderer<'a> {
    line_buffer: &'a mut LineBuffer,
    glyph_mapping: &'a mut GlyphMapping,
    tab_stops: &'a TabStops,
    highlight_mode: VirtualTextHighlightMode,
    col: usize,
    max_col: usize,
    virtual_column: VimColNrT,
    skip_cells: usize,
}

impl VirtualTextRenderer<'_> {
    /// Renders one chunk, preserving its highlight across all produced cells.
    fn render_chunk(&mut self, chunk: &VirtualTextChunk) {
        let mut text = chunk.text.as_ref();
        let mut clipped_padding = 0;

        while !self.is_full() && (clipped_padding > 0 || !text.is_empty()) {
            if clipped_padding > 0 {
                if !self.put_space(chunk.highlight) {
                    return;
                }
                clipped_padding -= 1;
                continue;
            }

            let cluster_len = mbyte::first_cluster_len(text);
            if cluster_len == 0 || cluster_len > text.len() {
                return;
            }

            if self.skip_cells > 0 {
                let Some(width) = self.input_width(text) else {
                    return;
                };
                let skipped = self.skip_cells.min(width);
                self.skip_cells -= skipped;
                clipped_padding = width - skipped;
                if !self.advance_virtual_column(width) {
                    return;
                }
                text = &text[cluster_len..];
                continue;
            }

            let max_cells = self.max_col - self.col;
            let tab_cells = if text[0] == b'\t' {
                self.tab_stops.padding_at(self.virtual_column)
            } else {
                0
            };
            let Some(glyph) = TextGlyph::from_text(text, max_cells, tab_cells, self.glyph_mapping)
            else {
                return;
            };
            let bytes_consumed = glyph.bytes_consumed;
            if !self.line_buffer.put_virtual_text_glyph(
                self.col,
                glyph,
                chunk.highlight.0,
                self.highlight_mode,
                text[0] == b' ',
                max_cells,
            ) {
                return;
            }

            self.col += glyph.width;
            if !self.advance_virtual_column(glyph.width) {
                return;
            }
            text = &text[bytes_consumed..];
        }
    }

    /// Returns the display width of the first input glyph.
    fn input_width(&self, text: &[u8]) -> Option<usize> {
        if text[0] == b'\t' {
            Some(self.tab_stops.padding_at(self.virtual_column))
        } else {
            usize::try_from(mbyte::first_char_cells(text))
                .ok()
                .filter(|&width| width > 0)
        }
    }

    /// Advances the source virtual column when its value remains representable.
    fn advance_virtual_column(&mut self, cells: usize) -> bool {
        let Ok(cells) = VimColNrT::try_from(cells) else {
            return false;
        };
        let Some(virtual_column) = self.virtual_column.checked_add(cells) else {
            return false;
        };
        self.virtual_column = virtual_column;
        true
    }

    /// Writes one padding or highlight-extension space.
    fn put_space(&mut self, highlight: Highlight) -> bool {
        let glyph = TextGlyph {
            contents: TextGlyphContents::Spaces,
            width: 1,
            bytes_consumed: 1,
        };
        if !self.line_buffer.put_virtual_text_glyph(
            self.col,
            glyph,
            highlight.0,
            self.highlight_mode,
            true,
            self.max_col - self.col,
        ) {
            return false;
        }
        self.col += 1;
        true
    }

    /// Extends an empty final chunk's highlight through the remaining cells.
    fn fill_spaces(&mut self, highlight: Highlight) {
        while !self.is_full() && self.put_space(highlight) {}
    }

    fn is_full(&self) -> bool {
        self.col >= self.max_col
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::schar_from_ascii;

    fn initialized_line_buffer(len: usize) -> LineBuffer {
        let mut line_buffer = LineBuffer::new(len);
        line_buffer.fill(0..len, schar_from_ascii(' '), 1, -1);
        line_buffer
    }

    fn chunk(text: &[u8], attribute: i32) -> VirtualTextChunk {
        VirtualTextChunk::new(text.to_vec(), Highlight(attribute))
    }

    fn options<'tabs>(
        max_col: usize,
        virtual_column: VimColNrT,
        skip_cells: usize,
        extend_highlight: bool,
        tab_stops: &'tabs TabStops,
    ) -> VirtualTextRenderOptions<'tabs> {
        VirtualTextRenderOptions::new(
            0,
            max_col,
            virtual_column,
            skip_cells,
            extend_highlight,
            tab_stops,
            VirtualTextHighlightMode::Replace,
        )
    }

    #[test]
    fn renders_complete_chunks_with_their_highlights() {
        let virtual_text = VirtualText::new(vec![chunk(b"ab", 10), chunk(b"c", 20)]);
        let tab_stops = TabStops::try_from_options(8, &[]).unwrap();
        let mut line_buffer = initialized_line_buffer(6);
        let mut glyph_mapping = GlyphMapping::new();

        let end_col = virtual_text.render(
            &mut line_buffer,
            &mut glyph_mapping,
            VirtualTextRenderOptions::new(
                1,
                5,
                0,
                0,
                false,
                &tab_stops,
                VirtualTextHighlightMode::Replace,
            ),
        );

        assert_eq!(end_col, 4);
        assert_eq!(
            line_buffer
                .cells
                .iter()
                .map(|cell| cell.character)
                .collect::<Vec<_>>(),
            [
                schar_from_ascii(' '),
                schar_from_ascii('a'),
                schar_from_ascii('b'),
                schar_from_ascii('c'),
                schar_from_ascii(' '),
                schar_from_ascii(' '),
            ]
        );
        assert_eq!(
            line_buffer
                .cells
                .iter()
                .map(|cell| cell.attribute)
                .collect::<Vec<_>>(),
            [1, 10, 10, 20, 1, 1]
        );
    }

    #[test]
    fn expands_tabs_using_variable_tab_stops() {
        let virtual_text = VirtualText::new(vec![chunk(b"\tX\t", 5)]);
        let tab_stops = TabStops::try_from_options(8, &[2, 4]).unwrap();
        let mut line_buffer = initialized_line_buffer(8);
        let mut glyph_mapping = GlyphMapping::new();

        let end_col = virtual_text.render(
            &mut line_buffer,
            &mut glyph_mapping,
            options(8, 0, 0, false, &tab_stops),
        );

        assert_eq!(end_col, 6);
        assert_eq!(line_buffer.cells[2].character, schar_from_ascii('X'));
        assert_eq!(
            line_buffer
                .cells
                .iter()
                .map(|cell| cell.attribute)
                .collect::<Vec<_>>(),
            [5, 5, 5, 5, 5, 5, 1, 1]
        );
    }

    #[test]
    fn pads_a_partially_skipped_wide_glyph() {
        let virtual_text = VirtualText::new(vec![chunk("界\tX".as_bytes(), 7)]);
        let tab_stops = TabStops::try_from_options(4, &[]).unwrap();
        let mut line_buffer = initialized_line_buffer(5);
        let mut glyph_mapping = GlyphMapping::new();

        let end_col = virtual_text.render(
            &mut line_buffer,
            &mut glyph_mapping,
            options(5, 0, 1, false, &tab_stops),
        );

        assert_eq!(end_col, 4);
        assert_eq!(line_buffer.cells[3].character, schar_from_ascii('X'));
        assert_eq!(
            line_buffer
                .cells
                .iter()
                .map(|cell| cell.attribute)
                .collect::<Vec<_>>(),
            [7, 7, 7, 7, 1]
        );
    }

    #[test]
    fn extends_an_empty_final_chunks_highlight() {
        let virtual_text = VirtualText::new(vec![chunk(b"x", 2), chunk(b"", 9)]);
        let tab_stops = TabStops::try_from_options(8, &[]).unwrap();
        let mut line_buffer = initialized_line_buffer(4);
        let mut glyph_mapping = GlyphMapping::new();

        let end_col = virtual_text.render(
            &mut line_buffer,
            &mut glyph_mapping,
            options(4, 0, 0, true, &tab_stops),
        );

        assert_eq!(end_col, 4);
        assert_eq!(
            line_buffer
                .cells
                .iter()
                .map(|cell| cell.attribute)
                .collect::<Vec<_>>(),
            [2, 9, 9, 9]
        );
    }
}
