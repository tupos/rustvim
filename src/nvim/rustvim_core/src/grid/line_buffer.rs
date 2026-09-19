use super::{
    GlyphMapping, Highlight, TextGlyph, TextGlyphContents, TruncationSide, VimColNrT, VimSAttrT,
    VimSCharT, VirtualColumns, VirtualTextHighlightMode, cell::GridCell, schar_cells_impl,
    schar_from_ascii, schar_to_ascii,
};

/// Owned temporary storage for one rendered screen line.
#[derive(Debug)]
pub struct LineBuffer {
    pub(super) cells: Vec<GridCell>,
    pub(super) mirror_scratch: Vec<GridCell>,
}

impl LineBuffer {
    pub(super) const fn empty() -> Self {
        Self {
            cells: Vec::new(),
            mirror_scratch: Vec::new(),
        }
    }

    pub(super) fn realloc(&mut self, size: usize) {
        self.cells = vec![GridCell::default(); size];
        self.mirror_scratch = vec![GridCell::default(); size];
    }

    /// Returns the number of initialized screen cells.
    pub(super) fn len(&self) -> usize {
        self.cells.len()
    }

    /// Returns whether the buffer has no initialized screen cells.
    pub(super) fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub(super) fn new(cols: usize) -> Self {
        let mut line_buffer = Self::empty();
        line_buffer.realloc(cols);
        line_buffer
    }

    pub(super) fn release(&mut self) {
        *self = Self::empty();
    }

    pub(super) fn invalidate_chars_and_attrs(&mut self) {
        for cell in &mut self.cells {
            cell.character = 0xFF;
            cell.attribute = 0xFF;
        }
    }

    pub(super) fn fill(
        &mut self,
        range: std::ops::Range<usize>,
        sc: VimSCharT,
        attr: VimSAttrT,
        vcol: VimColNrT,
    ) {
        self.cells[range].fill(GridCell::new(sc, attr, vcol));
    }

    /// Updates a cell's metadata and optionally replaces its character.
    ///
    /// Returns whether the cell belongs to the initialized buffer.
    pub(super) fn update_cell(
        &mut self,
        col: usize,
        character: Option<VimSCharT>,
        attr: VimSAttrT,
        vcol: VimColNrT,
    ) -> bool {
        let Some(cell) = self.cells.get_mut(col) else {
            return false;
        };

        if let Some(character) = character {
            cell.character = character;
        }
        cell.attribute = attr;
        cell.virtual_column = vcol;
        true
    }

    /// Writes one rendered glyph and all metadata belonging to its screen cells.
    ///
    /// A double-width glyph occupies a character cell followed by an empty
    /// continuation cell. Both cells receive the same attribute, and their
    /// virtual columns are consecutive.
    ///
    /// Returns the number of occupied cells, or `None` if the complete glyph
    /// does not fit or its continuation virtual column is not representable.
    pub(super) fn put_rendered_glyph(
        &mut self,
        col: usize,
        character: VimSCharT,
        attr: VimSAttrT,
        vcol: VimColNrT,
        glyph_map: &GlyphMapping,
    ) -> Option<usize> {
        let width = if schar_cells_impl(character, glyph_map) > 1 {
            2
        } else {
            1
        };
        let glyph = TextGlyph {
            contents: TextGlyphContents::Character(character),
            width,
            bytes_consumed: 0,
        };

        self.put_text_glyph(
            col,
            glyph,
            attr,
            VirtualColumns::Incrementing(vcol),
            width,
            false,
        )
        .then_some(width)
    }

    /// Writes one prepared text glyph and its cell metadata.
    ///
    /// When the glyph overwrites the left half of a following double-width
    /// character, that continuation cell is cleared. The complete write is
    /// validated before any line-buffer state is changed.
    pub(super) fn put_text_glyph(
        &mut self,
        col: usize,
        glyph: TextGlyph,
        attr: VimSAttrT,
        virtual_columns: VirtualColumns<'_>,
        max_cells: usize,
        repair_leading_continuation: bool,
    ) -> bool {
        if glyph.width == 0 || glyph.width > max_cells {
            return false;
        }
        let Some(line_end) = col.checked_add(max_cells) else {
            return false;
        };
        match virtual_columns {
            VirtualColumns::Incrementing(first) => {
                let Ok(last_offset) = VimColNrT::try_from(glyph.width - 1) else {
                    return false;
                };
                if first.checked_add(last_offset).is_none() {
                    return false;
                }
            }
            VirtualColumns::Provided(values) if values.len() < glyph.width => return false,
            VirtualColumns::Provided(_)
            | VirtualColumns::Unavailable
            | VirtualColumns::Preserve => {}
        }

        let Some(line_cells) = self.cells.get_mut(col..line_end) else {
            return false;
        };
        let starts_in_continuation = line_cells[0].character == 0;
        if starts_in_continuation && (!repair_leading_continuation || col == 0) {
            return false;
        }

        if starts_in_continuation {
            self.cells[col - 1].character = schar_from_ascii(' ');
            self.cells[col].character = schar_from_ascii(' ');
        }

        let line_cells = &mut self.cells[col..line_end];
        if glyph.width < max_cells && line_cells[glyph.width].character == 0 {
            line_cells[glyph.width].character = schar_from_ascii(' ');
        }
        let glyph_cells = &mut line_cells[..glyph.width];
        match glyph.contents {
            TextGlyphContents::Character(character) => {
                glyph_cells[0].character = character;
                if glyph.width > 1 {
                    glyph_cells[1].character = 0;
                }
            }
            TextGlyphContents::Spaces => {
                for cell in glyph_cells.iter_mut() {
                    cell.character = schar_from_ascii(' ');
                }
            }
        }
        for cell in glyph_cells.iter_mut() {
            cell.attribute = attr;
        }
        match virtual_columns {
            VirtualColumns::Incrementing(first) => {
                for (offset, cell) in glyph_cells.iter_mut().enumerate() {
                    cell.virtual_column = first + offset as VimColNrT;
                }
            }
            VirtualColumns::Provided(values) => {
                for (cell, &virtual_column) in glyph_cells.iter_mut().zip(&values[..glyph.width]) {
                    cell.virtual_column = virtual_column;
                }
            }
            VirtualColumns::Unavailable => {
                for cell in glyph_cells.iter_mut() {
                    cell.virtual_column = -1;
                }
            }
            VirtualColumns::Preserve => {}
        }
        true
    }

    /// Overlays one virtual-text glyph while preserving virtual columns.
    ///
    /// Blend-through spaces retain the existing characters and only update
    /// their attributes. Other glyphs repair either half of an overwritten
    /// double-width character before replacing it.
    pub(super) fn put_virtual_text_glyph(
        &mut self,
        col: usize,
        glyph: TextGlyph,
        virt_attr: VimSAttrT,
        highlight_mode: VirtualTextHighlightMode,
        source_is_space: bool,
        max_cells: usize,
    ) -> bool {
        if glyph.width == 0 || glyph.width > max_cells {
            return false;
        }
        let Some(end_col) = col.checked_add(glyph.width) else {
            return false;
        };
        let Some(background_attr) = self.cells.get(col).map(|cell| cell.attribute) else {
            return false;
        };
        if end_col > self.cells.len() {
            return false;
        }

        let (attr, through) = match highlight_mode {
            VirtualTextHighlightMode::Replace => (Highlight(virt_attr), false),
            VirtualTextHighlightMode::Combine => (
                Highlight(background_attr).combine(Highlight(virt_attr)),
                false,
            ),
            VirtualTextHighlightMode::Blend => {
                Highlight(background_attr).blend(Highlight(virt_attr), source_is_space)
            }
        };
        if through {
            for cell in &mut self.cells[col..end_col] {
                cell.attribute = attr.0;
            }
            true
        } else {
            self.put_text_glyph(
                col,
                glyph,
                attr.0,
                VirtualColumns::Preserve,
                max_cells,
                true,
            )
        }
    }

    /// Fills a virtual-column range with one value.
    ///
    /// Returns whether the complete range belongs to the initialized buffer.
    pub(super) fn fill_vcols(&mut self, range: std::ops::Range<usize>, vcol: VimColNrT) -> bool {
        let Some(cells) = self.cells.get_mut(range) else {
            return false;
        };
        for cell in cells {
            cell.virtual_column = vcol;
        }
        true
    }

    /// Fills a virtual-column range with consecutive values.
    ///
    /// Returns whether the complete range belongs to the initialized buffer
    /// and every generated virtual column is representable.
    pub(super) fn fill_vcols_from(
        &mut self,
        range: std::ops::Range<usize>,
        first_vcol: VimColNrT,
    ) -> bool {
        let Some(cells) = self.cells.get_mut(range) else {
            return false;
        };
        let Some(last_offset) = cells.len().checked_sub(1) else {
            return true;
        };
        let Ok(last_offset) = VimColNrT::try_from(last_offset) else {
            return false;
        };
        if first_vcol.checked_add(last_offset).is_none() {
            return false;
        }

        for (offset, cell) in cells.iter_mut().enumerate() {
            cell.virtual_column = first_vcol + offset as VimColNrT;
        }
        true
    }

    /// Fills character and attribute cells while preserving virtual columns.
    ///
    /// Returns whether the complete range belongs to the initialized buffer.
    pub(super) fn fill_chars_and_attrs(
        &mut self,
        range: std::ops::Range<usize>,
        sc: VimSCharT,
        attr: VimSAttrT,
    ) -> bool {
        let Some(cells) = self.cells.get_mut(range) else {
            return false;
        };

        for cell in cells {
            cell.character = sc;
            cell.attribute = attr;
        }
        true
    }

    /// Copies screen characters into the buffer without changing their
    /// attributes or virtual columns.
    ///
    /// Returns whether the complete character slice fits in the buffer.
    pub(super) fn put_chars(&mut self, start_col: usize, chars: &[VimSCharT]) -> bool {
        let Some(end_col) = start_col.checked_add(chars.len()) else {
            return false;
        };
        let Some(destination) = self.cells.get_mut(start_col..end_col) else {
            return false;
        };
        for (cell, &character) in destination.iter_mut().zip(chars) {
            cell.character = character;
        }
        true
    }

    /// Writes a truncation marker at one edge of a line-buffer range.
    ///
    /// When the marker replaces one half of a double-width character, the
    /// other half is replaced with a space. Empty or out-of-bounds ranges are
    /// ignored.
    pub(super) fn put_truncation_marker(
        &mut self,
        range: std::ops::Range<usize>,
        side: TruncationSide,
        marker: Option<VimSCharT>,
        attr: VimSAttrT,
    ) {
        let Some(cells) = self.cells.get_mut(range) else {
            return;
        };
        if cells.is_empty() {
            return;
        }

        let marker = marker.unwrap_or_else(|| {
            schar_from_ascii(match side {
                TruncationSide::Left => '<',
                TruncationSide::Right => '>',
            })
        });
        let marker_col = match side {
            TruncationSide::Left => 0,
            TruncationSide::Right => cells.len() - 1,
        };

        match side {
            TruncationSide::Left => {
                if cells.len() > 1 && cells[marker_col + 1].character == 0 {
                    cells[marker_col + 1].character = schar_from_ascii(' ');
                }
            }
            TruncationSide::Right => {
                if cells.len() > 1 && cells[marker_col].character == 0 {
                    cells[marker_col - 1].character = schar_from_ascii(' ');
                }
            }
        }
        cells[marker_col].character = marker;
        cells[marker_col].attribute = attr;
    }

    /// Places the three-cell marker used for a smooth-scrolled line.
    ///
    /// When requested, leading ASCII digits belonging to the line-number
    /// column are preserved. Replacing the first half of a double-width
    /// character also clears its second half.
    ///
    /// Returns whether `width` belongs to the initialized buffer.
    pub(super) fn put_smoothscroll_marker(
        &mut self,
        width: usize,
        preserve_line_number: bool,
        attr: VimSAttrT,
    ) -> bool {
        if width > self.cells.len() {
            return false;
        }

        let marker_start = if preserve_line_number {
            self.cells[..width]
                .iter()
                .take_while(|cell| {
                    schar_to_ascii(cell.character).is_some_and(|c| c.is_ascii_digit())
                })
                .count()
        } else {
            0
        };
        let marker_end = marker_start.saturating_add(3).min(width);
        let marker = schar_from_ascii('<');

        for col in marker_start..marker_end {
            self.put_truncation_marker(col..width, TruncationSide::Left, Some(marker), attr);
        }
        true
    }

    /// Mirrors a rendered range while retaining double-width character pairs.
    pub(super) fn mirror(&mut self, first: usize, last: usize, width: usize) {
        assert!(
            first <= last && last <= width && last - first <= width,
            "first={}, last={}, max_col={}",
            first,
            last,
            width
        );

        self.mirror_scratch[first..last].copy_from_slice(&self.cells[first..last]);

        for (source, destination) in (first..last).zip((width - last..width - first).rev()) {
            self.cells[destination] = self.mirror_scratch[source];
        }

        let mirror = width - 1;
        let mut col = first;
        while col < last {
            let rev = mirror - col;
            if col + 1 < last && self.mirror_scratch[col + 1].character == 0 {
                self.cells[rev - 1].character = self.mirror_scratch[col].character;
                self.cells[rev].character = 0;
                col += 1;
            } else {
                self.cells[rev].character = self.mirror_scratch[col].character;
            }
            col += 1;
        }
    }
}
