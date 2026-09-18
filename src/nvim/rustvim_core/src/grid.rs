// vim: sw=4
use crate::{
    generated::enums::{RdbFlags, Slf},
    globals, mbyte, ui,
};
use std::{borrow::Borrow, collections::BTreeMap, rc::Rc};

/// if data[0] is 0xFF, then data[1..4] is a 24-bit index (in machine endianness)
/// otherwise it must be a UTF-8 string of length maximum 4 (no NUL when n=4)
pub type VimSCharT = u32;
pub type VimSAttrT = i32;
pub type VimColNrT = i32;

/// must be at least as big as the biggest of schar_T, sattr_T, colnr_T
pub type VimSScratchT = i32;
pub type VimHandleT = libc::c_int;
pub type VimDirtyColT = libc::c_int;

/// Includes final NUL. MAX_MCO is no longer used, but at least 4*(MAX_MCO+1)+1=29
/// ensures we can fit all composed chars which did fit before.
static MAX_SCHAR_SIZE: usize = 32;

static NULL_TERMINATOR: u8 = b'\0';

static mut GLYPH_MAPPING: GlyphMapping = GlyphMapping::new();

static DEFAULT_GRID_HANDLE: u32 = 1;

unsafe extern "C" {
    static mut full_screen: bool;
    static mut rdb_flags: libc::c_uint;

    static mut p_arshape: libc::c_int;
    static mut p_tbidi: libc::c_int;

    static mut default_grid: CScreenGrid;
    static mut msg_grid: CScreenGrid;
    static mut msg_grid_adj: CScreenGrid;
    static mut pum_grid: CScreenGrid;

    static mut last_grid_handle: libc::c_int;

    static mut linebuf_char: *mut VimSCharT;
    static mut linebuf_attr: *mut VimSAttrT;
    static mut linebuf_vcol: *mut VimColNrT;
    static mut linebuf_scratch: *mut VimSScratchT;
    static mut linebuf_size: libc::size_t;

    fn decor_check_invalid_glyphs();
    fn check_chars_options() -> bool;
    fn line_do_arabic_shape(buf: *mut VimSCharT, cols: libc::c_int);
    fn hl_combine_attr(char_attr: libc::c_int, prim_attr: libc::c_int) -> libc::c_int;
}

/// ScreenGrid represents a resizable rectuangular grid displayed by UI clients.
///
/// chars[] contains the UTF-8 text that is currently displayed on the grid.
/// It is stored as a single block of cells. When redrawing a part of the grid,
/// the new state can be compared with the existing state of the grid. This way
/// we can avoid sending bigger updates than necessary to the Ul layer.
///
/// Screen cells are stored as NUL-terminated UTF-8 strings, and a cell can
/// contain composing characters as many as fits in MAX_SCHAR_SIZE-1 bytes
/// The composing characters are to be drawn on top of the original character.
/// The content after the NUL is not defined (so comparison must be done a
/// single cell at a time). Double-width characters are stored in the left cell,
/// and the right cell should only contain the empty string. When a part of the
/// screen is cleared, the cells should be filled with a single whitespace char.
///
/// attrs[] contains the highlighting attribute for each cell.
///
/// vcols[] contains the virtual columns in the line. -1 means not available
/// or before buffer text.
/// -2 or -3 means in fold column and a mouse click should:
///  -2: open a fold
///  -3: close a fold
///
/// line_offset\[n\] is the offset from chars[], attrs[] and vcols[] for the start
/// of line 'n'. These offsets are in general not linear, as full screen scrolling
/// is implemented by rotating the offsets in the line_offset array.
#[derive(Debug, Clone)]
pub struct ScreenGrid {
    pub handle: VimHandleT,

    pub chars: Vec<VimSCharT>,
    pub attrs: Vec<VimSAttrT>,
    pub vcols: Vec<VimColNrT>,
    pub line_offset: Vec<usize>,
    /// last column that was drawn (not cleared with the default background).
    /// only used when "throttled" is set. Not allocated by grid_alloc!
    //dirty_col: Vec<VimDirtyColT>,
    dirty_col: *mut VimDirtyColT,

    /// the size of the allocated grid.
    pub rows: i32,
    pub cols: i32,

    /// The state of the grid is valid. Otherwise it needs to be redrawn.
    valid: bool,

    /// only draw internally and don't send updates yet to the compositor or
    /// external UI.
    throttled: bool,

    /// whether the compositor should blend the grid with the background grid
    blending: bool,

    /// whether the grid interacts with mouse events.
    mouse_enabled: bool,

    /// z-index: the order in the stack of grids.
    zindex: i32,

    /// Below is state owned by the compositor. Should generally not be set/read
    /// outside this module, except for specific compatibility hacks

    /// position of the grid on the composed screen.
    comp_row: i32,
    comp_col: i32,

    /// Requested width and height of the grid upon resize. Used by
    /// `ui_compositor` to correctly determine which regions need to
    /// be redrawn.
    comp_width: i32,
    comp_height: i32,

    /// z-index of the grid. Grids with higher index is draw on top.
    /// default_grid.comp_index is always zero.
    comp_index: usize,

    /// compositor should momentarily ignore the grid. Used internally when
    /// moving around grids etc.
    comp_disabled: bool,

    /// need to resend win_float_pos or similar due to comp_index change
    pending_comp_index_update: bool,
}

impl ScreenGrid {
    pub fn new(rows: i32, cols: i32, valid: bool, line_buffer: &mut LineBuffer) -> Self {
        let num_cells = rows as usize * cols as usize;

        let mut grid = ScreenGrid {
            handle: 0,
            chars: vec![schar_from_ascii(' '); num_cells],
            attrs: vec![Self::attrs_fill(valid); num_cells],
            vcols: vec![-1; num_cells],
            line_offset: Vec::with_capacity(rows as usize),
            dirty_col: std::ptr::null_mut(),
            //dirty_col: Vec::new(),
            rows,
            cols,
            valid: false,
            throttled: false,
            blending: false,
            mouse_enabled: true,
            zindex: 0,
            comp_row: 0,
            comp_col: 0,
            comp_width: 0,
            comp_height: 0,
            comp_index: 0,
            comp_disabled: false,
            pending_comp_index_update: true,
        };
        for i in 0..rows {
            let offset = i as usize * cols as usize;
            grid.line_offset.push(offset);
        }

        grid.realloc_line_buffer(line_buffer);

        grid
    }

    pub fn from_grid(
        grid: &ScreenGrid,
        rows: i32,
        cols: i32,
        valid: bool,
        copy: bool,
        line_buffer: &mut LineBuffer,
    ) -> Self {
        let mut new_grid = grid.clone();

        let num_cells = rows as usize * cols as usize;

        new_grid.chars = vec![schar_from_ascii(' '); num_cells];
        new_grid.attrs = vec![Self::attrs_fill(valid); num_cells];
        new_grid.vcols = vec![-1; num_cells];
        new_grid.line_offset = Vec::with_capacity(rows as usize);
        new_grid.rows = rows;
        new_grid.cols = cols;

        for i in 0..new_grid.rows as usize {
            let offset = i * new_grid.cols as usize;
            new_grid.line_offset.push(offset);

            // If the screen is not going to be cleared, copy as much as
            // possible from the old screen to the new one and clear the rest
            // (used when resizing the window at the "--more--" prompt or when
            // executing an external command, for the GUI).
            if copy {
                new_grid.copy_grid_content_from_old_grid(grid, i);
            }
        }

        new_grid.realloc_line_buffer(line_buffer);

        new_grid
    }

    fn realloc_line_buffer(&self, line_buffer: &mut LineBuffer) {
        let ucols = self.cols as usize;
        if ucols <= line_buffer.size {
            return;
        }
        line_buffer.realloc(ucols);
    }

    fn copy_grid_content_from_old_grid(&mut self, src: &ScreenGrid, new_row: usize) {
        if new_row >= src.rows as usize || src.chars.is_empty() {
            return;
        }
        let len = std::cmp::min(self.cols, src.cols) as usize;
        let range = self.line_offset[new_row]..self.line_offset[new_row] + len;
        let src_range = src.line_offset[new_row]..src.line_offset[new_row] + len;
        self.chars[range.clone()].copy_from_slice(&src.chars[src_range.clone()]);
        self.attrs[range.clone()].copy_from_slice(&src.attrs[src_range.clone()]);
        self.vcols[range.clone()].copy_from_slice(&src.vcols[src_range.clone()]);
    }

    /// # Safety
    ///
    /// The returned `CScreenGrid` becomes the unique owner of the `chars`,
    /// `attrs`, `vcols`, and `line_offset` allocations. The caller must not
    /// duplicate or independently free those pointers, must access them only
    /// within the lengths described by `rows` and `cols`, and must reconstruct
    /// at most one `ScreenGrid` from them.
    pub unsafe fn to_cgrid(self) -> CScreenGrid {
        CScreenGrid {
            handle: self.handle,
            chars: self.chars.leak().as_mut_ptr(),
            attrs: self.attrs.leak().as_mut_ptr(),
            vcols: self.vcols.leak().as_mut_ptr(),
            line_offset: self.line_offset.leak().as_mut_ptr(),
            dirty_col: self.dirty_col,
            //dirty_col: self.dirty_col.leak().as_mut_ptr(),
            rows: self.rows,
            cols: self.cols,
            valid: self.valid,
            throttled: self.throttled,
            blending: self.blending,
            mouse_enabled: self.mouse_enabled,
            zindex: self.zindex,
            comp_row: self.comp_row,
            comp_col: self.comp_col,
            comp_width: self.comp_width,
            comp_height: self.comp_height,
            comp_index: self.comp_index,
            comp_disabled: self.comp_disabled,
            pending_comp_index_update: self.pending_comp_index_update,
        }
    }

    fn from_cgrid(
        grid: &CScreenGrid,
        rows: i32,
        cols: i32,
        valid: bool,
        copy: bool,
        line_buffer: &mut LineBuffer,
    ) -> Self {
        let old_grid: ScreenGrid = grid.into();

        Self::from_grid(&old_grid, rows, cols, valid, copy, line_buffer)
    }

    pub fn clear_line(&mut self, offset: usize, width: usize, valid: bool) {
        let range = offset..(offset + width);
        self.chars[range.clone()].fill(schar_from_ascii(' '));
        self.attrs[range.clone()].fill(Self::attrs_fill(valid));
        self.vcols[range.clone()].fill(-1);
    }

    fn attrs_fill(valid: bool) -> VimSAttrT {
        if valid { 0 } else { -1 }
    }

    fn invalidate(&mut self) {
        self.attrs.fill(-1);
    }

    fn get_char_and_attr(&self, row: usize, col: usize) -> (VimSCharT, VimSAttrT) {
        if self.attrs.is_empty() || row >= self.rows as usize || col >= self.cols as usize {
            return (NULL_TERMINATOR as VimSCharT, 0);
        }
        let offset = self.line_offset[row] + col;
        (self.chars[offset], self.attrs[offset])
    }

    fn is_invalid_row(&self, row: usize) -> bool {
        self.attrs[self.line_offset[row]] < 0
    }

    /// Check whether the given character needs redrawing:
    /// - the (first byte of the) character is different
    /// - the attributes are different
    /// - the character is multi-byte and the next byte is different
    /// - the character is two cells wide and the second cell differs.
    fn is_char_needs_redraw(&self, col: usize, off_to: usize, cols: i32, lb: &LineBuffer) -> bool {
        unsafe {
            cols > 0
                && ((lb.chars[col] != self.chars[off_to]
                    || lb.attrs[col] != self.attrs[off_to]
                    || (cols > 1
                        && lb.chars[col + 1] == 0
                        && lb.chars[col + 1] != self.chars[off_to + 1]))
                    || (rdb_flags & RdbFlags::Nodelta as u32 != 0))
        }
    }

    /// Move one buffered line to the window grid, but only the characters that
    /// have actually changed.  Handle insert/delete character.
    ///
    /// @param coloff  gives the first column on the grid for this line.
    /// @param endcol  gives the columns where valid characters are.
    /// @param clear_width  see SLF_RIGHTLEFT.
    /// @param flags  can have bits:
    /// - SLF_RIGHTLEFT  rightleft text, like a window with 'rightleft' option set:
    ///   - When false, clear columns "endcol" to "clear_width".
    ///   - When true, clear columns "col" to "endcol".
    /// - SLF_WRAP  hint to UI that "row" contains a line wrapped into the next row.
    /// - SLF_INC_VCOL:
    ///   - When false, use "last_vcol" for grid->vcols[] of the columns to clear.
    ///   - When true, use an increasing sequence starting from "last_vcol + 1" for
    ///     grid->vcols[] of the columns to clear.
    //     TODO: Make size proper sizes and not i32;
    //     TODO: this has high cyclomatic complexity and should be rewritten!!!
    //           as currently it is just a copy paste of the old c impl
    // Keep the arguments aligned with grid_put_linebuf() in grid.c.
    #[allow(clippy::too_many_arguments)]
    fn put_line_buffer(
        &mut self,
        row: i32,
        col_offset: i32,
        mut col: i32,
        mut endcol: i32,
        mut clear_width: i32,
        bg_attr: i32,
        mut clear_attr: i32,
        mut last_vcol: VimColNrT,
        flags: i32,
        lb: &mut LineBuffer,
    ) {
        assert!(0 <= row && row < self.rows);
        let is_right_left_set = flags & Slf::Rightleft as i32 != 0;
        let is_inc_vcol_set = flags & Slf::IncVcol as i32 != 0;
        let is_wrap_set = flags & Slf::Wrap as i32 != 0;

        let mut clear_next = false;
        if endcol > self.cols {
            endcol = self.cols;
        }
        if row >= self.rows || col_offset >= self.cols {
            return;
        }
        let row = row as usize;
        let invalid_row = unsafe {
            self.chars.as_mut_ptr() != default_grid.chars && self.is_invalid_row(row) && col == 0
        };
        let offset_to = self.line_offset[row] + col_offset as usize;
        let max_offset_to = self.line_offset[row] + self.cols as usize;

        // When at the start of the text and overwriting the right half of a
        // two-cell character in the same grid, truncate that into a '>'.
        if col > 0 && self.chars[offset_to + col as usize] == 0 {
            lb.chars[col as usize - 1] = schar_from_ascii('>');
            lb.attrs[col as usize - 1] = self.attrs[offset_to + col as usize - 1];
            col -= 1;
        }

        let mut clear_start = endcol;
        if is_right_left_set {
            clear_start = col;
            col = endcol;
            endcol = clear_width;
            clear_width = col;
        }

        unsafe {
            if p_arshape != 0 && p_tbidi == 0 && endcol > col {
                line_do_arabic_shape(lb.chars.as_mut_ptr().add(col as usize), endcol - col);
            }
        }

        if bg_attr != 0 {
            for c in col as usize..endcol as usize {
                lb.attrs[c] = unsafe { hl_combine_attr(bg_attr, lb.attrs[c]) };
            }
        }
        let mut redraw_next =
            self.is_char_needs_redraw(col as usize, offset_to + col as usize, endcol - col, lb);

        let mut start_dirty = -1;
        let mut end_dirty = 0;
        while col < endcol {
            // 1: normal char
            // 2: occupies two display cells
            let char_cells = if col + 1 < endcol && lb.chars[col as usize + 1] == 0 {
                2
            } else {
                1
            };

            // Does this character need redraw?
            let redraw_this = redraw_next;
            let offset = offset_to + col as usize;
            redraw_next = self.is_char_needs_redraw(
                (col + char_cells) as usize,
                offset + char_cells as usize,
                endcol - col - char_cells,
                lb,
            );

            if redraw_this {
                if start_dirty == -1 {
                    start_dirty = col;
                }
                end_dirty = col + char_cells;

                // When writing a single-width character over a double-width
                // character and at the end of the redrawn text, need to clear out
                // the right half of the old character.
                // Also required when writing the right half of a double-width
                // char over the left half of an existing one
                if col + char_cells == endcol
                    && (offset + char_cells as usize) < max_offset_to
                    && self.chars[offset + char_cells as usize] == NULL_TERMINATOR as VimSCharT
                {
                    clear_next = true;
                }

                self.chars[offset] = lb.chars[col as usize];
                if char_cells == 2 {
                    self.chars[offset + 1] = lb.chars[col as usize + 1];
                }

                self.attrs[offset] = lb.attrs[col as usize];
                // For simplicity set the attributes of second half of a
                // double-wide character equal to the first half.
                if char_cells == 2 {
                    self.attrs[offset + 1] = lb.attrs[col as usize];
                }
            }

            self.vcols[offset] = lb.vcols[col as usize];
            if char_cells == 2 {
                self.vcols[offset + 1] = lb.vcols[col as usize + 1];
            }

            col += char_cells;
        } // while

        if clear_next {
            // Clear the second half of a double-wide character of which the left
            // half was overwritten with a single-wide character.
            self.chars[offset_to + col as usize] = schar_from_ascii(' ');
            end_dirty += 1;
        }

        // When clearing the left half of a double-wide char also clear the right half.
        if (offset_to + clear_width as usize) < max_offset_to
            && self.chars[offset_to + clear_width as usize] == 0
        {
            clear_width += 1;
        }

        let mut clear_dirty_start = -1;
        let mut clear_end: i32 = -1;
        if is_right_left_set {
            for c in (clear_start as usize..clear_width as usize).rev() {
                let off = offset_to + c;
                self.vcols[off] = if is_inc_vcol_set {
                    last_vcol += 1;
                    last_vcol
                } else {
                    last_vcol
                }
            }
        }
        clear_attr = unsafe { hl_combine_attr(bg_attr, clear_attr) };
        // blank out the rest of the line
        // TODO(bfredl): we could cache winline widths
        for c in clear_start as usize..clear_width as usize {
            let off = offset_to + c;
            if self.chars[off] != schar_from_ascii(' ')
                || self.attrs[off] != clear_attr
                || unsafe { rdb_flags & RdbFlags::Nodelta as u32 } != 0
            {
                self.chars[off] = schar_from_ascii(' ');
                self.attrs[off] = clear_attr;
                if clear_dirty_start == -1 {
                    clear_dirty_start = c as i32;
                }
                clear_end = c as i32 + 1;
            }
            if !is_right_left_set {
                self.vcols[off] = if is_inc_vcol_set {
                    last_vcol += 1;
                    last_vcol
                } else {
                    last_vcol
                }
            }
        }

        if is_right_left_set && start_dirty != -1 && clear_dirty_start != -1 {
            if self.throttled || clear_dirty_start >= start_dirty - 5 {
                // cannot draw now or too small to be worth a separate "clear" event
                start_dirty = clear_dirty_start;
            } else {
                ui::ui_line_r(
                    self,
                    row as i32,
                    invalid_row,
                    col_offset + clear_dirty_start,
                    col_offset + clear_dirty_start,
                    col_offset + clear_end,
                    clear_attr,
                    is_wrap_set,
                );
            }
            clear_end = end_dirty;
        } else {
            if start_dirty == -1 {
                // clear only
                start_dirty = clear_dirty_start;
                end_dirty = clear_dirty_start;
            } else if clear_end < end_dirty {
                // put only
                clear_end = end_dirty;
            } else {
                end_dirty = endcol;
            }
        }
        if clear_end > start_dirty {
            if !self.throttled {
                ui::ui_line_r(
                    self,
                    row as i32,
                    invalid_row,
                    col_offset + start_dirty,
                    col_offset + end_dirty,
                    col_offset + clear_end,
                    clear_attr,
                    is_wrap_set,
                );
            } else if !self.dirty_col.is_null() {
                // TODO(bfredl): really get rid of the extra pseudo terminal in message.c
                // by using a linebuf_char copy for "throttled message line"
                unsafe {
                    if clear_end > *self.dirty_col.add(row) {
                        *self.dirty_col.add(row) = clear_end;
                    }
                }
            }
        }
    }

    /// assign a handle to the grid
    fn assign_handle(&mut self, handle: VimHandleT) -> Option<VimHandleT> {
        if self.handle == 0 {
            self.handle = handle + 1;
            return Some(self.handle);
        }
        None
    }

    fn line_copy(&mut self, to: usize, from: usize, col: usize, width: usize) {
        let offset_to = self.line_offset[to] + col;
        let offset_from = self.line_offset[from] + col;
        let range = offset_from..(offset_from + width);
        self.chars.copy_within(range.clone(), offset_to);
        self.attrs.copy_within(range.clone(), offset_to);
        self.vcols.copy_within(range.clone(), offset_to);
    }

    /// insert lines on the screen and move the existing lines down
    /// 'line_count' is the number of lines to be inserted.
    /// 'end' is the line after the scrolled part. Normally it is Rows.
    /// 'col' is the column from with we start inserting.
    ///
    /// 'row', 'col' and 'end' are relative to the start of the region.
    fn ins_lines(&mut self, row: i32, line_count: i32, end: i32, col: i32, width: i32) {
        if line_count <= 0 {
            return;
        }

        let mut j;
        let mut temp;
        for i in 0..line_count {
            j = end - 1 - i;
            assert!(j >= 0, "j is less than 0: end={}, i={}", end, i);

            if self.cols != width {
                loop {
                    j -= line_count;
                    if j < row {
                        break;
                    }
                    self.line_copy(
                        (j + line_count) as usize,
                        j as usize,
                        col as usize,
                        width as usize,
                    );
                }
                j += line_count;
                self.clear_line(
                    self.line_offset[j as usize] + col as usize,
                    width as usize,
                    false,
                );
            } else {
                temp = self.line_offset[j as usize];
                loop {
                    j -= line_count;
                    if j < row {
                        break;
                    }
                    self.line_offset[(j + line_count) as usize] = self.line_offset[j as usize];
                }
                self.line_offset[(j + line_count) as usize] = temp;
                self.clear_line(temp, self.cols as usize, false);
            }
        }
        if !self.throttled {
            unsafe {
                ui::ui_call_grid_scroll(
                    self.handle as i64,
                    row as i64,
                    end as i64,
                    col as i64,
                    (col + width) as i64,
                    -line_count as i64,
                    0,
                );
            }
        }
    }

    /// delete lines on the screen and move lines up.
    /// 'end' is the line after the scrolled part. Normally it is Rows.
    /// When scrolling region used 'off' is the offset from the top for the region.
    /// 'row' and 'end' are relative to the start of the region.
    fn del_lines(&mut self, row: i32, line_count: i32, end: i32, col: i32, width: i32) {
        if line_count <= 0 {
            return;
        }

        let mut j;
        let mut temp;
        for i in 0..line_count {
            j = row + i;
            assert!(j >= 0, "j is less than 0: row={}, i={}", row, i);

            if self.cols != width {
                // need to copy part of a line
                loop {
                    j += line_count;
                    if j > end - 1 {
                        break;
                    }
                    self.line_copy(
                        (j - line_count) as usize,
                        j as usize,
                        col as usize,
                        width as usize,
                    );
                }
                j -= line_count;
                self.clear_line(
                    self.line_offset[j as usize] + col as usize,
                    width as usize,
                    false,
                );
            } else {
                // whole width, moving the line pointers is faster
                temp = self.line_offset[j as usize];
                loop {
                    j += line_count;
                    if j > end - 1 {
                        break;
                    }
                    self.line_offset[(j - line_count) as usize] = self.line_offset[j as usize];
                }
                self.line_offset[(j - line_count) as usize] = temp;
                self.clear_line(temp, self.cols as usize, false);
            }
        }
        if !self.throttled {
            unsafe {
                ui::ui_call_grid_scroll(
                    self.handle as i64,
                    row as i64,
                    end as i64,
                    col as i64,
                    (col + width) as i64,
                    line_count as i64,
                    0,
                );
            }
        }
    }
} // impl

#[derive(Debug)]
pub struct LineBuffer {
    chars: Vec<VimSCharT>,
    attrs: Vec<VimSAttrT>,
    vcols: Vec<VimColNrT>,
    scratch: Vec<VimSScratchT>,
    size: usize,
}

impl LineBuffer {
    fn realloc(&mut self, size: usize) {
        self.size = size;
        self.chars = Vec::with_capacity(size);
        self.attrs = Vec::with_capacity(size);
        self.vcols = Vec::with_capacity(size);
        self.scratch = Vec::with_capacity(size);
    }
    fn new(cols: usize) -> Self {
        LineBuffer {
            chars: Vec::with_capacity(cols),
            attrs: Vec::with_capacity(cols),
            vcols: Vec::with_capacity(cols),
            scratch: Vec::with_capacity(cols),
            size: cols,
        }
    }
    /// This function is highly usafe!!!. If you are not carefull you will easily
    /// leak memory or produce UB. It should be called only after the initial
    /// line buffer was allocated.
    unsafe fn from_cline_buffer_ref(clb: &CLineBufferRef) -> Self {
        unsafe {
            let bsz = *clb.size;
            Self {
                chars: Vec::from_raw_parts(*clb.chars, bsz, bsz),
                attrs: Vec::from_raw_parts(*clb.attrs, bsz, bsz),
                vcols: Vec::from_raw_parts(*clb.vcols, bsz, bsz),
                scratch: Vec::from_raw_parts(*clb.scratch, bsz, bsz),
                size: bsz,
            }
        }
    }
    /// This function is highly usafe!!!. If you are not carefull you will easily
    /// leak memory or produce UB. It should be called only after the ownership
    /// of the existing memory was taken by the call to from_cline_buffer.
    unsafe fn into_cline_buffer_ref(self) -> CLineBufferRef {
        let clb = CLineBufferRef::new();
        unsafe {
            *clb.chars = self.chars.leak().as_mut_ptr();
            *clb.attrs = self.attrs.leak().as_mut_ptr();
            *clb.vcols = self.vcols.leak().as_mut_ptr();
            *clb.scratch = self.scratch.leak().as_mut_ptr();
            *clb.size = self.size;
        }
        clb
    }

    fn invalidate_chars_and_attrs(&mut self) {
        self.chars.fill(0xFF);
        self.attrs.fill(0xFF);
    }

    fn fill(
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
    fn mirror_arr<T: Copy, U: Copy>(
        arr: &mut [T],
        scratch: &mut [U],
        first: usize,
        last: usize,
        width: usize,
    ) {
        let scratch = unsafe {
            std::slice::from_raw_parts_mut(scratch.as_mut_ptr() as *mut T, scratch.len())
        };
        let copy = &mut scratch[first..last];
        copy.copy_from_slice(&arr[first..last]);
        let len = last - first;
        arr[width - len..width]
            .iter_mut()
            .rev()
            .zip(copy.iter())
            .for_each(|(dest, src)| *dest = *src);
    }
    fn mirror_schars(&mut self, first: usize, last: usize, width: usize) {
        let scratch = unsafe {
            std::slice::from_raw_parts_mut(
                self.scratch.as_mut_ptr() as *mut VimSCharT,
                self.scratch.len(),
            )
        };
        scratch[first..last].copy_from_slice(&self.chars[first..last]);
        let mirror = width - 1;
        let mut col = first;
        while col < last {
            let rev = mirror - col;
            if col + 1 < last && scratch[col + 1] == 0 {
                self.chars[rev - 1] = scratch[col];
                self.chars[rev] = 0;
                col += 1;
            } else {
                self.chars[rev] = scratch[col];
            }
            col += 1;
        }
    }
    fn mirror(&mut self, first: usize, last: usize, width: usize) {
        assert!(
            first <= last && last <= width && last - first <= width,
            "first={}, last={}, max_col={}",
            first,
            last,
            width
        );
        self.mirror_schars(first, last, width);
        Self::mirror_arr(&mut self.attrs, &mut self.scratch, first, last, width);
        Self::mirror_arr(&mut self.vcols, &mut self.scratch, first, last, width);
    }
}
//                                                                  abcdef           fedcba
#[derive(Debug)]
pub struct CLineBufferRef {
    chars: *mut *mut VimSCharT,
    attrs: *mut *mut VimSAttrT,
    vcols: *mut *mut VimColNrT,
    scratch: *mut *mut VimSScratchT,
    size: *mut libc::size_t,
}

impl CLineBufferRef {
    fn new() -> Self {
        Self {
            chars: &raw mut linebuf_char,
            attrs: &raw mut linebuf_attr,
            vcols: &raw mut linebuf_vcol,
            scratch: &raw mut linebuf_scratch,
            size: &raw mut linebuf_size,
        }
    }
    fn alloc(&self, size: usize) {
        let lb = LineBuffer::new(size);
        unsafe {
            lb.into_cline_buffer_ref();
        }
    }
    fn realloc(&self, size: usize) {
        unsafe {
            let mut lb = LineBuffer::from_cline_buffer_ref(self);
            lb.realloc(size);
            lb.into_cline_buffer_ref();
        }
    }
}

struct LineGrid {
    grid: *mut CScreenGrid,
    row: i32,
    col_offset: i32,
    max_col: i32,
    first: i32,
    last: i32,
    clear_to: i32,
    bg_attr: i32,
    clear_attr: i32,
    flags: i32,
}

static mut LINE_GRID: LineGrid = LineGrid::new();

impl LineGrid {
    const fn new() -> Self {
        LineGrid {
            grid: std::ptr::null_mut(),
            row: -1,
            col_offset: 0,
            max_col: 0,
            first: i32::MAX,
            last: 0,
            clear_to: 0,
            bg_attr: 0,
            clear_attr: 0,
            flags: 0,
        }
    }
    /// Start a group of grid_line_puts calls that builds a single grid line.
    ///
    /// Must be matched with a grid_line_flush call before moving to
    /// another line.
    fn start(&mut self, view: *mut CGridView, mut row: i32, line_buf: &mut LineBuffer) {
        let mut col = 0;
        let grid = grid_adjust(view, &mut row, &mut col);
        self.screengrid_line_start(grid, row, col, line_buf);
    }
    fn screengrid_line_start(
        &mut self,
        grid: *mut CScreenGrid,
        row: i32,
        col: i32,
        line_buf: &mut LineBuffer,
    ) {
        self.max_col = unsafe { (*grid).cols };
        assert!(self.grid.is_null());
        self.grid = grid;
        self.row = row;
        self.col_offset = col;
        self.first = line_buf.size as i32;
        self.max_col = std::cmp::min(self.max_col, unsafe { (*self.grid).cols } - self.col_offset);
        self.last = 0;
        self.clear_to = 0;
        self.bg_attr = 0;
        self.clear_attr = 0;
        self.flags = 0;

        assert!(self.max_col <= line_buf.size as i32);

        unsafe {
            if full_screen && (rdb_flags & RdbFlags::Invalid as u32 != 0) {
                // Current batch must not depend on previous contents of linebuf_char.
                // Set invalid values which will cause assertion failures later if they are used.
                line_buf.invalidate_chars_and_attrs();
            }
        }
    }
    /// Get present char from current rendered screen line
    ///
    /// This indicates what already is on screen, not the pending render buffer.
    ///
    /// @return char or space if out of bounds
    fn get_schar_and_attr(&self, mut col: i32) -> (VimSCharT, VimSAttrT) {
        if col < self.max_col {
            col += self.col_offset;
            let grid: ScreenGrid = unsafe { (&*self.grid).into() };
            let offset = grid.line_offset[self.row as usize] + col as usize;
            let sc = grid.chars[offset];
            let attr = grid.attrs[offset];
            unsafe {
                *self.grid = grid.to_cgrid();
            }
            (sc, attr)
        } else {
            // TODO check what shall we return for attr in this case
            (schar_from_ascii(' '), -1)
        }
    }
    fn put_schar_and_attr(
        &mut self,
        col: i32,
        sc: VimSCharT,
        attr: VimSAttrT,
        lb: &mut LineBuffer,
    ) {
        assert!(!self.grid.is_null());
        if col >= self.max_col {
            return;
        }
        lb.chars[col as usize] = sc;
        lb.attrs[col as usize] = attr;
        self.first = std::cmp::min(self.first, col);
        // TODO(bfredl): Y U NO DOUBLEWIDTH?
        self.last = std::cmp::max(self.last, col + 1);
        lb.vcols[col as usize] = -1;
    }
    /// Put string "text" at "col" position relative to the grid line from the
    /// recent grid_line_start() call.
    ///
    /// @param textlen length of string or -1 to use strlen(text)
    /// Note: only outputs within one row!
    ///
    /// @return number of grid cells used
    fn puts(&mut self, mut col: i32, text: &[u8], attr: VimSAttrT, lb: &mut LineBuffer) -> usize {
        assert!(!self.grid.is_null());
        let start_col = col;
        let mut it = text.iter().enumerate();
        while let Some((i, c)) = it.next() {
            if col >= self.max_col || i >= text.len() || *c == NULL_TERMINATOR {
                break;
            }
            let p = text[i..].as_ptr().cast::<libc::c_char>();
            let max_len = (text.len() - i) as i32;
            let mut mbyte_blen = unsafe { mbyte::utfc_ptr2len_len(p, max_len) };
            if mbyte_blen > max_len {
                mbyte_blen = 1;
            }
            let mut first_c = 0;
            let mut schar = unsafe { mbyte::utfc_ptrlen2schar(p, mbyte_blen, &mut first_c) };
            let mut mbyte_cells = unsafe { mbyte::utf_ptr2cells_len(p, mbyte_blen) };
            if mbyte_cells > 2 || schar == 0 {
                mbyte_cells = 1;
                schar = schar_from_char(0xFFFD);
            }

            if col + mbyte_cells > self.max_col {
                // Only 1 cell left, but character requires 2 cells:
                // display a '>' in the last column to avoid wrapping.
                schar = schar_from_ascii('>');
                mbyte_cells = 1;
            }

            // When at the start of the text and overwriting the right half of a
            // two-cell character in the same grid, truncate that into a '>'.
            if i == 0 && col > self.first && col < self.last && lb.chars[col as usize] == 0 {
                lb.chars[col as usize - 1] = schar_from_ascii('>');
            }

            let ucol = col as usize;
            lb.chars[ucol] = schar;
            lb.attrs[ucol] = attr;
            lb.vcols[ucol] = -1;
            if mbyte_cells == 2 {
                lb.chars[ucol + 1] = 0;
                lb.attrs[ucol + 1] = attr;
                lb.vcols[ucol + 1] = -1;
            }
            col += mbyte_cells;
            for _ in 0..mbyte_blen - 1 {
                it.next();
            }
        }
        if col > start_col {
            self.first = std::cmp::min(self.first, start_col);
            self.last = std::cmp::max(self.last, col);
        }
        (col - start_col) as usize
    }
    fn fill(
        &mut self,
        start_col: i32,
        end_col: i32,
        sc: VimSCharT,
        attr: VimSAttrT,
        line_buf: &mut LineBuffer,
    ) -> i32 {
        let end_col = std::cmp::min(end_col, self.max_col);
        if start_col >= end_col {
            return end_col;
        }
        let start = start_col as usize;
        let end = end_col as usize;
        line_buf.fill(start..end, sc, attr, -1);

        self.first = std::cmp::min(self.first, start_col);
        self.last = std::cmp::max(self.last, end_col);

        end_col
    }
    /// bg_attr     applies to both the buffered line and the columns to clear
    /// clear_attr  applies only to the columns to clear
    fn clear_end(
        &mut self,
        start_col: i32,
        end_col: i32,
        bg_attr: VimSAttrT,
        clear_attr: VimSAttrT,
    ) {
        if self.first > start_col {
            self.first = start_col;
            self.last = start_col;
        }
        self.clear_to = end_col;
        self.bg_attr = bg_attr;
        self.clear_attr = clear_attr;
    }
    /// move the cursor to a position in a currently rendered line.
    fn cursor_goto(&self, col: i32) {
        unsafe {
            ui::ui_grid_cursor_goto((*self.grid).handle, self.row, col);
        }
    }
    fn mirror(&mut self, width: i32, lb: &mut LineBuffer) {
        self.clear_to = std::cmp::max(self.last, self.clear_to);
        if self.first >= self.clear_to {
            return;
        }

        lb.mirror(self.first as usize, self.last as usize, width as usize);

        let first = self.first;
        let last = self.last;

        self.first = width - self.clear_to;
        self.clear_to = width - first;
        self.last = width - last;

        self.flags |= Slf::Rightleft as i32;
    }
    /// End a group of grid_line_puts calls and send the screen buffer to the UI layer.
    fn flush(&mut self, lb: &mut LineBuffer) {
        let grid = self.grid;
        self.grid = std::ptr::null_mut();
        self.clear_to = std::cmp::max(self.last, self.clear_to);
        assert!(self.clear_to <= self.max_col);
        if self.first >= self.clear_to {
            return;
        }
        let mut g: ScreenGrid = unsafe { (&*grid).into() };
        g.put_line_buffer(
            self.row,
            self.col_offset,
            self.first,
            self.last,
            self.clear_to,
            self.bg_attr,
            self.clear_attr,
            -1,
            self.flags,
            lb,
        );
        unsafe {
            *grid = g.to_cgrid();
        }
    }
    /// flush grid line but only if on a valid row
    ///
    /// This is a stopgap until message.c has been refactored to behave
    fn flush_if_valid_row(&mut self, lb: &mut LineBuffer) {
        if self.row < 0 || self.row >= unsafe { (*self.grid).rows } {
            if (unsafe { rdb_flags } & RdbFlags::Invalid as u32) != 0 {
                panic!("rdb_flags & RdbFlags::Invalid != 0");
            } else {
                self.grid = std::ptr::null_mut();
                return;
            }
        }
        self.flush(lb);
    }
}

/// Start a group of grid_line_puts calls that builds a single grid line.
///
/// Must be matched with a grid_line_flush call before moving to
/// another line.
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_start(view: *mut CGridView, row: libc::c_int) {
    unsafe {
        let clb = CLineBufferRef::new();
        let mut lb = LineBuffer::from_cline_buffer_ref(&clb);

        LINE_GRID.start(view, row, &mut lb);

        lb.into_cline_buffer_ref();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn screengrid_line_start(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    col: libc::c_int,
) {
    unsafe {
        let clb = CLineBufferRef::new();
        let mut lb = LineBuffer::from_cline_buffer_ref(&clb);

        LINE_GRID.screengrid_line_start(grid, row, col, &mut lb);

        lb.into_cline_buffer_ref();
    }
}

/// Get present char from current rendered screen line
///
/// This indicates what already is on screen, not the pending render buffer.
///
/// @return char or space if out of bounds
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_getchar(col: libc::c_int, attr: *mut VimSAttrT) -> VimSCharT {
    let (sc, a) = unsafe { LINE_GRID.get_schar_and_attr(col) };
    if !attr.is_null() {
        unsafe {
            *attr = a;
        }
    }
    sc
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_put_schar(col: libc::c_int, schar: VimSCharT, attr: VimSAttrT) {
    let clb = CLineBufferRef::new();
    unsafe {
        let mut lb = LineBuffer::from_cline_buffer_ref(&clb);
        LINE_GRID.put_schar_and_attr(col, schar, attr, &mut lb);
        lb.into_cline_buffer_ref();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_puts(
    col: libc::c_int,
    text: *const libc::c_char,
    text_len: libc::c_int,
    attr: VimSAttrT,
) -> libc::c_int {
    let clb = CLineBufferRef::new();
    let mut lb = unsafe { LineBuffer::from_cline_buffer_ref(&clb) };
    let len = if text_len == -1 {
        unsafe { libc::strlen(text) }
    } else {
        text_len as usize
    };
    let num_cells = unsafe {
        LINE_GRID.puts(
            col,
            std::slice::from_raw_parts(text.cast::<u8>(), len),
            attr,
            &mut lb,
        )
    };
    unsafe {
        lb.into_cline_buffer_ref();
    }
    num_cells as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_fill(
    start_col: libc::c_int,
    end_col: libc::c_int,
    sc: VimSCharT,
    attr: libc::c_int,
) -> libc::c_int {
    unsafe {
        let clb = CLineBufferRef::new();
        let mut lb = LineBuffer::from_cline_buffer_ref(&clb);

        let ret = LINE_GRID.fill(start_col, end_col, sc, attr, &mut lb);

        lb.into_cline_buffer_ref();

        ret
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_clear_end(
    start_col: libc::c_int,
    end_col: libc::c_int,
    bg_attr: VimSAttrT,
    clear_attr: VimSAttrT,
) {
    unsafe {
        LINE_GRID.clear_end(start_col, end_col, bg_attr, clear_attr);
    }
}

/// move the cursor to a position in a currently rendered line.
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_cursor_goto(col: libc::c_int) {
    unsafe {
        LINE_GRID.cursor_goto(col);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_mirror(width: libc::c_int) {
    unsafe {
        let clb = CLineBufferRef::new();
        let mut lb = LineBuffer::from_cline_buffer_ref(&clb);

        LINE_GRID.mirror(width, &mut lb);

        lb.into_cline_buffer_ref();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn linebuf_mirror(
    pfirst: *mut libc::c_int,
    plast: *mut libc::c_int,
    pclear: *mut libc::c_int,
    width: libc::c_int,
) {
    unsafe {
        let clb = CLineBufferRef::new();
        let mut lb = LineBuffer::from_cline_buffer_ref(&clb);
        lb.mirror(*pfirst as usize, *plast as usize, width as usize);
        lb.into_cline_buffer_ref();

        let first = *pfirst;
        let last = *plast;

        *pfirst = width - *pclear;
        *pclear = width - first;
        *plast = width - last;
    }
}

/// End a group of grid_line_puts calls and send the screen buffer to the UI layer.
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_flush() {
    unsafe {
        let clb = CLineBufferRef::new();
        let mut lb = LineBuffer::from_cline_buffer_ref(&clb);

        LINE_GRID.flush(&mut lb);

        lb.into_cline_buffer_ref();
    }
}

/// flush grid line but only if on a valid row
///
/// This is a stopgap until message.c has been refactored to behave
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_flush_if_valid_row() {
    unsafe {
        let clb = CLineBufferRef::new();
        let mut lb = LineBuffer::from_cline_buffer_ref(&clb);

        LINE_GRID.flush_if_valid_row(&mut lb);

        lb.into_cline_buffer_ref();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_put_linebuf(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    col_offset: libc::c_int,
    col: libc::c_int,
    end_col: libc::c_int,
    clear_width: libc::c_int,
    bg_attr: libc::c_int,
    clear_attr: libc::c_int,
    last_vcol: VimColNrT,
    flags: libc::c_int,
) {
    let clb = CLineBufferRef::new();
    let mut lb = unsafe { LineBuffer::from_cline_buffer_ref(&clb) };
    let mut g: ScreenGrid = unsafe { (&*grid).into() };
    g.put_line_buffer(
        row,
        col_offset,
        col,
        end_col,
        clear_width,
        bg_attr,
        clear_attr,
        last_vcol,
        flags,
        &mut lb,
    );
    unsafe {
        *grid = g.to_cgrid();
        lb.into_cline_buffer_ref();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_clear(
    view: *mut CGridView,
    start_row: libc::c_int,
    end_row: libc::c_int,
    start_col: libc::c_int,
    mut end_col: libc::c_int,
    attr: libc::c_int,
) {
    let clb = CLineBufferRef::new();
    let mut lb = unsafe { LineBuffer::from_cline_buffer_ref(&clb) };
    for row in start_row..end_row {
        unsafe {
            LINE_GRID.start(view, row, &mut lb);
            end_col = std::cmp::min(end_col, LINE_GRID.max_col);
            if LINE_GRID.row >= (*LINE_GRID.grid).rows || start_col >= end_col {
                LINE_GRID.grid = std::ptr::null_mut();
                return;
            }
            LINE_GRID.clear_end(start_col, end_col, attr, 0);
            LINE_GRID.flush(&mut lb);
        }
    }
    unsafe {
        lb.into_cline_buffer_ref();
    }
}

/// Converts ascii char to `VimSCharT`
pub fn schar_from_ascii(c: char) -> VimSCharT {
    if cfg!(target_endian = "big") {
        return (c as u32).to_be();
    }
    (c as u32).to_le()
}

/// Takes schar and either inserts it into a glyph_map if it is a "indexed" schar or just converts
/// bytes into schar if its size <= 4. Returns either the index to the bytes in the glyph map or
/// the schar constructed from bytes.
fn schar_from_bytes(buf: &[u8], glyph_map: &mut GlyphMapping) -> VimSCharT {
    assert!(buf.len() < MAX_SCHAR_SIZE);
    if buf.len() <= 4 {
        let mut ret = [0; 4];
        ret[..buf.len()].copy_from_slice(buf);
        if cfg!(target_endian = "big") {
            VimSCharT::from_be_bytes(ret)
        } else {
            VimSCharT::from_le_bytes(ret)
        }
    } else {
        glyph_map.insert(buf)
    }
}

/// Takes schar and either returns bytes from the glyph_map or returns bytes from the schar. The
/// former situation happens when schar is the "indexed" one. Does not set the final
/// NULL_TERMINATOR in the buffer.
fn schar_to_bytes(sc: VimSCharT, glyph_map: &GlyphMapping) -> GlyphBuff {
    if GlyphMapping::is_glyph_mapping_schart(sc) {
        glyph_map
            .get_bytes(sc)
            .cloned()
            .unwrap_or(GlyphBuff::new(0, [0; MAX_SCHAR_SIZE]))
    } else {
        let b = sc.to_le_bytes();
        let pos = b
            .iter()
            .enumerate()
            .find_map(|(pos, el)| if *el == 0 { Some(pos) } else { None })
            .unwrap_or(4);
        let mut ret = [0; MAX_SCHAR_SIZE];
        ret[..pos].copy_from_slice(&b[..pos]);
        GlyphBuff::new(pos as u8, ret)
    }
}

/// Sets NULL_TERMINATOR
fn schar_to_null_term_bytes(sc: VimSCharT, glyph_map: &GlyphMapping) -> GlyphBuff {
    let mut glyph_buff = schar_to_bytes(sc, glyph_map);
    glyph_buff.len += 1;
    assert!((glyph_buff.len as usize) < MAX_SCHAR_SIZE);
    glyph_buff.buff[glyph_buff.len as usize] = NULL_TERMINATOR;
    glyph_buff
}

/// Returns the length of the schar.
fn schar_len_impl(sc: VimSCharT, glyph_map: &GlyphMapping) -> usize {
    if GlyphMapping::is_glyph_mapping_schart(sc) {
        glyph_map.get_len(sc).unwrap_or(0)
    } else {
        sc.to_le_bytes()
            .iter()
            .enumerate()
            .find_map(|(pos, el)| if *el == 0 { Some(pos) } else { None })
            .unwrap_or(4)
    }
}

fn schar_in_arabic_block_impl(sc: VimSCharT, glyph_map: &GlyphMapping) -> bool {
    let first_byte = if GlyphMapping::is_glyph_mapping_schart(sc) {
        glyph_map.get_bytes(sc).map(|gb| gb.buff[0]).unwrap_or(0)
    } else {
        sc.to_le_bytes()[0]
    };
    first_byte & 0xFE == 0xD8
}

#[repr(C)]
#[derive(Debug)]
pub struct CScreenGrid {
    handle: VimHandleT,
    chars: *mut VimSCharT,
    attrs: *mut VimSAttrT,
    vcols: *mut VimColNrT,
    line_offset: *mut libc::size_t,
    dirty_col: *mut libc::c_int,

    rows: libc::c_int,
    cols: libc::c_int,

    valid: bool,

    throttled: bool,

    blending: bool,

    mouse_enabled: bool,

    zindex: libc::c_int,

    comp_row: libc::c_int,
    comp_col: libc::c_int,

    comp_width: libc::c_int,
    comp_height: libc::c_int,

    comp_index: libc::size_t,

    comp_disabled: bool,

    pending_comp_index_update: bool,
}

#[repr(C)]
#[derive(Debug)]
pub struct CGridView {
    target: *mut CScreenGrid,
    row_offset: libc::c_int,
    col_offset: libc::c_int,
}

macro_rules! init_ptr {
    ($p:expr, $cap:expr) => {
        if $p.is_null() {
            Vec::with_capacity($cap)
        } else {
            unsafe { Vec::from_raw_parts($p, $cap, $cap) }
        }
    };
}

impl From<&CScreenGrid> for ScreenGrid {
    fn from(cgrid: &CScreenGrid) -> Self {
        let num_cells = cgrid.rows as usize * cgrid.cols as usize;
        let chars = init_ptr!(cgrid.chars, num_cells);
        let attrs = init_ptr!(cgrid.attrs, num_cells);
        let vcols = init_ptr!(cgrid.vcols, num_cells);
        let line_offset = init_ptr!(cgrid.line_offset, cgrid.rows as usize);
        ScreenGrid {
            handle: cgrid.handle,
            chars,
            attrs,
            vcols,
            line_offset,
            dirty_col: cgrid.dirty_col,
            //dirty_col: unsafe { Vec::from_raw_parts(cgrid.dirty_col, 0, old_num_cells) },
            rows: cgrid.rows,
            cols: cgrid.cols,
            valid: cgrid.valid,
            throttled: cgrid.throttled,
            blending: cgrid.blending,
            mouse_enabled: cgrid.mouse_enabled,
            zindex: cgrid.zindex,
            comp_row: cgrid.comp_row,
            comp_col: cgrid.comp_col,
            comp_width: cgrid.comp_width,
            comp_height: cgrid.comp_height,
            comp_index: cgrid.comp_index,
            comp_disabled: cgrid.comp_disabled,
            pending_comp_index_update: cgrid.pending_comp_index_update,
        }
    }
}

/// Glyph buffer to store bytes for the cell value of the `ScreenGrid`.
#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord)]
struct GlyphBuff {
    len: u8,
    buff: [u8; MAX_SCHAR_SIZE],
}

impl GlyphBuff {
    /// Constructs a new empty `GlyphBuff`
    fn new(len: u8, buff: [u8; MAX_SCHAR_SIZE]) -> Self {
        Self { len, buff }
    }
}

/// Used as a dictionary for glyphs which do not fit a sizeof(VimSCharT)
/// length UTF-8 string. For example this includes so called zalgo text or
/// some other characters that are larger than the standard unicode 4 bytes.
///
/// The maximum byte size of a glyph is MAX_SCHAR_SIZE (including the final NUL).
///
// original vim comment
// Used to cache glyphs which doesn't fit an a sizeof(schar_T) length UTF-8 string.
// Then it instead stores an index into glyph_cache.keys[] which is a flat char array.
// The hash part is used by schar_from_buf() to quickly lookup glyphs which already
// has been interned. schar_get() should used to convert a schar_T value
// back to a string buffer.
//
// The maximum byte size of a glyph is MAX_SCHAR_SIZE (including the final NUL).
struct GlyphMapping {
    /// Stores mapping of `VimSCharT` to the bytes. The key type is composed
    /// of (in machine endianness) 4 bytes where the first byte is 0xFF and the
    /// subsequent 3 bytes are used as a key in glyph cache.
    /// This is possible as an escape char 0xFF is an invalid UTF-8.
    dict: BTreeMap<VimSCharT, Rc<GlyphBuff>>,
    /// The map is used to quickly search the key for insertion into the
    /// glyph cache.
    index: BTreeMap<Rc<GlyphBuff>, VimSCharT>,
    /// Is used to determine the next mapping when a new glyph is inserted.
    /// It is a counter that is incremented by an insertion into the dict.
    next_free: VimSCharT,
}

impl GlyphMapping {
    /// Constructs a new empty `GlyphMapping`
    const fn new() -> Self {
        Self {
            dict: BTreeMap::new(),
            index: BTreeMap::new(),
            next_free: Self::to_schart(0),
        }
    }
    /// Converts 3 bytes index into the 4 bytes schart value
    const fn to_schart(idx: VimSCharT) -> VimSCharT {
        if cfg!(target_endian = "big") {
            idx + (0xFF << 24)
        } else {
            0xFF + (idx << 8)
        }
    }
    /// Increments a 3 byte index stored in `next_free` and return a 4 byte
    /// schart with the 0xFF byte set.
    const fn get_next_free(&self) -> VimSCharT {
        let mut idx = Self::extract_index(self.next_free);
        idx += 1;
        Self::to_schart(idx)
    }
    /// Extracts a 3 bytes index from the schart which is stored as data[1..3(including)]
    const fn extract_index(val: VimSCharT) -> VimSCharT {
        if cfg!(target_endian = "big") {
            val & 0x00FFFFFF
        } else {
            val >> 8
        }
    }
    /// Determines if the given schart is used as a mapping into glyph cache
    /// or it is a normal UTF-8 character.
    fn is_glyph_mapping_schart(c: VimSCharT) -> bool {
        c.to_le_bytes()[0] == 0xFF
    }
    /// Inserts into the cache. Returns the mapping of `VimSCharT` to bytes,
    /// that are used in the grid to draw an actual content of the cell.
    fn insert(&mut self, val: &[u8]) -> VimSCharT {
        assert!(val.len() < MAX_SCHAR_SIZE);

        let mut arr = [0; MAX_SCHAR_SIZE];
        arr[..val.len()].copy_from_slice(val);
        let k: Rc<GlyphBuff> = Rc::new(GlyphBuff::new(val.len() as u8, arr));
        if let Some(idx) = self.index.get(&k) {
            return *idx;
        }

        let new_mapping = self.next_free;
        assert!(new_mapping < 0xFFFFFFFF);
        self.dict.insert(new_mapping, k.clone());
        self.index.insert(k, new_mapping);
        self.next_free = self.get_next_free();
        new_mapping
    }
    /// Returns the bytes from the mapping schart -> bytes
    fn get_bytes(&self, c: VimSCharT) -> Option<&GlyphBuff> {
        self.dict.get(&c).map(|glyph_buff| glyph_buff.borrow())
    }
    /// Returns the length of the bytes from the mapping schart -> bytes
    fn get_len(&self, c: VimSCharT) -> Option<usize> {
        self.dict.get(&c).map(|glyph_buff| glyph_buff.len as usize)
    }
    /// Clears mapping if the mapping is full. Empties `dict`, `index` and
    /// resets the `next_free`
    fn clear_if_full(&mut self) {
        if self.is_full() {
            return;
        }
        self.clear();
    }
    /// Checks if the mapping is full. Technically the mapping is full when
    /// we stored 2**24 - 1 values. However we check for 2**21
    fn is_full(&self) -> bool {
        Self::extract_index(self.next_free) >= (1 << 21)
    }
    /// Clears the contents of the mapping
    fn clear(&mut self) {
        self.dict.clear();
        self.index.clear();
        self.next_free = Self::to_schart(0);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn alloc_initial_line_buffer() {
    let _ = CLineBufferRef::new().alloc(unsafe { globals::Columns as usize });
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_alloc(
    grid: *mut CScreenGrid,
    rows: libc::c_int,
    cols: libc::c_int,
    copy: bool,
    valid: bool,
) {
    unsafe {
        let clb = CLineBufferRef::new();
        let mut lb = LineBuffer::from_cline_buffer_ref(&clb);
        let g = ScreenGrid::from_cgrid(&*grid, rows, cols, valid, copy, &mut lb);
        *grid = g.to_cgrid();
        lb.into_cline_buffer_ref();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_free(grid: *mut CScreenGrid) {
    unsafe {
        let _: ScreenGrid = (&*grid).into();
        (*grid).chars = std::ptr::null_mut();
        (*grid).attrs = std::ptr::null_mut();
        (*grid).vcols = std::ptr::null_mut();
        (*grid).line_offset = std::ptr::null_mut();
    }
}

/// clear a line in the grid starting at `offset` until `width` characters
/// are cleared. `valid` is used to fill attributes. See `ScreenGrid::attrs_fill`
#[unsafe(no_mangle)]
pub extern "C" fn grid_clear_line(
    grid: *mut CScreenGrid,
    offset: libc::size_t,
    width: libc::c_int,
    valid: bool,
) {
    unsafe {
        let mut g: ScreenGrid = (&*grid).into();
        g.clear_line(offset, width as usize, valid);
        *grid = g.to_cgrid();
    }
}

/// @param buf need not be NUL terminated, but may not contain embedded NULs.
///
/// caller must ensure len < MAX_SCHAR_SIZE (not =, as NUL needs a byte)
#[unsafe(no_mangle)]
pub extern "C" fn schar_from_buf(buf: *const libc::c_char, len: libc::size_t) -> VimSCharT {
    assert!(len < MAX_SCHAR_SIZE);
    let buf = buf as *const u8;
    let s = if buf.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(buf, len) }
    };
    unsafe {
        let p = &raw mut GLYPH_MAPPING;
        schar_from_bytes(s, &mut *p)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_from_str(buf: *const libc::c_char) -> VimSCharT {
    // note: critical max is really (1<<24)-1. This gives us some marginal
    // until next time update_screen() is called
    if buf.is_null() {
        return 0;
    }
    let len = unsafe { libc::strlen(buf) };
    schar_from_buf(buf, len)
}

/// Check if cache is full, and if it is, clear it.
///
/// This should normally only be called in update_screen()
///
/// @return true if cache was clered, and all your screen buffers now are hosed
/// and you need to use UPD_CLEAR
#[unsafe(no_mangle)]
pub extern "C" fn schar_cache_clear_if_full() -> bool {
    unsafe {
        let gm = &mut *&raw mut GLYPH_MAPPING;
        if gm.is_full() {
            schar_cache_clear();
            return true;
        }
        false
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_cache_clear() {
    unsafe {
        decor_check_invalid_glyphs();
        let gm = &mut *&raw mut GLYPH_MAPPING;
        gm.clear();
        if check_chars_options() {
            panic!("Erroc checking chars options");
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_high(sc: VimSCharT) -> bool {
    GlyphMapping::is_glyph_mapping_schart(sc)
}

/// advance buf_out. do NOT set final NUL
#[unsafe(no_mangle)]
pub extern "C" fn schar_get_adv(buf_out: *mut *mut libc::c_char, sc: VimSCharT) -> libc::size_t {
    let gm = unsafe { &mut *&raw mut GLYPH_MAPPING };
    let gb = schar_to_bytes(sc, gm);
    let len = gb.len as usize;
    let slice = unsafe { std::slice::from_raw_parts_mut(*buf_out as *mut u8, len) };
    slice.copy_from_slice(&gb.buff[..len]);
    unsafe {
        *buf_out = (*buf_out).add(len);
    }
    len
}

/// sets final NUL
#[unsafe(no_mangle)]
pub extern "C" fn schar_get(mut buf_out: *mut libc::c_char, sc: VimSCharT) -> libc::size_t {
    unsafe {
        let len = schar_get_adv(&raw mut buf_out, sc);
        *buf_out = NULL_TERMINATOR as libc::c_char;
        len
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_len(sc: VimSCharT) -> libc::size_t {
    let gm = unsafe { &mut *&raw mut GLYPH_MAPPING };
    schar_len_impl(sc, &gm)
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_in_arabic_block(sc: VimSCharT) -> bool {
    let gm = unsafe { &mut *&raw mut GLYPH_MAPPING };
    schar_in_arabic_block_impl(sc, gm)
}

/// @return ascii char or NULL_TERMINATOR if not ascii
#[unsafe(no_mangle)]
pub extern "C" fn schar_get_ascii(sc: VimSCharT) -> libc::c_char {
    let c = if cfg!(target_endian = "big") {
        if sc & 0x80FFFFFF == 0 {
            sc.to_be_bytes()[0]
        } else {
            NULL_TERMINATOR
        }
    } else {
        if sc < 0x80 { sc as u8 } else { NULL_TERMINATOR }
    };
    c as libc::c_char
}

/// Determine if dedicated window grid should be used or the default_grid
///
/// If UI did not request multigrid support, draw all windows on the
/// default_grid.
///
/// NB: this function can only been used with window grids in a context where
/// win_grid_alloc already has been called!
///
/// If the default_grid is used, adjust window relative positions to global
/// screen positions.
#[unsafe(no_mangle)]
pub extern "C" fn grid_adjust(
    grid: *const CGridView,
    row_offset: *mut libc::c_int,
    col_offset: *mut libc::c_int,
) -> *mut CScreenGrid {
    unsafe {
        assert!(!grid.is_null(), "view grid is nullptr");

        *row_offset += (*grid).row_offset;
        *col_offset += (*grid).col_offset;

        return (*grid).target;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_invalidate(grid: *mut CScreenGrid) {
    unsafe {
        let mut g: ScreenGrid = (&*grid).into();
        g.invalidate();
        *grid = g.to_cgrid();
    }
}

/// Get a single character directly from grid.chars
///
/// @param[out] attrp  set to the character's attribute (optional)
#[unsafe(no_mangle)]
pub extern "C" fn grid_getchar(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    col: libc::c_int,
    attrp: *mut libc::c_int,
) -> VimSCharT {
    let g: ScreenGrid = unsafe { (&*grid).into() };
    let (sc, at) = g.get_char_and_attr(row as usize, col as usize);
    unsafe {
        *grid = g.to_cgrid();
    }
    if !attrp.is_null() {
        unsafe {
            *attrp = at;
        }
    }
    sc
}

/// Put a unicode character in a screen cell.
#[unsafe(no_mangle)]
pub extern "C" fn schar_from_char(mut c: libc::c_int) -> VimSCharT {
    if c >= 0x200000 {
        // TODO(bfredl): this must NEVER happen, even if the file contained overlong sequences
        c = 0xFFFD;
    }
    let mut bytes = (0 as VimSCharT).to_le_bytes();
    unsafe {
        mbyte::utf_char2bytes(c, bytes.as_mut_ptr().cast::<libc::c_char>());
    }
    VimSCharT::from_le_bytes(bytes)
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_assign_handle(grid: *mut CScreenGrid) {
    let mut g: ScreenGrid = unsafe { (&*grid).into() };

    unsafe {
        if let Some(lh) = g.assign_handle(last_grid_handle) {
            last_grid_handle = lh;
        }
    }

    unsafe {
        *grid = g.to_cgrid();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_ins_lines(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    line_count: libc::c_int,
    end: libc::c_int,
    col: libc::c_int,
    width: libc::c_int,
) {
    let mut g: ScreenGrid = unsafe { (&*grid).into() };

    g.ins_lines(row, line_count, end, col, width);

    unsafe {
        *grid = g.to_cgrid();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_del_lines(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    line_count: libc::c_int,
    end: libc::c_int,
    col: libc::c_int,
    width: libc::c_int,
) {
    let mut g: ScreenGrid = unsafe { (&*grid).into() };

    g.del_lines(row, line_count, end, col, width);

    unsafe {
        *grid = g.to_cgrid();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glyph_mapping_extract_index() {
        let val = 0x0A0B0C0D;
        if cfg!(target_endian = "big") {
            assert_eq!(GlyphMapping::extract_index(val), 0x000B0C0D);
        } else {
            assert_eq!(GlyphMapping::extract_index(val), 0x000A0B0C);
        }
    }
    #[test]
    fn glyph_mapping_to_schart() {
        let val = 0xAE;
        if cfg!(target_endian = "big") {
            assert_eq!(GlyphMapping::to_schart(val), 0xFF0000AE);
        } else {
            assert_eq!(GlyphMapping::to_schart(val), 0xAEFF);
        }
    }
    #[test]
    fn glyph_mapping_get_next_free() {
        let gc = GlyphMapping::new();
        if cfg!(target_endian = "big") {
            assert_eq!(gc.get_next_free(), 0xFF000001);
        } else {
            assert_eq!(gc.get_next_free(), 0x01FF);
        }
    }
    #[test]
    fn glyph_mapping_insert() {
        let mut gm = GlyphMapping::new();
        let val: usize = 0xCFA2ADA4F0;
        if cfg!(target_endian = "big") {
            let bytes = val.to_be_bytes();
            let mapping = gm.insert(&bytes);
            assert!(gm.dict.contains_key(&mapping));
            assert_eq!(gm.get_next_free(), 0xFF000002);
            let mapping1 = gm.insert(&bytes);
            assert_eq!(mapping, mapping1);
            assert_eq!(gm.dict.len(), 1);
            assert_eq!(gm.index.len(), 1);
        } else {
            let bytes = val.to_le_bytes();
            let mapping = gm.insert(&bytes);
            assert!(gm.dict.contains_key(&mapping));
            assert_eq!(gm.get_next_free(), 0x02FF);
            let mapping1 = gm.insert(&bytes);
            assert_eq!(mapping, mapping1);
            assert_eq!(gm.dict.len(), 1);
            assert_eq!(gm.index.len(), 1);
        }
    }
    #[test]
    fn glyph_mapping_is_full() {
        let mut gm = GlyphMapping::new();
        assert_eq!(gm.is_full(), false);
        gm.next_free = VimSCharT::from_le_bytes([0xFF, 0x00, 0x00, 0x20]);
        assert_eq!(gm.is_full(), true);
    }
    #[test]
    fn schar_from_bytes() {
        let mut gm = GlyphMapping::new();
        let test_cases = [
            (vec![0x41], [0x41, 0, 0, 0]),
            (
                vec![0x73, 0xcc, 0xb8, 0xcc, 0x89, 0xcc, 0xbe],
                [0xFF, 0, 0, 0],
            ),
            (
                vec![0x73, 0xcc, 0xb8, 0xcc, 0x89, 0xcc, 0xbe],
                [0xFF, 0, 0, 0],
            ),
            (
                vec![0x73, 0xcc, 0xb8, 0xcc, 0x89, 0xcc, 0xbf],
                [0xFF, 1, 0, 0],
            ),
        ];
        for (t, e) in &test_cases {
            assert_eq!(super::schar_from_bytes(t, &mut gm), u32::from_le_bytes(*e));
        }
    }
    #[test]
    fn schar_to_bytes() {
        let mut gm = GlyphMapping::new();
        gm.insert(&[0x73, 0xcc, 0xb8, 0xcc, 0x89, 0xcc, 0xbf]);
        let set_bytes = |buf: &[u8]| -> [u8; MAX_SCHAR_SIZE] {
            let mut ret = [0; MAX_SCHAR_SIZE];
            ret[..buf.len()].copy_from_slice(buf);
            ret
        };
        let test_cases = [
            ([0x41, 0, 0, 0], GlyphBuff::new(1, set_bytes(&[0x41]))),
            ([0xFF, 1, 0, 0], GlyphBuff::new(0, set_bytes(&[]))),
            (
                [0xFF, 0, 0, 0],
                GlyphBuff::new(7, set_bytes(&[0x73, 0xcc, 0xb8, 0xcc, 0x89, 0xcc, 0xbf])),
            ),
        ];
        for (t, e) in &test_cases {
            assert_eq!(super::schar_to_bytes(u32::from_le_bytes(*t), &mut gm), *e);
        }
    }
    #[test]
    fn schar_len() {
        let mut gm = GlyphMapping::new();
        gm.insert(&[0x73, 0xcc, 0xb8, 0xcc, 0x89, 0xcc, 0xbf]);
        let test_cases = [
            ([0x41, 0, 0, 0], 1),
            ([0x41, 0x51, 0x61, 0x71], 4),
            ([0xFF, 1, 0, 0], 0),
            ([0xFF, 0, 0, 0], 7),
        ];
        for (t, e) in &test_cases {
            assert_eq!(super::schar_len_impl(u32::from_le_bytes(*t), &mut gm), *e);
        }
    }
    #[link(name = "nvim")]
    unsafe extern "C" {
        fn linebuf_mirror(
            firstp: *mut libc::c_int,
            lastp: *mut libc::c_int,
            clearp: *mut libc::c_int,
            maxcol: libc::c_int,
        );
        fn utf_ptr2cells(p_in: *const libc::c_char) -> libc::c_int;
    }
    #[test]
    fn line_buffer_mirror() {
        let mut lb = LineBuffer::new(8);
        lb.chars = vec![1, 0, 2, 3, 0, 4, 5, 0];
        lb.attrs = vec![1, 2, 3, 4, 5, 6, 7, 8];
        lb.vcols = vec![0; 8];
        lb.scratch = vec![0; 8];
        println!("chars: {:?}", lb.chars);
        println!("attrs: {:?}", lb.attrs);

        let first = 0;
        let last = 8;
        let max_col = 8;

        lb.mirror(first, last, max_col);

        println!("chars: {:?}", lb.chars);
        println!("attrs: {:?}", lb.attrs);
    }
    #[test]
    fn test2() {
        unsafe {
            alloc_initial_line_buffer();
            let clb = CLineBufferRef::new();
            let mut lb = LineBuffer::from_cline_buffer_ref(&clb);
            lb.realloc(8);
            lb.chars = vec![1, 0, 2, 3, 0, 4, 5, 0];
            lb.attrs = vec![1, 2, 3, 4, 5, 6, 7, 8];
            println!("chars: {:?}", lb.chars);
            println!("attrs: {:?}", lb.attrs);
            lb.into_cline_buffer_ref();

            let mut first = 0;
            let mut last = 8;
            let mut clear = 0;
            let max_col = 8;

            linebuf_mirror(&raw mut first, &raw mut last, &raw mut clear, max_col);

            let clb = CLineBufferRef::new();
            let lb = LineBuffer::from_cline_buffer_ref(&clb);
            println!("chars: {:?}", lb.chars);
            println!("attrs: {:?}", lb.attrs);
            lb.into_cline_buffer_ref();
        }
    }
}
