// vim: sw=4
type VimSCharT = u32;
type VimSAttrT = i32;
type VimColNrT = i32;
type VimSScratchT = i32;
type VimHandleT = libc::c_int;
type VimDirtyColT = libc::c_int;

unsafe extern "C" {
    static mut Rows: libc::c_int;
    static mut Columns: libc::c_int;
    static mut default_grid: CScreenGrid;

    static mut msg_grid: CScreenGrid;
    static mut msg_grid_adj: CScreenGrid;

    static mut linebuf_char: *mut VimSCharT;
    static mut linebuf_attr: *mut VimSAttrT;
    static mut linebuf_vcol: *mut VimColNrT;
    static mut linebuf_scratch: *mut VimSScratchT;
    static mut linebuf_size: libc::size_t;
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
    handle: VimHandleT,

    chars: Vec<VimSCharT>,
    attrs: Vec<VimSAttrT>,
    vcols: Vec<VimColNrT>,
    line_offset: Vec<usize>,

    /// last column that was drawn (not cleared with the default background).
    /// only used when "throttled" is set. Not allocated by grid_alloc!
    //dirty_col: Vec<VimDirtyColT>,
    dirty_col: *mut VimDirtyColT,

    /// the size of the allocated grid.
    rows: i32,
    cols: i32,

    /// The state of the grid is valid. Otherwise it needs to be redrawn.
    valid: bool,

    /// only draw internally and don't send updates yet to the compositor or
    /// external UI.
    throttled: bool,

    row_offset: i32,
    col_offset: i32,
    target: *mut CScreenGrid,
    //view: Option<GridView>,
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
            row_offset: 0,
            col_offset: 0,
            target: std::ptr::null_mut(),
            //view: None,
            blending: false,
            mouse_enabled: true,
            zindex: 0,
            comp_row: 0,
            comp_col: 0,
            comp_width: 0,
            comp_height: 0,
            comp_index: 0,
            comp_disabled: false,
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
                new_grid.copy_grid_content_from_old_grid(&grid, i);
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

    unsafe fn to_cgrid(self) -> CScreenGrid {
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
            valid: self.valid as libc::boolean_t,
            throttled: self.throttled as libc::boolean_t,
            row_offset: self.row_offset,
            col_offset: self.col_offset,
            target: self.target,
            blending: self.blending as libc::boolean_t,
            mouse_enabled: self.mouse_enabled as libc::boolean_t,
            zindex: self.zindex,
            comp_row: self.comp_row,
            comp_col: self.comp_col,
            comp_width: self.comp_width,
            comp_height: self.comp_height,
            comp_index: self.comp_index,
            comp_disabled: self.comp_disabled as libc::boolean_t,
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
        let old_grid: ScreenGrid = (&*grid).into();

        Self::from_grid(&old_grid, rows, cols, valid, copy, line_buffer)
    }

    pub fn clear_line(&mut self, offset: usize, width: usize, valid: bool) {
        let range = offset..(offset + width);
        self.chars[range.clone()].fill(schar_from_ascii(' '));
        self.attrs[range.clone()].fill(Self::attrs_fill(valid));
        self.vcols[range.clone()].fill(-1);
    }

    fn attrs_fill(valid: bool) -> VimSAttrT {
        if valid {
            0
        } else {
            -1
        }
    }
}

fn cbool(x: libc::boolean_t) -> bool {
    if x != 0 {
        true
    } else {
        false
    }
}

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
    unsafe fn to_cline_buffer_ref(self) -> CLineBufferRef {
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
}

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
            lb.to_cline_buffer_ref();
        }
    }
    fn realloc(&self, size: usize) {
        unsafe {
            let mut lb = LineBuffer::from_cline_buffer_ref(self);
            lb.realloc(size);
            lb.to_cline_buffer_ref();
        }
    }
}

#[derive(Debug, Clone)]
pub struct GridView {
    /// TODO(bfredl): maybe physical grids and "views" (i e drawing
    /// specifications) should be two separate types?
    /// offsets for the grid relative to another grid. Used for grids
    /// that are views into another, actually allocated grid 'target'
    row_offset: i32,
    col_offset: i32,
    target: *const ScreenGrid,
}

pub fn schar_from_ascii(c: char) -> VimSCharT {
    if cfg!(target_endian = "big") {
        return (c as u32).to_be();
    }
    (c as u32).to_le()
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

    valid: libc::boolean_t,

    throttled: libc::boolean_t,

    row_offset: libc::c_int,
    col_offset: libc::c_int,
    target: *mut CScreenGrid,

    blending: libc::boolean_t,

    mouse_enabled: libc::boolean_t,

    zindex: libc::c_int,

    comp_row: libc::c_int,
    comp_col: libc::c_int,

    comp_width: libc::c_int,
    comp_height: libc::c_int,

    comp_index: libc::size_t,

    comp_disabled: libc::boolean_t,
}

impl From<&CScreenGrid> for ScreenGrid {
    fn from(cgrid: &CScreenGrid) -> Self {
        let num_cells = cgrid.rows as usize * cgrid.cols as usize;
        ScreenGrid {
            handle: cgrid.handle,
            chars: unsafe { Vec::from_raw_parts(cgrid.chars, num_cells, num_cells) },
            attrs: unsafe { Vec::from_raw_parts(cgrid.attrs, num_cells, num_cells) },
            vcols: unsafe { Vec::from_raw_parts(cgrid.vcols, num_cells, num_cells) },
            line_offset: unsafe {
                Vec::from_raw_parts(cgrid.line_offset, cgrid.rows as usize, cgrid.rows as usize)
            },
            dirty_col: cgrid.dirty_col,
            //dirty_col: unsafe { Vec::from_raw_parts(cgrid.dirty_col, 0, old_num_cells) },
            rows: cgrid.rows,
            cols: cgrid.cols,
            valid: cbool(cgrid.valid),
            throttled: cbool(cgrid.throttled),
            row_offset: cgrid.row_offset,
            col_offset: cgrid.col_offset,
            target: cgrid.target,
            //view: todo!(),
            blending: cbool(cgrid.blending),
            mouse_enabled: cbool(cgrid.mouse_enabled),
            zindex: cgrid.zindex,
            comp_row: cgrid.comp_row,
            comp_col: cgrid.comp_col,
            comp_width: cgrid.comp_width,
            comp_height: cgrid.comp_height,
            comp_index: cgrid.comp_index,
            comp_disabled: cbool(cgrid.comp_disabled),
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn alloc_initial_line_buffer() {
    let _ = CLineBufferRef::new().alloc(unsafe { Columns as usize });
}

#[unsafe(no_mangle)]
pub extern "C" fn init_default_grid() {
    unsafe {
        default_grid.chars = Vec::new().leak().as_mut_ptr();
        default_grid.attrs = Vec::new().leak().as_mut_ptr();
        default_grid.vcols = Vec::new().leak().as_mut_ptr();
        default_grid.line_offset = Vec::new().leak().as_mut_ptr();

        msg_grid.chars = Vec::new().leak().as_mut_ptr();
        msg_grid.attrs = Vec::new().leak().as_mut_ptr();
        msg_grid.vcols = Vec::new().leak().as_mut_ptr();
        msg_grid.line_offset = Vec::new().leak().as_mut_ptr();

        msg_grid_adj.chars = Vec::new().leak().as_mut_ptr();
        msg_grid_adj.attrs = Vec::new().leak().as_mut_ptr();
        msg_grid_adj.vcols = Vec::new().leak().as_mut_ptr();
        msg_grid_adj.line_offset = Vec::new().leak().as_mut_ptr();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_alloc(
    grid: *mut CScreenGrid,
    rows: libc::c_int,
    cols: libc::c_int,
    copy: libc::boolean_t,
    valid: libc::boolean_t,
) {
    unsafe {
        let clb = CLineBufferRef::new();
        let mut lb = LineBuffer::from_cline_buffer_ref(&clb);
        let g = ScreenGrid::from_cgrid(&*grid, rows, cols, cbool(valid), cbool(copy), &mut lb);
        *grid = g.to_cgrid();
        lb.to_cline_buffer_ref();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_free(grid: *mut CScreenGrid) {
    unsafe {
        let _: ScreenGrid = (&*grid).into();
    }
}

/// clear a line in the grid starting at `offset` until `width` characters
/// are cleared. `valid` is used to fill attributes. See `ScreenGrid::attrs_fill`
#[unsafe(no_mangle)]
pub extern "C" fn grid_clear_line(
    grid: *mut CScreenGrid,
    offset: libc::size_t,
    width: libc::c_int,
    valid: libc::boolean_t,
) {
    unsafe {
        let mut g: ScreenGrid = (&*grid).into();
        g.clear_line(offset, width as usize, cbool(valid));
        *grid = g.to_cgrid();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {}
}
