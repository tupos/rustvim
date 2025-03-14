// vim: sw=4
use std::{borrow::Borrow, collections::BTreeMap, rc::Rc};

/// if data[0] is 0xFF, then data[1..4] is a 24-bit index (in machine endianness)
/// otherwise it must be a UTF-8 string of length maximum 4 (no NUL when n=4)
type VimSCharT = u32;
type VimSAttrT = i32;
type VimColNrT = i32;

/// must be at least as big as the biggest of schar_T, sattr_T, colnr_T
type VimSScratchT = i32;
type VimHandleT = libc::c_int;
type VimDirtyColT = libc::c_int;

/// Includes final NUL. MAX_MCO is no longer used, but at least 4*(MAX_MCO+1)+1=29
/// ensures we can fit all composed chars which did fit before.
static MAX_SCHAR_SIZE: usize = 32;

static NULL_TERMINATOR: u8 = '\0' as u8;

static mut GLYPH_MAPPING: GlyphMapping = GlyphMapping::new();

unsafe extern "C" {
    static mut Rows: libc::c_int;
    static mut Columns: libc::c_int;

    static mut default_grid: CScreenGrid;
    static mut msg_grid: CScreenGrid;
    static mut msg_grid_adj: CScreenGrid;
    static mut pum_grid: CScreenGrid;

    static mut linebuf_char: *mut VimSCharT;
    static mut linebuf_attr: *mut VimSAttrT;
    static mut linebuf_vcol: *mut VimColNrT;
    static mut linebuf_scratch: *mut VimSScratchT;
    static mut linebuf_size: libc::size_t;

    fn decor_check_invalid_glyphs();
    fn check_chars_options() -> bool;
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
            valid: self.valid,
            throttled: self.throttled,
            row_offset: self.row_offset,
            col_offset: self.col_offset,
            target: self.target,
            blending: self.blending,
            mouse_enabled: self.mouse_enabled,
            zindex: self.zindex,
            comp_row: self.comp_row,
            comp_col: self.comp_col,
            comp_width: self.comp_width,
            comp_height: self.comp_height,
            comp_index: self.comp_index,
            comp_disabled: self.comp_disabled,
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

    fn invalidate(&mut self) {
        self.attrs.fill(-1);
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
            .map(|gb| gb.clone())
            .unwrap_or(GlyphBuff::new(0, [0; MAX_SCHAR_SIZE]))
    } else {
        let b = sc.to_le_bytes();
        let pos = b
            .iter()
            .enumerate()
            .find_map(|(pos, el)| if *el == 0 { Some(pos) } else { None })
            .unwrap_or(4) as usize;
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
            .unwrap_or(4) as usize
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

    row_offset: libc::c_int,
    col_offset: libc::c_int,
    target: *mut CScreenGrid,

    blending: bool,

    mouse_enabled: bool,

    zindex: libc::c_int,

    comp_row: libc::c_int,
    comp_col: libc::c_int,

    comp_width: libc::c_int,
    comp_height: libc::c_int,

    comp_index: libc::size_t,

    comp_disabled: bool,
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
            row_offset: cgrid.row_offset,
            col_offset: cgrid.col_offset,
            target: cgrid.target,
            //view: todo!(),
            blending: cgrid.blending,
            mouse_enabled: cgrid.mouse_enabled,
            zindex: cgrid.zindex,
            comp_row: cgrid.comp_row,
            comp_col: cgrid.comp_col,
            comp_width: cgrid.comp_width,
            comp_height: cgrid.comp_height,
            comp_index: cgrid.comp_index,
            comp_disabled: cgrid.comp_disabled,
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
    let _ = CLineBufferRef::new().alloc(unsafe { Columns as usize });
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
        lb.to_cline_buffer_ref();
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
        if sc < 0x80 {
            sc as u8
        } else {
            NULL_TERMINATOR
        }
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
}
