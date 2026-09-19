use super::{
    GlyphMapping, Highlight, TextGlyph, TextGlyphContents, TruncationSide, VimColNrT, VimSAttrT,
    VimSCharT, VirtualColumns, VirtualTextHighlightMode, schar_cells_impl, schar_from_ascii,
    schar_to_ascii,
};

/// Complete contents of one temporary rendered screen cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct LineCell {
    pub(super) character: VimSCharT,
    pub(super) attribute: VimSAttrT,
    pub(super) virtual_column: VimColNrT,
}

impl LineCell {
    /// Creates a cell containing its character and rendering metadata.
    pub(super) const fn new(
        character: VimSCharT,
        attribute: VimSAttrT,
        virtual_column: VimColNrT,
    ) -> Self {
        Self {
            character,
            attribute,
            virtual_column,
        }
    }
}

#[derive(Debug)]
pub struct LineBuffer {
    pub(super) chars: Vec<VimSCharT>,
    pub(super) attrs: Vec<VimSAttrT>,
    pub(super) vcols: Vec<VimColNrT>,
    pub(super) mirror_scratch: Vec<LineCell>,
    pub(super) size: usize,
}

impl LineBuffer {
    pub(super) const fn empty() -> Self {
        Self {
            chars: Vec::new(),
            attrs: Vec::new(),
            vcols: Vec::new(),
            mirror_scratch: Vec::new(),
            size: 0,
        }
    }

    pub(super) fn realloc(&mut self, size: usize) {
        self.size = size;
        self.chars = vec![0; size];
        self.attrs = vec![0; size];
        self.vcols = vec![0; size];
        self.mirror_scratch = vec![LineCell::default(); size];
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
        self.chars.fill(0xFF);
        self.attrs.fill(0xFF);
    }

    pub(super) fn fill(
        &mut self,
        range: std::ops::Range<usize>,
        sc: VimSCharT,
        attr: VimSAttrT,
        vcol: VimColNrT,
    ) {
        for i in range {
            self.chars[i] = sc;
            self.attrs[i] = attr;
            self.vcols[i] = vcol;
        }
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
        let (Some(char_cell), Some(attr_cell), Some(vcol_cell)) = (
            self.chars.get_mut(col),
            self.attrs.get_mut(col),
            self.vcols.get_mut(col),
        ) else {
            return false;
        };

        if let Some(character) = character {
            *char_cell = character;
        }
        *attr_cell = attr;
        *vcol_cell = vcol;
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
        let Some(end_col) = col.checked_add(glyph.width) else {
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

        if line_end > self.chars.len() || end_col > self.attrs.len() || end_col > self.vcols.len() {
            return false;
        }
        let starts_in_continuation = self.chars[col] == 0;
        if starts_in_continuation && (!repair_leading_continuation || col == 0) {
            return false;
        }

        if starts_in_continuation {
            self.chars[col - 1] = schar_from_ascii(' ');
            self.chars[col] = schar_from_ascii(' ');
        }
        let line_chars = &mut self.chars[col..line_end];
        let attrs = &mut self.attrs[col..end_col];
        let vcols = &mut self.vcols[col..end_col];

        if glyph.width < max_cells && line_chars[glyph.width] == 0 {
            line_chars[glyph.width] = schar_from_ascii(' ');
        }
        let chars = &mut line_chars[..glyph.width];
        match glyph.contents {
            TextGlyphContents::Character(character) => {
                chars[0] = character;
                if glyph.width > 1 {
                    chars[1] = 0;
                }
            }
            TextGlyphContents::Spaces => chars.fill(schar_from_ascii(' ')),
        }
        attrs.fill(attr);
        match virtual_columns {
            VirtualColumns::Incrementing(first) => {
                for (offset, vcol) in vcols.iter_mut().enumerate() {
                    *vcol = first + offset as VimColNrT;
                }
            }
            VirtualColumns::Provided(values) => {
                vcols.copy_from_slice(&values[..glyph.width]);
            }
            VirtualColumns::Unavailable => vcols.fill(-1),
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
        let Some(&background_attr) = self.attrs.get(col) else {
            return false;
        };
        if end_col > self.attrs.len() {
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
            self.attrs[col..end_col].fill(attr.0);
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
        let Some(vcols) = self.vcols.get_mut(range) else {
            return false;
        };
        vcols.fill(vcol);
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
        let Some(vcols) = self.vcols.get_mut(range) else {
            return false;
        };
        let Some(last_offset) = vcols.len().checked_sub(1) else {
            return true;
        };
        let Ok(last_offset) = VimColNrT::try_from(last_offset) else {
            return false;
        };
        if first_vcol.checked_add(last_offset).is_none() {
            return false;
        }

        for (offset, vcol) in vcols.iter_mut().enumerate() {
            *vcol = first_vcol + offset as VimColNrT;
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
        let Some(chars) = self.chars.get_mut(range.clone()) else {
            return false;
        };
        let Some(attrs) = self.attrs.get_mut(range) else {
            return false;
        };

        chars.fill(sc);
        attrs.fill(attr);
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
        let Some(destination) = self.chars.get_mut(start_col..end_col) else {
            return false;
        };
        destination.copy_from_slice(chars);
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
        let Some(chars) = self.chars.get_mut(range.clone()) else {
            return;
        };
        let Some(attrs) = self.attrs.get_mut(range) else {
            return;
        };
        if chars.is_empty() {
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
            TruncationSide::Right => chars.len() - 1,
        };

        match side {
            TruncationSide::Left => {
                if chars.len() > 1 && chars[marker_col + 1] == 0 {
                    chars[marker_col + 1] = schar_from_ascii(' ');
                }
            }
            TruncationSide::Right => {
                if chars.len() > 1 && chars[marker_col] == 0 {
                    chars[marker_col - 1] = schar_from_ascii(' ');
                }
            }
        }
        chars[marker_col] = marker;
        attrs[marker_col] = attr;
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
        if width > self.chars.len() || width > self.attrs.len() {
            return false;
        }

        let marker_start = if preserve_line_number {
            self.chars[..width]
                .iter()
                .take_while(|&&character| {
                    schar_to_ascii(character).is_some_and(|c| c.is_ascii_digit())
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

        for col in first..last {
            self.mirror_scratch[col] =
                LineCell::new(self.chars[col], self.attrs[col], self.vcols[col]);
        }

        for (source, destination) in (first..last).zip((width - last..width - first).rev()) {
            let cell = self.mirror_scratch[source];
            self.attrs[destination] = cell.attribute;
            self.vcols[destination] = cell.virtual_column;
        }

        let mirror = width - 1;
        let mut col = first;
        while col < last {
            let rev = mirror - col;
            if col + 1 < last && self.mirror_scratch[col + 1].character == 0 {
                self.chars[rev - 1] = self.mirror_scratch[col].character;
                self.chars[rev] = 0;
                col += 1;
            } else {
                self.chars[rev] = self.mirror_scratch[col].character;
            }
            col += 1;
        }
    }
}
