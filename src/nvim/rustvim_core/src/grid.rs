// vim: sw=4
mod cell;
mod coordinates;
mod fold_column;
mod line_buffer;
mod line_submission;
mod row;
mod store;
mod tabstops;
mod virtual_text;

use crate::{
    defines::OptInt,
    generated::enums::{RdbFlags, Slf},
    globals, mbyte,
    ui::{
        self, GridOutputTarget,
        output::{OwnedRawLineCommand, UiCommand},
    },
};
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    ffi::CStr,
    ops::Range,
    sync::{
        LazyLock, Mutex, MutexGuard,
        atomic::{AtomicI32, Ordering},
    },
};

pub(crate) use cell::GridCell;
pub(crate) use coordinates::{
    ComposedGridSpace, ComposedPosition, ComposedRegion, GridDelta, GridPosition, GridRegion,
    GridSize, LocalGridSpace, Position, Region,
};
use fold_column::{FoldColumn, FoldColumnInfo, FoldColumnSymbols};
pub use line_buffer::LineBuffer;
use line_buffer::{LineTextRenderOptions, LineTextVirtualColumns};
use line_submission::{LineSubmission, SmoothScrollMarker};
pub(crate) use row::{GridRow, GridRowSegment};
use store::{GridStoreResult, StoredGridGuard, lock_grid_store};
use tabstops::TabStops;
use virtual_text::{VirtualText, VirtualTextChunk, VirtualTextRenderOptions};

/// if data[0] is 0xFF, then data[1..4] is a 24-bit index (in machine endianness)
/// otherwise it must be a UTF-8 string of length maximum 4 (no NUL when n=4)
pub type VimSCharT = u32;
pub type VimSAttrT = i32;
pub type VimColNrT = i32;

pub type VimHandleT = libc::c_int;
pub type VimDirtyColT = libc::c_int;

/// Stable identity of one screen grid within Rust implementation code.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct GridId(i32);

impl GridId {
    /// Creates an identity from a validated native grid handle.
    pub(crate) const fn new(handle: i32) -> Self {
        Self(handle)
    }

    /// Returns the native handle at the final UI boundary.
    pub(crate) const fn as_native_handle(self) -> VimHandleT {
        self.0
    }
}

impl std::fmt::Display for GridId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Per-row redraw extent used while grid output is throttled.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DirtyColumns {
    columns: Box<[VimDirtyColT]>,
}

impl DirtyColumns {
    /// Creates clean dirty-column state for `rows` rows.
    fn new(rows: usize) -> Self {
        Self {
            columns: vec![0; rows].into_boxed_slice(),
        }
    }

    /// Resizes the state while preserving values for rows that still exist.
    fn resize(&mut self, rows: usize) {
        let mut columns = vec![0; rows].into_boxed_slice();
        let copy_len = std::cmp::min(self.columns.len(), columns.len());
        columns[..copy_len].copy_from_slice(&self.columns[..copy_len]);
        self.columns = columns;
    }

    /// Records the furthest changed column in `row`.
    fn mark(&mut self, row: usize, column: VimDirtyColT) {
        if let Some(dirty_col) = self.columns.get_mut(row) {
            *dirty_col = std::cmp::max(*dirty_col, column);
        }
    }

    /// Scrolls the per-row state upward and clears the final row.
    fn scroll_up(&mut self) {
        let len = self.columns.len();
        if len == 0 {
            return;
        }
        self.columns.copy_within(1..len, 0);
        self.columns[len - 1] = 0;
    }

    /// Returns and clears the dirty column for `row`.
    fn take(&mut self, row: usize) -> Option<VimDirtyColT> {
        self.columns.get_mut(row).map(std::mem::take)
    }

    /// Transfers ownership to the raw representation stored in `CScreenGrid`.
    fn into_raw(self) -> *mut VimDirtyColT {
        Box::into_raw(self.columns) as *mut VimDirtyColT
    }

    /// Reclaims ownership from the raw representation stored in `CScreenGrid`.
    ///
    /// # Safety
    ///
    /// `columns` must be null or uniquely own a Rust boxed slice containing
    /// exactly `rows` initialized values.
    unsafe fn from_raw(columns: *mut VimDirtyColT, rows: usize) -> Option<Self> {
        if columns.is_null() {
            return None;
        }
        let columns = std::ptr::slice_from_raw_parts_mut(columns, rows);
        Some(Self {
            // SAFETY: The caller guarantees that this is the unique pointer
            // produced by `DirtyColumns::into_raw` with the same row count.
            columns: unsafe { Box::from_raw(columns) },
        })
    }
}

/// Storage mode selected by the window subsystem for one screen grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowGridMode {
    /// Render into storage owned by this window grid.
    Dedicated(GridSize),
    /// Render through a view into the shared default grid.
    Shared,
}

/// Grid selected as the backing storage for a window view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowGridViewTarget {
    Dedicated,
    Shared,
}

/// Target and offset selected for a window's grid view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowGridViewSelection {
    target: WindowGridViewTarget,
    offset: GridPosition,
}

/// Complete result of configuring a window grid and its view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowGridUpdate {
    grid_id: GridId,
    lifecycle: GridLifecycleChange,
    view: WindowGridViewSelection,
}

/// Storage mode requested for the message grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageGridMode {
    Visible(GridSize),
    Hidden,
}

/// Transition prepared for the message-grid owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageGridTransition {
    Unchanged,
    Resized,
    /// The compositor must remove the layer before storage is released.
    ReleaseRequired,
}

/// Persistent bookkeeping for throttled message-grid output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MessageScrollState {
    position_at_last_flush: i32,
    scrolled_lines_at_last_flush: i32,
    scroll_discount: i32,
}

impl MessageScrollState {
    /// Creates initial message-scroll bookkeeping.
    const fn new() -> Self {
        Self {
            position_at_last_flush: 0,
            scrolled_lines_at_last_flush: 0,
            scroll_discount: 0,
        }
    }

    /// Records a message position that was published immediately.
    fn record_position(&mut self, position: i32) {
        self.position_at_last_flush = position;
    }

    /// Synchronizes the scroll baseline after allocating or resizing the grid.
    fn record_resize(&mut self, scrolled_lines: i32) {
        self.scrolled_lines_at_last_flush = scrolled_lines;
    }

    /// Accounts for the extra row redrawn by the message scrollback pager.
    fn discount_first_scroll(&mut self) {
        self.scrolled_lines_at_last_flush = self
            .scrolled_lines_at_last_flush
            .checked_sub(1)
            .expect("message scroll baseline underflow");
        self.scroll_discount = self
            .scroll_discount
            .checked_add(1)
            .expect("message scroll discount overflow");
    }

    /// Records the state observed by a completed output flush.
    fn finish_flush(&mut self, position: i32, scrolled_lines: i32) {
        self.position_at_last_flush = position;
        self.scrolled_lines_at_last_flush = scrolled_lines;
        self.scroll_discount = 0;
    }

    /// Discards pending scrolling after a full message redraw or reset.
    fn reset_progress(&mut self) {
        self.scrolled_lines_at_last_flush = 0;
        self.scroll_discount = 0;
    }
}

static MESSAGE_GRID_SCROLL_STATE: Mutex<MessageScrollState> = Mutex::new(MessageScrollState::new());

/// Acquires the persistent message-grid scrolling state.
fn lock_message_scroll_state() -> MutexGuard<'static, MessageScrollState> {
    MESSAGE_GRID_SCROLL_STATE
        .try_lock()
        .expect("message-grid scroll state is already borrowed")
}

/// Validated input for flushing pending message-grid output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MessageGridScrollFlushRequest {
    position: i32,
    scrolled_lines: i32,
    screen_size: GridSize,
    clear_attribute: VimSAttrT,
}

/// Owned output produced by one message-grid flush.
struct MessageGridScrollFlushPlan {
    target: GridOutputTarget,
    position_update: Option<i32>,
    scroll: Option<UiCommand<LocalGridSpace>>,
    lines: Vec<OwnedRawLineCommand>,
}

impl MessageGridScrollFlushPlan {
    /// Creates a flush with no pending output.
    fn empty(target: GridOutputTarget) -> Self {
        Self {
            target,
            position_update: None,
            scroll: None,
            lines: Vec::new(),
        }
    }
}

/// Result of scrolling the message display upward.
struct MessageGridScrollUp {
    started_throttling: bool,
    action: MessageGridScrollUpAction,
}

/// Grid action selected while scrolling the message display upward.
enum MessageGridScrollUpAction {
    /// Move the message grid before optionally clearing its first row.
    Reposition {
        position: i32,
        scrolled: bool,
        clear_first_row: bool,
    },
    /// Shift the message-grid rows in their existing position.
    ShiftRows {
        target: GridOutputTarget,
        scroll: Option<UiCommand<LocalGridSpace>>,
    },
}

/// Effects of changing the logical message-grid position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MessageGridPositionUpdate {
    publish: bool,
    view_row_offset: Option<i32>,
}

/// C callback used to publish a message-grid position.
type CMessageGridPositionEmitter = unsafe extern "C" fn(row: libc::c_int, scrolled: bool);

/// State transition performed while configuring one window grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GridLifecycleChange {
    Unchanged,
    Resized,
    Released,
    Revalidated,
    Invalidated,
}

/// One request to scroll lines within a window's visible grid region.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowLineScrollRequest {
    row: i32,
    line_count: i32,
    view_origin: GridPosition,
    view_size: GridSize,
}

/// Includes final NUL. MAX_MCO is no longer used, but at least 4*(MAX_MCO+1)+1=29
/// ensures we can fit all composed chars which did fit before.
static MAX_SCHAR_SIZE: usize = 32;

static NULL_TERMINATOR: u8 = b'\0';

static GLYPH_MAPPING: Mutex<GlyphMapping> = Mutex::new(GlyphMapping::new());

/// Acquires access to the process-global glyph mapping.
fn lock_glyph_mapping() -> MutexGuard<'static, GlyphMapping> {
    GLYPH_MAPPING
        .lock()
        .expect("glyph mapping mutex must not be poisoned")
}

const DEFAULT_GRID_HANDLE: VimHandleT = 1;

/// Assigns monotonically increasing handles to newly created grids.
struct GridHandleAllocator {
    last: AtomicI32,
}

impl GridHandleAllocator {
    /// Creates an allocator whose next handle follows `last_assigned`.
    const fn new(last_assigned: VimHandleT) -> Self {
        Self {
            last: AtomicI32::new(last_assigned),
        }
    }

    /// Assigns the next handle when `handle` is currently unset.
    fn assign(&self, handle: &mut VimHandleT) {
        if *handle != 0 {
            return;
        }

        // Relaxed ordering is sufficient because the counter only provides
        // unique values; access to each grid is synchronized separately.
        let previous = self
            .last
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
                last.checked_add(1)
            })
            .expect("grid handle space exhausted");
        *handle = previous + 1;
    }
}

static GRID_HANDLE_ALLOCATOR: GridHandleAllocator = GridHandleAllocator::new(DEFAULT_GRID_HANDLE);

unsafe extern "C" {
    static mut full_screen: bool;
    static mut rdb_flags: libc::c_uint;

    static mut p_arshape: libc::c_int;
    static mut p_tbidi: libc::c_int;

    pub(crate) static mut default_grid: CScreenGrid;
    pub(crate) static mut msg_grid: CScreenGrid;
    static mut msg_grid_adj: CScreenGrid;
    static mut pum_grid: CScreenGrid;

    fn decor_check_invalid_glyphs();
    fn check_chars_options() -> bool;
    /// Shape one Arabic base codepoint using its neighboring screen cells.
    #[link_name = "arabic_shape"]
    fn arabic_shape_ffi(
        c: libc::c_int,
        c1p: *mut libc::c_int,
        prev_c: libc::c_int,
        prev_c1: libc::c_int,
        next_c: libc::c_int,
    ) -> libc::c_int;
    fn hl_combine_attr(char_attr: libc::c_int, prim_attr: libc::c_int) -> libc::c_int;
    fn hl_blend_attrs(
        back_attr: libc::c_int,
        front_attr: libc::c_int,
        through: *mut bool,
    ) -> libc::c_int;
    fn hl_apply_winblend(winbl: libc::c_int, attr: libc::c_int) -> libc::c_int;
    fn syn_id2attr(hl_id: libc::c_int) -> libc::c_int;
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

    /// Dirty-column tracking while output is throttled.
    ///
    /// `None` means that tracking is disabled for this grid.
    dirty_columns: Option<DirtyColumns>,

    /// the size of the allocated grid.
    pub rows: i32,
    pub cols: i32,

    /// The state of the grid is valid. Otherwise it needs to be redrawn.
    valid: bool,

    /// only draw internally and don't send updates yet to the compositor or
    /// external UI.
    throttled: bool,
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
            dirty_columns: None,
            rows,
            cols,
            valid: false,
            throttled: false,
        };
        for i in 0..rows {
            let offset = i as usize * cols as usize;
            grid.line_offset.push(offset);
        }

        grid.realloc_line_buffer(line_buffer);

        grid
    }

    /// Returns this grid's stable Rust identity.
    pub(crate) fn id(&self) -> GridId {
        GridId::new(self.handle)
    }

    /// Reports whether this grid requires a complete redraw.
    fn requires_redraw(&self) -> bool {
        !self.valid
    }

    /// Reports whether a screen update must invalidate composed output.
    fn requires_compositor_invalidation(&self, clear_requested: bool) -> bool {
        clear_requested || self.requires_redraw()
    }

    /// Prepares invalid grid contents for a complete redraw.
    ///
    /// Existing attributes are invalidated so subsequent rendering cannot
    /// reuse stale cells. The grid is then ready to receive the redraw.
    fn revalidate_for_redraw(&mut self) {
        if !self.requires_redraw() {
            return;
        }
        self.invalidate();
        self.valid = true;
    }

    /// Records that this grid requires a complete redraw.
    fn require_full_redraw(&mut self) {
        self.valid = false;
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
        if let Some(dirty_columns) = &mut new_grid.dirty_columns {
            dirty_columns.resize(rows as usize);
        }

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

    /// Configures storage and validity for the window subsystem's chosen mode.
    ///
    /// Valid contents are retained across a dedicated-grid resize. An invalid
    /// grid is instead cleared to invalid cells, matching the redraw contract
    /// formerly implemented by `win_grid_alloc()`.
    fn configure_for_window(
        &mut self,
        mode: WindowGridMode,
        line_buffer: &mut LineBuffer,
    ) -> GridLifecycleChange {
        match mode {
            WindowGridMode::Dedicated(size)
                if !self.is_allocated() || self.rows != size.height || self.cols != size.width =>
            {
                let copy_contents = self.valid;
                *self = Self::from_grid(
                    self,
                    size.height,
                    size.width,
                    false,
                    copy_contents,
                    line_buffer,
                );
                self.valid = true;
                GridLifecycleChange::Resized
            }
            WindowGridMode::Dedicated(_) if !self.valid => {
                self.invalidate();
                self.valid = true;
                GridLifecycleChange::Revalidated
            }
            WindowGridMode::Dedicated(_) => GridLifecycleChange::Unchanged,
            WindowGridMode::Shared if self.is_allocated() => {
                self.release_storage();
                self.valid = false;
                GridLifecycleChange::Released
            }
            WindowGridMode::Shared => GridLifecycleChange::Unchanged,
        }
    }

    /// Configures window-grid storage and selects its backing view.
    fn configure_window_view(
        &mut self,
        mode: WindowGridMode,
        offset: GridPosition,
        line_buffer: &mut LineBuffer,
    ) -> WindowGridUpdate {
        let target = match mode {
            WindowGridMode::Dedicated(_) => WindowGridViewTarget::Dedicated,
            WindowGridMode::Shared => WindowGridViewTarget::Shared,
        };
        let lifecycle = self.configure_for_window(mode, line_buffer);

        WindowGridUpdate {
            grid_id: self.id(),
            lifecycle,
            view: WindowGridViewSelection { target, offset },
        }
    }

    /// Assigns the reserved identity used by the default screen grid.
    fn initialize_as_default(&mut self) {
        self.handle = DEFAULT_GRID_HANDLE;
    }

    /// Resizes the default grid when its configured dimensions have changed.
    fn configure_as_default(
        &mut self,
        size: GridSize,
        line_buffer: &mut LineBuffer,
    ) -> GridLifecycleChange {
        if size.height == 0
            || size.width == 0
            || (self.is_allocated() && self.rows == size.height && self.cols == size.width)
        {
            return GridLifecycleChange::Unchanged;
        }

        *self = Self::from_grid(self, size.height, size.width, true, true, line_buffer);
        self.initialize_as_default();
        GridLifecycleChange::Resized
    }

    /// Configures storage and redraw state for the popup-menu grid.
    ///
    /// Existing cells are copied during a resize only when the popup remains
    /// valid at its compositor position.
    fn configure_for_popup_menu(
        &mut self,
        size: GridSize,
        contents_invalid: bool,
        line_buffer: &mut LineBuffer,
    ) -> GridLifecycleChange {
        if !self.is_allocated() || self.rows != size.height || self.cols != size.width {
            *self = Self::from_grid(
                self,
                size.height,
                size.width,
                false,
                !contents_invalid,
                line_buffer,
            );
            GridLifecycleChange::Resized
        } else if contents_invalid {
            self.invalidate();
            GridLifecycleChange::Invalidated
        } else {
            GridLifecycleChange::Unchanged
        }
    }

    /// Prepares storage for the requested message-grid mode.
    ///
    /// A hidden grid with allocated storage returns `ReleaseRequired` without
    /// releasing it, so the compositor can first recompose the covered area.
    fn prepare_for_messages(
        &mut self,
        mode: MessageGridMode,
        handle_allocator: &GridHandleAllocator,
        line_buffer: &mut LineBuffer,
    ) -> MessageGridTransition {
        handle_allocator.assign(&mut self.handle);
        match mode {
            MessageGridMode::Visible(size)
                if !self.is_allocated() || self.rows != size.height || self.cols != size.width =>
            {
                *self = Self::from_grid(self, size.height, size.width, true, false, line_buffer);
                self.reset_dirty_columns();
                self.throttled = false;
                MessageGridTransition::Resized
            }
            MessageGridMode::Visible(_) => MessageGridTransition::Unchanged,
            MessageGridMode::Hidden if self.is_allocated() => {
                MessageGridTransition::ReleaseRequired
            }
            MessageGridMode::Hidden => MessageGridTransition::Unchanged,
        }
    }

    /// Releases message-grid storage after its compositor layer was removed.
    fn release_message(&mut self) {
        self.release_storage();
        self.throttled = false;
    }

    /// Determines the externally visible effects of positioning this message
    /// grid.
    fn update_message_position(&self, row: i32) -> MessageGridPositionUpdate {
        assert!(row >= 0, "message-grid position must be nonnegative");
        MessageGridPositionUpdate {
            publish: !self.throttled,
            view_row_offset: self.is_allocated().then_some(-row),
        }
    }

    /// Starts one upward message-grid scroll.
    ///
    /// Repositioning is returned as a separate action because its callback
    /// must run after the current grid borrow ends and before a possible
    /// zero-`cmdheight` row clear.
    fn scroll_message_up(
        &mut self,
        current_position: i32,
        enable_throttling: bool,
        zero_cmdheight: bool,
    ) -> MessageGridScrollUp {
        let started_throttling = enable_throttling && !self.throttled;
        if enable_throttling {
            self.throttled = true;
        }

        if current_position > 0 {
            return MessageGridScrollUp {
                started_throttling,
                action: MessageGridScrollUpAction::Reposition {
                    position: current_position - 1,
                    scrolled: !zero_cmdheight,
                    clear_first_row: zero_cmdheight,
                },
            };
        }

        let target = GridOutputTarget::new(self.id(), self.cols);
        let scroll = self.del_lines(0, 1, self.rows, 0, self.cols);
        self.scroll_dirty_columns();
        MessageGridScrollUp {
            started_throttling,
            action: MessageGridScrollUpAction::ShiftRows { target, scroll },
        }
    }

    /// Clears the first message-grid row when storage is allocated.
    fn clear_first_message_row(&mut self) {
        if !self.is_allocated() {
            return;
        }
        let width =
            usize::try_from(self.cols).expect("allocated message-grid width must be nonnegative");
        self.clear_row(0, width, false);
    }

    /// Disables deferred message-grid output.
    fn disable_message_throttling(&mut self) {
        self.throttled = false;
    }

    /// Clears a prefix of complete message-grid rows.
    fn clear_message_rows(&mut self, rows_to_clear: usize, valid_rows: usize) {
        if !self.is_allocated() {
            return;
        }
        let row_count = std::cmp::min(
            rows_to_clear,
            usize::try_from(self.rows).expect("allocated message-grid height must be nonnegative"),
        );
        let width =
            usize::try_from(self.cols).expect("allocated message-grid width must be nonnegative");
        for row in 0..row_count {
            self.clear_row(row, width, row < valid_rows);
        }
    }

    /// Clears message rows before redrawing the screen and ends throttling.
    fn prepare_message_redraw(&mut self, rows_to_clear: usize, valid_rows: usize) {
        self.clear_message_rows(rows_to_clear, valid_rows);
        self.disable_message_throttling();
    }

    /// Finishes one throttled message-grid update.
    ///
    /// Cell payloads are copied into owned commands so they remain valid after
    /// the grid borrow ends and while UI callbacks are running.
    fn flush_message_scroll(
        &mut self,
        request: MessageGridScrollFlushRequest,
        state: MessageScrollState,
    ) -> MessageGridScrollFlushPlan {
        let target = GridOutputTarget::new(self.id(), self.cols);
        let mut plan = MessageGridScrollFlushPlan::empty(target);
        if !self.throttled {
            return plan;
        }
        self.throttled = false;

        assert!(
            request.position >= 0 && state.position_at_last_flush >= request.position,
            "message-grid positions must be ordered and nonnegative"
        );
        let position_delta = state.position_at_last_flush - request.position;
        let scrolled_delta = request
            .scrolled_lines
            .checked_sub(state.scrolled_lines_at_last_flush)
            .expect("message scroll delta overflow");
        assert!(
            scrolled_delta >= 0,
            "message scroll count must not move backwards"
        );
        let line_delta = std::cmp::min(scrolled_delta, self.rows);

        if position_delta > 0 {
            plan.position_update = Some(request.position);
        }

        let rows_to_scroll = line_delta
            .checked_sub(position_delta)
            .and_then(|rows| rows.checked_sub(state.scroll_discount))
            .expect("message scroll calculation overflow");
        assert!(
            rows_to_scroll >= 0,
            "message scroll discount exceeds pending output"
        );

        // The position should normally already be zero when scrolling begins,
        // but headless message output does not always preserve that invariant.
        if rows_to_scroll > 0 && request.position == 0 {
            let region =
                GridRegion::try_new(0..request.screen_size.height, 0..request.screen_size.width)
                    .expect("validated screen dimensions form a grid region");
            plan.scroll = Some(UiCommand::GridScroll {
                region,
                delta: GridDelta::new(rows_to_scroll, 0),
            });
        }

        let first_screen_row = request
            .screen_size
            .height
            .saturating_sub(line_delta.max(1))
            .max(0);
        for screen_row in first_screen_row..request.screen_size.height {
            let row = screen_row - request.position;
            assert!(
                0 <= row && row < self.rows,
                "dirty message row must belong to the message grid"
            );
            let dirty_column = self.take_dirty_column(row as usize).unwrap_or_default();
            plan.lines.push(ui::ui_line_r(
                self,
                row,
                false,
                0,
                dirty_column,
                self.cols,
                request.clear_attribute,
                false,
            ));
        }
        plan
    }

    /// Reports whether this grid currently owns cell storage.
    fn is_allocated(&self) -> bool {
        !self.chars.is_empty()
    }

    /// Creates the resize output for an allocated grid.
    fn resize_command_if_allocated(&self) -> Option<(GridOutputTarget, UiCommand<LocalGridSpace>)> {
        if !self.is_allocated() {
            return None;
        }
        let size = GridSize::try_new(self.rows, self.cols)?;
        let target = GridOutputTarget::new(self.id(), self.cols);
        Some((target, UiCommand::GridResize(size)))
    }

    /// Releases cell storage while preserving this grid's stable identity.
    fn release_storage(&mut self) {
        self.chars = Vec::new();
        self.attrs = Vec::new();
        self.vcols = Vec::new();
        self.line_offset = Vec::new();
        self.dirty_columns = None;
    }

    fn realloc_line_buffer(&self, line_buffer: &mut LineBuffer) {
        let ucols = self.cols as usize;
        if ucols <= line_buffer.len() {
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

    /// Enables dirty-column tracking and resets every row to clean.
    fn reset_dirty_columns(&mut self) {
        self.dirty_columns = Some(DirtyColumns::new(self.rows as usize));
    }

    /// Records the furthest changed column in `row` when tracking is enabled.
    fn mark_dirty_column(&mut self, row: usize, column: VimDirtyColT) {
        if let Some(dirty_columns) = &mut self.dirty_columns {
            dirty_columns.mark(row, column);
        }
    }

    /// Scrolls dirty-column state upward when tracking is enabled.
    fn scroll_dirty_columns(&mut self) {
        if let Some(dirty_columns) = &mut self.dirty_columns {
            dirty_columns.scroll_up();
        }
    }

    /// Returns and clears one row's dirty column when tracking is enabled.
    fn take_dirty_column(&mut self, row: usize) -> Option<VimDirtyColT> {
        self.dirty_columns.as_mut()?.take(row)
    }

    /// Takes ownership of every allocation stored in a C screen grid.
    ///
    /// The C pointer fields remain available as non-owning views because
    /// synchronous C UI callbacks may inspect this or other global grids while
    /// the Rust operation is active.
    ///
    /// # Safety
    ///
    /// Every non-null buffer pointer in `cgrid` must have been produced by
    /// `ScreenGrid::to_cgrid`, must remain uniquely owned by `cgrid`, and must
    /// describe the lengths implied by `rows` and `cols`. Until ownership is
    /// returned with `ScreenGrid::to_cgrid`, the C pointers must not be
    /// reclaimed, reallocated, or used to create another owner.
    pub(crate) unsafe fn take_from_cgrid(cgrid: &mut CScreenGrid) -> Self {
        let num_cells = cgrid.rows as usize * cgrid.cols as usize;
        // SAFETY: The caller guarantees ownership and the cell-buffer lengths.
        let chars = unsafe { take_c_buffer(cgrid.chars, num_cells) };
        // SAFETY: The caller guarantees ownership and the cell-buffer lengths.
        let attrs = unsafe { take_c_buffer(cgrid.attrs, num_cells) };
        // SAFETY: The caller guarantees ownership and the cell-buffer lengths.
        let vcols = unsafe { take_c_buffer(cgrid.vcols, num_cells) };
        // SAFETY: The caller guarantees ownership and the row-buffer length.
        let line_offset = unsafe { take_c_buffer(cgrid.line_offset, cgrid.rows as usize) };
        // SAFETY: The caller guarantees ownership and the row-buffer length.
        let dirty_columns = unsafe { DirtyColumns::from_raw(cgrid.dirty_col, cgrid.rows as usize) };

        Self {
            handle: cgrid.handle,
            chars,
            attrs,
            vcols,
            line_offset,
            dirty_columns,
            rows: cgrid.rows,
            cols: cgrid.cols,
            valid: cgrid.valid,
            throttled: cgrid.throttled,
        }
    }

    /// # Safety
    ///
    /// The returned `CScreenGrid` becomes the unique owner of the `chars`,
    /// `attrs`, `vcols`, `line_offset`, and `dirty_col` allocations. The
    /// caller must not duplicate or independently free those pointers, must
    /// access them only within the lengths described by `rows` and `cols`, and
    /// must reconstruct at most one `ScreenGrid` from them.
    pub unsafe fn to_cgrid(self) -> CScreenGrid {
        CScreenGrid {
            handle: self.handle,
            chars: leak_nonempty_buffer(self.chars),
            attrs: leak_nonempty_buffer(self.attrs),
            vcols: leak_nonempty_buffer(self.vcols),
            line_offset: leak_nonempty_buffer(self.line_offset),
            dirty_col: self
                .dirty_columns
                .map(DirtyColumns::into_raw)
                .unwrap_or(std::ptr::null_mut()),
            rows: self.rows,
            cols: self.cols,
            valid: self.valid,
            throttled: self.throttled,
        }
    }

    pub fn clear_line(&mut self, offset: usize, width: usize, valid: bool) {
        let range = offset..(offset + width);
        self.chars[range.clone()].fill(schar_from_ascii(' '));
        self.attrs[range.clone()].fill(Self::attrs_fill(valid));
        self.vcols[range.clone()].fill(-1);
    }

    /// Clears cells from the beginning of one logical grid row.
    fn clear_row(&mut self, row: usize, width: usize, valid: bool) {
        let Some(&offset) = self.line_offset.get(row) else {
            return;
        };
        let Some(end) = offset.checked_add(width) else {
            return;
        };
        if end > self.chars.len() || end > self.attrs.len() || end > self.vcols.len() {
            return;
        }
        self.clear_line(offset, width, valid);
    }

    /// Clears complete logical rows using this grid's allocated width.
    ///
    /// Returns `false` when the grid has no allocated cell storage.
    fn clear_rows(&mut self, rows: Range<usize>, valid: bool) -> bool {
        if !self.is_allocated() {
            return false;
        }
        let (Ok(row_count), Ok(width)) = (usize::try_from(self.rows), usize::try_from(self.cols))
        else {
            return false;
        };
        let start = rows.start.min(row_count);
        let end = rows.end.min(row_count);
        for row in start..end {
            self.clear_row(row, width, valid);
        }
        true
    }

    /// Clears every allocated logical row.
    fn clear_all(&mut self, valid: bool) -> bool {
        let Ok(row_count) = usize::try_from(self.rows) else {
            return false;
        };
        self.clear_rows(0..row_count, valid)
    }

    fn attrs_fill(valid: bool) -> VimSAttrT {
        if valid { 0 } else { -1 }
    }

    fn invalidate(&mut self) {
        self.attrs.fill(-1);
    }

    /// Marks a logical row as invalid by invalidating its first cell.
    fn invalidate_row(&mut self, row: usize) {
        let Some(&offset) = self.line_offset.get(row) else {
            return;
        };
        if let Some(attr) = self.attrs.get_mut(offset) {
            *attr = -1;
        }
    }

    /// Returns the complete contents of one logical grid cell.
    pub(crate) fn cell(&self, row: usize, col: usize) -> Option<GridCell> {
        let width = usize::try_from(self.cols).ok()?;
        if col >= width {
            return None;
        }
        let offset = self.line_offset.get(row)?.checked_add(col)?;
        Some(GridCell::new(
            *self.chars.get(offset)?,
            *self.attrs.get(offset)?,
            *self.vcols.get(offset)?,
        ))
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
                && ((lb.cells[col].character != self.chars[off_to]
                    || lb.cells[col].attribute != self.attrs[off_to]
                    || (cols > 1
                        && lb.cells[col + 1].character == 0
                        && lb.cells[col + 1].character != self.chars[off_to + 1]))
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
    ) -> Vec<OwnedRawLineCommand> {
        let mut output = Vec::new();
        assert!(0 <= row && row < self.rows);
        let is_right_left_set = flags & Slf::Rightleft as i32 != 0;
        let is_inc_vcol_set = flags & Slf::IncVcol as i32 != 0;
        let is_wrap_set = flags & Slf::Wrap as i32 != 0;

        let mut clear_next = false;
        if endcol > self.cols {
            endcol = self.cols;
        }
        if row >= self.rows || col_offset >= self.cols {
            return output;
        }
        let row = row as usize;
        let invalid_row =
            self.id() != GridId::new(DEFAULT_GRID_HANDLE) && self.is_invalid_row(row) && col == 0;
        let offset_to = self.line_offset[row] + col_offset as usize;
        let max_offset_to = self.line_offset[row] + self.cols as usize;

        // When at the start of the text and overwriting the right half of a
        // two-cell character in the same grid, truncate that into a '>'.
        if col > 0 && self.chars[offset_to + col as usize] == 0 {
            let cell = &mut lb.cells[col as usize - 1];
            cell.character = schar_from_ascii('>');
            cell.attribute = self.attrs[offset_to + col as usize - 1];
            col -= 1;
        }

        let mut clear_start = endcol;
        if is_right_left_set {
            clear_start = col;
            col = endcol;
            endcol = clear_width;
            clear_width = col;
        }

        // SAFETY: Screen updates access these C option globals on Neovim's
        // main thread.
        unsafe {
            if p_arshape != 0 && p_tbidi == 0 && endcol > col {
                let mut glyph_map = lock_glyph_mapping();
                ArabicShaper::new(&mut glyph_map)
                    .shape(&mut lb.cells[col as usize..endcol as usize]);
            }
        }

        if bg_attr != 0 {
            for c in col as usize..endcol as usize {
                let cell = &mut lb.cells[c];
                cell.attribute = unsafe { hl_combine_attr(bg_attr, cell.attribute) };
            }
        }
        let mut redraw_next =
            self.is_char_needs_redraw(col as usize, offset_to + col as usize, endcol - col, lb);

        let mut start_dirty = -1;
        let mut end_dirty = 0;
        while col < endcol {
            // 1: normal char
            // 2: occupies two display cells
            let char_cells = if col + 1 < endcol && lb.cells[col as usize + 1].character == 0 {
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

                let cell = lb.cells[col as usize];
                self.chars[offset] = cell.character;
                if char_cells == 2 {
                    self.chars[offset + 1] = lb.cells[col as usize + 1].character;
                }

                self.attrs[offset] = cell.attribute;
                // For simplicity set the attributes of second half of a
                // double-wide character equal to the first half.
                if char_cells == 2 {
                    self.attrs[offset + 1] = cell.attribute;
                }
            }

            self.vcols[offset] = lb.cells[col as usize].virtual_column;
            if char_cells == 2 {
                self.vcols[offset + 1] = lb.cells[col as usize + 1].virtual_column;
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
                output.push(ui::ui_line_r(
                    self,
                    row as i32,
                    invalid_row,
                    col_offset + clear_dirty_start,
                    col_offset + clear_dirty_start,
                    col_offset + clear_end,
                    clear_attr,
                    is_wrap_set,
                ));
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
                output.push(ui::ui_line_r(
                    self,
                    row as i32,
                    invalid_row,
                    col_offset + start_dirty,
                    col_offset + end_dirty,
                    col_offset + clear_end,
                    clear_attr,
                    is_wrap_set,
                ));
            } else {
                self.mark_dirty_column(row, clear_end);
            }
        }
        output
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
    fn scroll_command(
        &self,
        row: i32,
        end: i32,
        col: i32,
        width: i32,
        row_delta: i32,
    ) -> Option<UiCommand<LocalGridSpace>> {
        if self.throttled {
            return None;
        }
        let right = col.checked_add(width)?;
        let region = GridRegion::try_new(row..end, col..right)?;
        Some(UiCommand::GridScroll {
            region,
            delta: GridDelta::new(row_delta, 0),
        })
    }

    /// Scrolls lines within the visible region of a window grid view.
    ///
    /// The operation clips the window dimensions to the target grid and
    /// translates the requested row through the view origin.
    fn scroll_window_lines(
        &mut self,
        request: WindowLineScrollRequest,
    ) -> Option<UiCommand<LocalGridSpace>> {
        if request.row < 0 || request.view_origin.row < 0 || request.view_origin.column < 0 {
            return None;
        }
        let line_count = request.line_count.checked_abs()?;
        if line_count == 0 {
            return None;
        }

        let visible_width = self
            .cols
            .checked_sub(request.view_origin.column)?
            .min(request.view_size.width);
        let visible_height = self
            .rows
            .checked_sub(request.view_origin.row)?
            .min(request.view_size.height);
        if visible_width < 0
            || visible_height < 0
            || request.row.checked_add(line_count)? >= visible_height
        {
            return None;
        }

        let row = request.row.checked_add(request.view_origin.row)?;
        let end = visible_height.checked_add(request.view_origin.row)?;
        if request.line_count < 0 {
            self.del_lines(
                row,
                line_count,
                end,
                request.view_origin.column,
                visible_width,
            )
        } else {
            self.ins_lines(
                row,
                line_count,
                end,
                request.view_origin.column,
                visible_width,
            )
        }
    }

    fn ins_lines(
        &mut self,
        row: i32,
        line_count: i32,
        end: i32,
        col: i32,
        width: i32,
    ) -> Option<UiCommand<LocalGridSpace>> {
        if line_count <= 0 {
            return None;
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
        self.scroll_command(row, end, col, width, -line_count)
    }

    /// delete lines on the screen and move lines up.
    /// 'end' is the line after the scrolled part. Normally it is Rows.
    /// When scrolling region used 'off' is the offset from the top for the region.
    /// 'row' and 'end' are relative to the start of the region.
    fn del_lines(
        &mut self,
        row: i32,
        line_count: i32,
        end: i32,
        col: i32,
        width: i32,
    ) -> Option<UiCommand<LocalGridSpace>> {
        if line_count <= 0 {
            return None;
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
        self.scroll_command(row, end, col, width, line_count)
    }
} // impl

/// Grid selected for an explicit cell-inspection request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GridInspectionTarget {
    Default,
    PopupMenu,
    Window(GridId),
}

impl GridInspectionTarget {
    /// Interprets an API grid handle using the popup-menu-first selection
    /// order of `nvim__inspect_cell()`.
    fn from_requested_handle(
        requested_handle: i64,
        popup_grid_id: GridId,
    ) -> Result<Self, InvalidGridId> {
        let requested_grid_id = i32::try_from(requested_handle).ok().map(GridId::new);
        if requested_grid_id == Some(popup_grid_id) {
            return Ok(Self::PopupMenu);
        }
        if requested_handle <= i64::from(DEFAULT_GRID_HANDLE) {
            return Ok(Self::Default);
        }
        requested_grid_id.map(Self::Window).ok_or(InvalidGridId)
    }
}

/// Indicates that no allocated window grid matches an inspection request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidGridId;

/// Safe grid set used by one explicit cell-inspection operation.
struct GridCellInspector<'grid> {
    default_grid: &'grid ScreenGrid,
    popup_grid: &'grid ScreenGrid,
    window_grid: Option<&'grid ScreenGrid>,
}

impl<'grid> GridCellInspector<'grid> {
    /// Creates an inspector over the grids available to the API request.
    fn new(
        default: &'grid ScreenGrid,
        popup_grid: &'grid ScreenGrid,
        window_grid: Option<&'grid ScreenGrid>,
    ) -> Self {
        Self {
            default_grid: default,
            popup_grid,
            window_grid,
        }
    }

    /// Reads one cell while distinguishing an invalid grid from an invalid
    /// coordinate.
    fn cell(
        &self,
        target: GridInspectionTarget,
        row: i64,
        column: i64,
    ) -> Result<Option<GridCell>, InvalidGridId> {
        let grid = match target {
            GridInspectionTarget::Default => self.default_grid,
            GridInspectionTarget::PopupMenu => self.popup_grid,
            GridInspectionTarget::Window(grid_id) => self
                .window_grid
                .filter(|grid| grid.id() == grid_id && grid.is_allocated())
                .ok_or(InvalidGridId)?,
        };
        let (Ok(row), Ok(column)) = (usize::try_from(row), usize::try_from(column)) else {
            return Ok(None);
        };
        Ok(grid.cell(row, column))
    }
}

/// Safe borrowed view into a screen grid at a translated origin.
struct GridView<'grid> {
    target: &'grid ScreenGrid,
    offset: GridPosition,
}

impl<'grid> GridView<'grid> {
    /// Creates a view over `target` with the given target-grid offset.
    fn new(target: &'grid ScreenGrid, offset: GridPosition) -> Self {
        Self { target, offset }
    }

    /// Translates a view-relative position into target-grid coordinates.
    fn translated_position(&self, position: GridPosition) -> Option<GridPosition> {
        Some(GridPosition::new(
            position.row.checked_add(self.offset.row)?,
            position.column.checked_add(self.offset.column)?,
        ))
    }

    /// Reads a view-relative cell when the caller's grid identity matches the
    /// view target.
    fn cell_for_grid(&self, expected_grid: GridId, position: GridPosition) -> Option<GridCell> {
        if self.target.id() != expected_grid {
            return None;
        }
        let position = self.translated_position(position)?;
        let (Ok(row), Ok(column)) = (
            usize::try_from(position.row),
            usize::try_from(position.column),
        ) else {
            return None;
        };
        self.target.cell(row, column)
    }

    /// Plans one cursor move translated through this view.
    fn plan_cursor_move(
        &self,
        position: GridPosition,
    ) -> Option<(GridOutputTarget, UiCommand<LocalGridSpace>)> {
        let position = self.translated_position(position)?;
        let target = GridOutputTarget::new(self.target.id(), self.target.cols);
        Some((target, UiCommand::GridCursorGoto(position)))
    }

    /// Reports whether the target grid has `width` columns.
    fn width_matches_target(&self, width: i32) -> bool {
        self.target.cols == width
    }

    /// Reports whether this view has a target that requires a complete redraw.
    fn requires_redraw(&self) -> bool {
        self.target.requires_redraw()
    }
}

static PERMANENT_GRID_BINDINGS: LazyLock<Mutex<HashMap<usize, GridId>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Acquires the temporary mapping from C grid addresses to Rust identities.
fn lock_permanent_grid_bindings() -> MutexGuard<'static, HashMap<usize, GridId>> {
    PERMANENT_GRID_BINDINGS
        .try_lock()
        .expect("permanent grid bindings are already borrowed")
}

/// Returns the stable address used to recognize one temporary C identity.
fn cgrid_address(cgrid: std::ptr::NonNull<CScreenGrid>) -> usize {
    cgrid.as_ptr() as usize
}

/// Resolves a C grid address when its grid has permanent Rust ownership.
fn permanent_grid_id(cgrid: std::ptr::NonNull<CScreenGrid>) -> Option<GridId> {
    lock_permanent_grid_bindings()
        .get(&cgrid_address(cgrid))
        .copied()
}

/// Storage selected for one grid operation at the C boundary.
enum ScreenGridStorage {
    /// Allocations temporarily taken from the legacy C representation.
    Legacy(Option<ScreenGrid>),
    /// A direct borrow from permanent Rust storage.
    Permanent(StoredGridGuard),
}

/// Exclusive Rust access to a screen grid received through the C boundary.
///
/// Legacy grids temporarily transfer their allocations and receive them back
/// when this guard is dropped. Permanently stored grids use the C address only
/// as an opaque identity and remain owned by `GridStore`.
#[must_use = "the guard keeps exclusive ownership of the selected screen grid"]
pub(crate) struct ScreenGridGuard {
    cgrid: std::ptr::NonNull<CScreenGrid>,
    storage: ScreenGridStorage,
}

impl ScreenGridGuard {
    /// Resolves a C grid identity to exclusive Rust grid access.
    ///
    /// # Safety
    ///
    /// `cgrid` must identify either a permanently registered grid or a valid,
    /// exclusively owned legacy `CScreenGrid`. A legacy grid must remain valid
    /// until this guard is dropped, and its buffers must satisfy the ownership
    /// and layout requirements of `ScreenGrid::take_from_cgrid`.
    pub(crate) unsafe fn take(cgrid: *mut CScreenGrid) -> Self {
        let cgrid = std::ptr::NonNull::new(cgrid).expect("screen grid is nullptr");
        let permanent_grid_id = permanent_grid_id(cgrid);
        if let Some(grid_id) = permanent_grid_id {
            let grid = StoredGridGuard::acquire(grid_id)
                .expect("permanent C grid identity must resolve to its Rust grid");
            return Self {
                cgrid,
                storage: ScreenGridStorage::Permanent(grid),
            };
        }

        // SAFETY: The caller guarantees exclusive ownership of the C grid and
        // its allocations for the lifetime of this guard.
        let grid = unsafe { ScreenGrid::take_from_cgrid(&mut *cgrid.as_ptr()) };
        Self {
            cgrid,
            storage: ScreenGridStorage::Legacy(Some(grid)),
        }
    }

    /// Moves a legacy grid into permanent Rust storage.
    ///
    /// The C structure is replaced with an opaque identity after registration.
    pub(crate) fn store_permanently(mut self) -> GridStoreResult<GridId> {
        if let ScreenGridStorage::Permanent(grid) = &self.storage {
            return Ok(grid.id());
        }

        let ScreenGridStorage::Legacy(grid) = &mut self.storage else {
            unreachable!("all screen-grid storage variants are handled");
        };
        let grid_id = grid
            .as_ref()
            .expect("legacy screen-grid guard does not own a grid")
            .id();

        {
            let mut store = lock_grid_store();
            store.validate_registration(grid_id)?;
            let grid = grid
                .take()
                .expect("legacy screen-grid guard does not own a grid");
            store
                .register(grid)
                .expect("validated grid registration must succeed");
        }

        let previous = lock_permanent_grid_bindings().insert(cgrid_address(self.cgrid), grid_id);
        assert!(
            previous.is_none(),
            "C grid address must not already identify permanent storage"
        );

        // SAFETY: This guard has exclusive access to the C representation.
        // Its former allocations now belong to the permanent Rust store.
        unsafe {
            *self.cgrid.as_ptr() = CScreenGrid::opaque(grid_id);
        }
        Ok(grid_id)
    }

    /// Removes and drops a grid that is permanently bound to `cgrid`.
    ///
    /// Returns whether the C address identified permanent storage.
    ///
    /// # Safety
    ///
    /// `cgrid` must be null or point to an exclusively accessible
    /// `CScreenGrid`.
    unsafe fn release_permanent(cgrid: *mut CScreenGrid) -> bool {
        let Some(cgrid) = std::ptr::NonNull::new(cgrid) else {
            return false;
        };
        let Some(grid_id) = lock_permanent_grid_bindings().remove(&cgrid_address(cgrid)) else {
            return false;
        };
        lock_grid_store()
            .remove(grid_id)
            .expect("permanent C grid identity must resolve to its Rust grid");
        true
    }
}

impl std::ops::Deref for ScreenGridGuard {
    type Target = ScreenGrid;

    fn deref(&self) -> &Self::Target {
        match &self.storage {
            ScreenGridStorage::Legacy(grid) => grid
                .as_ref()
                .expect("legacy screen-grid guard does not own a grid"),
            ScreenGridStorage::Permanent(grid) => grid,
        }
    }
}

impl std::ops::DerefMut for ScreenGridGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match &mut self.storage {
            ScreenGridStorage::Legacy(grid) => grid
                .as_mut()
                .expect("legacy screen-grid guard does not own a grid"),
            ScreenGridStorage::Permanent(grid) => grid,
        }
    }
}

impl Drop for ScreenGridGuard {
    fn drop(&mut self) {
        let ScreenGridStorage::Legacy(grid) = &mut self.storage else {
            return;
        };
        let Some(grid) = grid.take() else {
            return;
        };
        // SAFETY: This guard uniquely owns the Rust grid and the C destination
        // remains valid until the guard is dropped.
        unsafe {
            *self.cgrid.as_ptr() = grid.to_cgrid();
        }
    }
}

/// The screen edge at which a line is truncated.
#[derive(Clone, Copy)]
enum TruncationSide {
    /// Truncate at the left edge of the range.
    Left,
    /// Truncate at the right edge of the range.
    Right,
}

/// Screen-cell contents produced from one text glyph.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextGlyphContents {
    /// One screen character, with an empty continuation cell when it is wide.
    Character(VimSCharT),
    /// A run of spaces produced by a tab or a glyph clipped at the line edge.
    Spaces,
}

/// One text glyph prepared for insertion into a line buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TextGlyph {
    contents: TextGlyphContents,
    width: usize,
    bytes_consumed: usize,
}

impl TextGlyph {
    /// Decodes the first glyph in `text` using Neovim's screen-cell rules.
    ///
    /// Tabs become a bounded run of spaces. A non-tab glyph that does not fit
    /// becomes one space without consuming input, matching `line_putchar()`.
    fn from_text(
        text: &[u8],
        max_cells: usize,
        tab_cells: usize,
        glyph_map: &mut GlyphMapping,
    ) -> Option<Self> {
        if text.is_empty() || max_cells == 0 {
            return None;
        }
        let bytes_consumed = mbyte::first_cluster_len(text);
        if bytes_consumed == 0 {
            return None;
        }

        if text[0] == b'\t' {
            if tab_cells == 0 {
                return None;
            }
            return Some(Self {
                contents: TextGlyphContents::Spaces,
                width: tab_cells.min(max_cells),
                bytes_consumed,
            });
        }

        let width = usize::try_from(mbyte::first_char_cells(text)).ok()?;
        if width == 0 {
            return None;
        }
        if width > max_cells {
            return Some(Self {
                contents: TextGlyphContents::Spaces,
                width: 1,
                bytes_consumed: 0,
            });
        }

        Some(Self {
            contents: TextGlyphContents::Character(Self::screen_character(text, glyph_map)),
            width,
            bytes_consumed,
        })
    }

    /// Converts the first text cluster to the representation stored in a cell.
    fn screen_character(text: &[u8], glyph_map: &mut GlyphMapping) -> VimSCharT {
        let prepend_space = mbyte::is_composing_first(mbyte::first_codepoint(text));
        let max_text_len = MAX_SCHAR_SIZE - 1 - usize::from(prepend_space);
        let bounded_text = &text[..text.len().min(max_text_len)];
        let glyph_len = mbyte::first_cluster_len(bounded_text);

        if glyph_len == 1 && text[0] >= 0x80 {
            return 0;
        }

        let prefix_len = usize::from(prepend_space);
        let mut bytes = [0; MAX_SCHAR_SIZE - 1];
        if prepend_space {
            bytes[0] = b' ';
        }
        bytes[prefix_len..prefix_len + glyph_len].copy_from_slice(&text[..glyph_len]);
        schar_from_bytes(&bytes[..prefix_len + glyph_len], glyph_map)
    }
}

/// Highlight composition used while drawing virtual text.
#[derive(Clone, Copy)]
enum VirtualTextHighlightMode {
    Replace,
    Combine,
    Blend,
}

impl From<libc::c_int> for VirtualTextHighlightMode {
    fn from(value: libc::c_int) -> Self {
        match value {
            2 => Self::Combine,
            3 => Self::Blend,
            _ => Self::Replace,
        }
    }
}

/// Source of virtual columns for a line-buffer glyph.
#[derive(Clone, Copy)]
enum VirtualColumns<'a> {
    Incrementing(VimColNrT),
    Provided(&'a [VimColNrT]),
    Unavailable,
    Preserve,
}

//                                                                  abcdef           fedcba
/// Exclusive access to the process-global Rust-owned line buffer.
struct LineBufferGuard {
    line_buffer: MutexGuard<'static, LineBuffer>,
}

impl LineBufferGuard {
    /// Borrows the global line buffer for one non-reentrant rendering
    /// operation.
    fn acquire() -> Self {
        Self {
            line_buffer: LINE_BUFFER
                .try_lock()
                .expect("line buffer is already borrowed"),
        }
    }
}

impl std::ops::Deref for LineBufferGuard {
    type Target = LineBuffer;

    fn deref(&self) -> &Self::Target {
        &self.line_buffer
    }
}

impl std::ops::DerefMut for LineBufferGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.line_buffer
    }
}

static LINE_BUFFER: Mutex<LineBuffer> = Mutex::new(LineBuffer::empty());

/// Grid-independent state for one buffered line operation.
///
/// This type contains no C pointers and performs all line-buffer bookkeeping.
struct LineState {
    row: i32,
    col_offset: i32,
    target_width: i32,
    max_col: i32,
    first: i32,
    last: i32,
    clear_to: i32,
    bg_attr: i32,
    clear_attr: i32,
    flags: i32,
    active: bool,
}

impl LineState {
    /// Creates inactive state for a buffered line operation.
    const fn new() -> Self {
        Self {
            row: -1,
            col_offset: 0,
            target_width: 0,
            max_col: 0,
            first: i32::MAX,
            last: 0,
            clear_to: 0,
            bg_attr: 0,
            clear_attr: 0,
            flags: 0,
            active: false,
        }
    }

    /// Begins buffering `row` at the given grid-column offset within a grid
    /// containing `cols` columns.
    fn begin(&mut self, cols: i32, row: i32, col: i32, line_buf: &mut LineBuffer) {
        assert!(!self.active);
        self.active = true;
        self.row = row;
        self.col_offset = col;
        self.target_width = cols;
        self.first = line_buf.len() as i32;
        self.max_col = std::cmp::min(cols, cols - self.col_offset);
        self.last = 0;
        self.clear_to = 0;
        self.bg_attr = 0;
        self.clear_attr = 0;
        self.flags = 0;

        assert!(self.max_col <= line_buf.len() as i32);

        unsafe {
            if full_screen && (rdb_flags & RdbFlags::Invalid as u32 != 0) {
                // Current batch must not depend on previous line-buffer characters.
                // Set invalid values which will cause assertion failures later if they are used.
                line_buf.invalidate_chars_and_attrs();
            }
        }
    }
    /// Reads a character and attribute from the current rendered grid line.
    ///
    /// Returns a space and attribute `-1` when `col` is past the line boundary.
    fn read_cell(&self, grid: &ScreenGrid, mut col: i32) -> (VimSCharT, VimSAttrT) {
        if col < self.max_col {
            col += self.col_offset;
            let offset = grid.line_offset[self.row as usize] + col as usize;
            let sc = grid.chars[offset];
            let attr = grid.attrs[offset];
            (sc, attr)
        } else {
            // TODO check what shall we return for attr in this case
            (schar_from_ascii(' '), -1)
        }
    }

    /// Buffers one screen character and its highlighting at `col`.
    fn put_cell(&mut self, col: i32, sc: VimSCharT, attr: VimSAttrT, lb: &mut LineBuffer) {
        assert!(self.active);
        if col >= self.max_col {
            return;
        }
        lb.cells[col as usize] = GridCell::new(sc, attr, -1);
        self.first = std::cmp::min(self.first, col);
        // TODO(bfredl): Y U NO DOUBLEWIDTH?
        self.last = std::cmp::max(self.last, col + 1);
    }

    /// Buffers text at `col` without crossing the current line boundary.
    ///
    /// Returns the number of occupied screen cells.
    fn put_text(
        &mut self,
        mut col: i32,
        text: &[u8],
        attr: VimSAttrT,
        lb: &mut LineBuffer,
    ) -> usize {
        assert!(self.active);
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
            if i == 0
                && col > self.first
                && col < self.last
                && lb.cells[col as usize].character == 0
            {
                lb.cells[col as usize - 1].character = schar_from_ascii('>');
            }

            let ucol = col as usize;
            lb.cells[ucol] = GridCell::new(schar, attr, -1);
            if mbyte_cells == 2 {
                lb.cells[ucol + 1] = GridCell::new(0, attr, -1);
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

    /// Fills a half-open cell range and returns its clipped end column.
    fn fill_cells(
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

    /// Schedules a half-open range to be cleared when the line is committed.
    ///
    /// `bg_attr` applies to the buffered line and `clear_attr` to cleared cells.
    fn set_clear_range(
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

    /// Mirrors pending cells across the full target-grid width.
    fn mirror_target_width(&mut self, line_buffer: &mut LineBuffer) {
        let target_width = self.target_width;
        self.mirror_cells(target_width, line_buffer);
    }

    /// Mirrors pending cells within `width` for right-to-left rendering.
    fn mirror_cells(&mut self, width: i32, lb: &mut LineBuffer) {
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

    /// Finishes the current line and reports whether it contains updates.
    fn finish(&mut self) -> bool {
        assert!(self.active);
        self.active = false;
        self.clear_to = std::cmp::max(self.last, self.clear_to);
        assert!(self.clear_to <= self.max_col);
        self.first < self.clear_to
    }

    /// Writes the completed line into an owned Rust grid.
    fn write_line(&self, grid: &mut ScreenGrid, lb: &mut LineBuffer) -> Vec<OwnedRawLineCommand> {
        grid.put_line_buffer(
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
        )
    }
}

/// Safe line renderer for a Rust-owned screen grid.
///
/// The exclusive grid reference remains valid for the renderer's lifetime, so
/// its implementation never needs to retain or dereference a raw grid pointer.
struct LineGrid<'grid> {
    grid: &'grid mut ScreenGrid,
    state: LineState,
}

impl<'grid> LineGrid<'grid> {
    /// Creates an inactive renderer over an exclusively borrowed grid.
    fn new(grid: &'grid mut ScreenGrid) -> Self {
        Self {
            grid,
            state: LineState::new(),
        }
    }

    /// Returns the number of rows in the borrowed grid.
    fn rows(&self) -> i32 {
        self.grid.rows
    }

    /// Returns the number of columns in the borrowed grid.
    fn cols(&self) -> i32 {
        self.grid.cols
    }

    /// Begins buffering a line belonging to the borrowed Rust grid.
    fn begin_grid(&mut self, row: i32, col: i32, line_buf: &mut LineBuffer) {
        self.state.begin(self.grid.cols, row, col, line_buf);
    }

    /// Places one character and its attributes in the buffered line.
    fn put_cell(&mut self, col: i32, sc: VimSCharT, attr: VimSAttrT, line_buf: &mut LineBuffer) {
        self.state.put_cell(col, sc, attr, line_buf);
    }

    /// Places text in the buffered line and returns its occupied cell count.
    fn put_text(
        &mut self,
        col: i32,
        text: &[u8],
        attr: VimSAttrT,
        line_buf: &mut LineBuffer,
    ) -> usize {
        self.state.put_text(col, text, attr, line_buf)
    }

    /// Ends the current line operation and returns its UI output.
    fn flush(&mut self, line_buf: &mut LineBuffer) -> Vec<OwnedRawLineCommand> {
        if self.state.finish() {
            self.state.write_line(&mut *self.grid, line_buf)
        } else {
            Vec::new()
        }
    }
}

/// Adapter for the legacy C API, whose line operation spans several calls.
///
/// C cannot retain a Rust borrow between those calls, so this adapter stores
/// the target as a non-null raw pointer. All implementation state remains in
/// `LineState`; raw-grid access is confined to this adapter.
struct CLineGrid {
    target: Option<std::ptr::NonNull<CScreenGrid>>,
    state: LineState,
}

std::thread_local! {
    static LINE_GRID: RefCell<CLineGrid> = const {
        RefCell::new(CLineGrid::new())
    };
}

/// Runs an operation with exclusive access to this thread's C line adapter.
///
/// A complete begin-to-flush operation must remain on the same thread.
fn with_line_grid<R>(operation: impl FnOnce(&mut CLineGrid) -> R) -> R {
    LINE_GRID.with_borrow_mut(operation)
}

impl CLineGrid {
    /// Creates an inactive adapter with no target grid.
    const fn new() -> Self {
        Self {
            target: None,
            state: LineState::new(),
        }
    }

    /// Returns the active C grid.
    fn target(&self) -> std::ptr::NonNull<CScreenGrid> {
        self.target.expect("line operation has no target grid")
    }

    /// Begins a line operation after resolving a C grid view.
    ///
    /// Must be matched with a grid_line_flush call before moving to another
    /// line.
    ///
    /// # Safety
    ///
    /// `view` must point to a valid `CGridView` and its target grid must remain
    /// valid until the matching flush.
    /// The matching flush must occur on the same thread.
    unsafe fn begin_view(&mut self, view: *mut CGridView, mut row: i32, line_buf: &mut LineBuffer) {
        let mut col = 0;
        // SAFETY: The caller guarantees that `view` and its target are valid.
        let grid = unsafe { grid_adjust(view, &mut row, &mut col) };
        // SAFETY: `grid_adjust` returned the valid target owned by `view`.
        unsafe {
            self.begin_raw_grid(grid, row, col, line_buf);
        }
    }

    /// Begins a line operation directly on a C-owned screen grid.
    ///
    /// # Safety
    ///
    /// `grid` must point to a valid, exclusively accessible `CScreenGrid` and
    /// remain valid until the matching flush.
    /// The matching flush must occur on the same thread.
    unsafe fn begin_raw_grid(
        &mut self,
        grid: *mut CScreenGrid,
        row: i32,
        col: i32,
        line_buf: &mut LineBuffer,
    ) {
        let target = std::ptr::NonNull::new(grid).expect("screen grid is nullptr");
        assert!(self.target.is_none());
        let cols = {
            // SAFETY: The caller guarantees exclusive access to the target
            // grid for this operation.
            unsafe { ScreenGridGuard::take(target.as_ptr()) }.cols
        };
        self.state.begin(cols, row, col, line_buf);
        self.target = Some(target);
    }

    /// Reads a cell from the active C-owned screen grid.
    fn read_cell(&mut self, col: i32) -> (VimSCharT, VimSAttrT) {
        let target = self.target();
        // SAFETY: An active operation uniquely owns the target grid buffers.
        let grid = unsafe { ScreenGridGuard::take(target.as_ptr()) };
        self.state.read_cell(&grid, col)
    }

    /// Buffers one screen character and its highlighting.
    fn put_cell(&mut self, col: i32, sc: VimSCharT, attr: VimSAttrT, line_buf: &mut LineBuffer) {
        self.state.put_cell(col, sc, attr, line_buf);
    }

    /// Buffers text and returns the number of occupied screen cells.
    fn put_text(
        &mut self,
        col: i32,
        text: &[u8],
        attr: VimSAttrT,
        line_buf: &mut LineBuffer,
    ) -> usize {
        self.state.put_text(col, text, attr, line_buf)
    }

    /// Fills a half-open range of cells and returns its clipped end column.
    fn fill_cells(
        &mut self,
        start_col: i32,
        end_col: i32,
        sc: VimSCharT,
        attr: VimSAttrT,
        line_buf: &mut LineBuffer,
    ) -> i32 {
        self.state
            .fill_cells(start_col, end_col, sc, attr, line_buf)
    }

    /// Schedules a half-open range to be cleared when the line is committed.
    fn set_clear_range(
        &mut self,
        start_col: i32,
        end_col: i32,
        bg_attr: VimSAttrT,
        clear_attr: VimSAttrT,
    ) {
        self.state
            .set_clear_range(start_col, end_col, bg_attr, clear_attr);
    }

    /// Plans a cursor move within the currently rendered line.
    fn cursor_update(&self, col: i32) -> (GridOutputTarget, UiCommand<LocalGridSpace>) {
        let target = self.target();
        // SAFETY: An active operation uniquely owns the target grid buffers.
        let grid = unsafe { ScreenGridGuard::take(target.as_ptr()) };
        let output_target = GridOutputTarget::new(grid.id(), grid.cols);
        let command = UiCommand::GridCursorGoto(GridPosition::new(self.state.row, col));
        (output_target, command)
    }

    /// Mirrors pending cells for right-to-left rendering.
    fn mirror_cells(&mut self, width: i32, line_buf: &mut LineBuffer) {
        self.state.mirror_cells(width, line_buf);
    }

    /// Ends a C-facing line operation and returns its UI output.
    fn flush(
        &mut self,
        line_buf: &mut LineBuffer,
    ) -> Option<(GridOutputTarget, Vec<OwnedRawLineCommand>)> {
        let target = self
            .target
            .take()
            .expect("line operation has no target grid");
        if !self.state.finish() {
            return None;
        }

        let (output_target, commands) = {
            // SAFETY: The active operation uniquely owns the target grid
            // buffers.
            let mut grid = unsafe { ScreenGridGuard::take(target.as_ptr()) };
            let output_target = GridOutputTarget::new(grid.id(), grid.cols);
            let commands = self.state.write_line(&mut grid, line_buf);
            (output_target, commands)
        };
        Some((output_target, commands))
    }

    /// Mirrors and flushes one message line.
    fn flush_message_line(
        &mut self,
        right_left: bool,
        line_buffer: &mut LineBuffer,
    ) -> Option<(GridOutputTarget, Vec<OwnedRawLineCommand>)> {
        if right_left {
            self.state.mirror_target_width(line_buffer);
        }
        self.flush_if_valid_row(line_buffer)
    }

    /// Flushes the current line only if its row belongs to the target grid.
    ///
    /// This is a stopgap until message.c has been refactored to behave
    fn flush_if_valid_row(
        &mut self,
        line_buf: &mut LineBuffer,
    ) -> Option<(GridOutputTarget, Vec<OwnedRawLineCommand>)> {
        let target = self.target();
        let rows = {
            // SAFETY: The active target remains exclusively accessible until
            // the matching flush.
            unsafe { ScreenGridGuard::take(target.as_ptr()) }.rows
        };
        if self.state.row < 0 || self.state.row >= rows {
            if (unsafe { rdb_flags } & RdbFlags::Invalid as u32) != 0 {
                panic!("rdb_flags & RdbFlags::Invalid != 0");
            } else {
                self.cancel();
                return None;
            }
        }
        self.flush(line_buf)
    }

    /// Returns the last usable column for the active line.
    fn max_col(&self) -> i32 {
        self.state.max_col
    }

    /// Reports whether the active row reaches or exceeds the target grid.
    fn row_reaches_grid_end(&self) -> bool {
        let target = self.target();
        let rows = {
            // SAFETY: The active target remains exclusively accessible until
            // the matching flush.
            unsafe { ScreenGridGuard::take(target.as_ptr()) }.rows
        };
        self.state.row >= rows
    }

    /// Discards the active operation without writing its buffered contents.
    fn cancel(&mut self) {
        self.target = None;
        self.state.active = false;
    }
}

/// Start a group of grid_line_puts calls that builds a single grid line.
///
/// Must be matched with a grid_line_flush call before moving to
/// another line.
///
/// # Safety
///
/// `view` must point to a valid `CGridView`. Its target grid must remain valid
/// until the matching `grid_line_flush` call.
/// The matching flush must occur on the same thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_line_start(view: *mut CGridView, row: libc::c_int) {
    let mut line_buffer = LineBufferGuard::acquire();
    with_line_grid(|line_grid| unsafe {
        line_grid.begin_view(view, row, &mut line_buffer);
    });
}

/// Begins a buffered line operation directly on a screen grid.
///
/// This Rust function retains the C symbol `screengrid_line_start`.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid` and
/// remain valid until the matching `grid_line_flush` call.
/// The matching flush must occur on the same thread.
#[unsafe(export_name = "screengrid_line_start")]
pub unsafe extern "C" fn begin_raw_grid(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    col: libc::c_int,
) {
    let mut line_buffer = LineBufferGuard::acquire();
    with_line_grid(|line_grid| unsafe {
        line_grid.begin_raw_grid(grid, row, col, &mut line_buffer);
    });
}

/// Get present char from current rendered screen line
///
/// This indicates what already is on screen, not the pending render buffer.
///
/// @return char or space if out of bounds
///
/// # Safety
///
/// An active line operation must reference a valid grid. `attr` must be null
/// or point to valid, properly aligned writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_line_getchar(col: libc::c_int, attr: *mut VimSAttrT) -> VimSCharT {
    let (sc, a) = with_line_grid(|line_grid| line_grid.read_cell(col));
    if !attr.is_null() {
        unsafe {
            *attr = a;
        }
    }
    sc
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_put_schar(col: libc::c_int, schar: VimSCharT, attr: VimSAttrT) {
    let mut line_buffer = LineBufferGuard::acquire();
    with_line_grid(|line_grid| {
        line_grid.put_cell(col, schar, attr, &mut line_buffer);
    });
}

/// # Safety
///
/// An active line operation must reference a valid grid and line buffer. If
/// `text_len` is `-1`, `text` must point to a NUL-terminated string.
/// Otherwise, `text_len` must be nonnegative and `text` must be valid for
/// reads of that many bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_line_puts(
    col: libc::c_int,
    text: *const libc::c_char,
    text_len: libc::c_int,
    attr: VimSAttrT,
) -> libc::c_int {
    let mut line_buffer = LineBufferGuard::acquire();
    let len = if text_len == -1 {
        unsafe { libc::strlen(text) }
    } else {
        text_len as usize
    };
    // SAFETY: The caller guarantees that `text` covers `len` readable bytes.
    let text = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), len) };
    let num_cells =
        with_line_grid(|line_grid| line_grid.put_text(col, text, attr, &mut line_buffer));
    num_cells as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_fill(
    start_col: libc::c_int,
    end_col: libc::c_int,
    sc: VimSCharT,
    attr: libc::c_int,
) -> libc::c_int {
    let mut line_buffer = LineBufferGuard::acquire();
    with_line_grid(|line_grid| line_grid.fill_cells(start_col, end_col, sc, attr, &mut line_buffer))
}

/// Writes a popup-menu truncation marker into the current line buffer.
///
/// Invalid signed values, integer overflow, empty ranges, and ranges outside
/// the initialized line buffer are ignored.
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_put_truncation(
    start_col: libc::c_int,
    width: libc::c_int,
    marker: VimSCharT,
    attr: VimSAttrT,
    right_to_left: bool,
) {
    let (Ok(start_col), Ok(width)) = (usize::try_from(start_col), usize::try_from(width)) else {
        return;
    };
    let Some(end_col) = start_col.checked_add(width) else {
        return;
    };
    let side = if right_to_left {
        TruncationSide::Left
    } else {
        TruncationSide::Right
    };
    let marker = (marker != 0).then_some(marker);

    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.put_truncation_marker(start_col..end_col, side, marker, attr);
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_clear_end(
    start_col: libc::c_int,
    end_col: libc::c_int,
    bg_attr: VimSAttrT,
    clear_attr: VimSAttrT,
) {
    with_line_grid(|line_grid| {
        line_grid.set_clear_range(start_col, end_col, bg_attr, clear_attr);
    });
}

/// move the cursor to a position in a currently rendered line.
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_cursor_goto(col: libc::c_int) {
    let (target, command) = with_line_grid(|line_grid| line_grid.cursor_update(col));
    target.emit(command);
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_mirror(width: libc::c_int) {
    let mut line_buffer = LineBufferGuard::acquire();
    with_line_grid(|line_grid| {
        line_grid.mirror_cells(width, &mut line_buffer);
    });
}

/// Resets the visible line-buffer cells before drawing a window line.
///
/// Negative and out-of-bounds widths are ignored.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_reset(width: libc::c_int) {
    let Ok(width) = usize::try_from(width) else {
        return;
    };
    let mut line_buffer = LineBufferGuard::acquire();
    if width <= line_buffer.len() {
        line_buffer.fill(0..width, schar_from_ascii(' '), 0, -1);
    }
}

/// Fills character and attribute cells and returns the resulting column.
///
/// Invalid positions, negative widths, integer overflow, and out-of-bounds
/// ranges leave both the line buffer and returned column unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_fill_chars_and_attrs(
    start_col: libc::c_int,
    width: libc::c_int,
    sc: VimSCharT,
    attr: VimSAttrT,
) -> libc::c_int {
    if start_col < 0 || width < 0 {
        return start_col;
    }
    let Some(end_col) = start_col.checked_add(width) else {
        return start_col;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    if line_buffer.fill_chars_and_attrs(start_col as usize..end_col as usize, sc, attr) {
        end_col
    } else {
        start_col
    }
}

/// Writes one complete line-buffer cell and returns the following column.
///
/// A negative or out-of-bounds column leaves both the line buffer and returned
/// column unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_put_cell(
    col: libc::c_int,
    sc: VimSCharT,
    attr: VimSAttrT,
    vcol: VimColNrT,
) -> libc::c_int {
    if col < 0 {
        return col;
    }
    let Some(next_col) = col.checked_add(1) else {
        return col;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    if line_buffer.update_cell(col as usize, Some(sc), attr, vcol) {
        next_col
    } else {
        col
    }
}

/// Generates and writes one complete fold column.
///
/// When both output pointers are null, cells are written directly to the
/// global line buffer. Otherwise both pointers must describe writable arrays
/// containing `width` elements.
///
/// Invalid dimensions or output pointers leave the destination unchanged and
/// return `start_col`.
///
/// # Safety
///
/// When non-null, `out_vcols` and `out_chars` must point to separate writable
/// arrays of `width` initialized elements. They must not alias Rust-owned
/// line-buffer storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn linebuf_render_fold_column(
    start_col: libc::c_int,
    attr: VimSAttrT,
    width: libc::c_int,
    level: libc::c_int,
    lowest_level: libc::c_int,
    starts_here: bool,
    is_closed: bool,
    virtual_line: bool,
    open: VimSCharT,
    closed: VimSCharT,
    separator: VimSCharT,
    inner: VimSCharT,
    out_vcols: *mut VimColNrT,
    out_chars: *mut VimSCharT,
) -> libc::c_int {
    let (Ok(start_col_index), Ok(width), Ok(level), Ok(lowest_level)) = (
        usize::try_from(start_col),
        usize::try_from(width),
        usize::try_from(level),
        usize::try_from(lowest_level),
    ) else {
        return start_col;
    };

    let fold_column = FoldColumn::render(
        width,
        FoldColumnInfo::new(level, lowest_level, starts_here, is_closed),
        FoldColumnSymbols::new(open, closed, separator, (inner != 0).then_some(inner)),
        virtual_line,
    );

    if out_vcols.is_null() && out_chars.is_null() {
        let mut line_buffer = LineBufferGuard::acquire();
        let end_col = line_buffer.put_fold_column(start_col_index, &fold_column, attr);
        return libc::c_int::try_from(end_col).unwrap_or(start_col);
    }
    if out_vcols.is_null() || out_chars.is_null() {
        return start_col;
    }

    // SAFETY: The caller guarantees that both output pointers address
    // separate writable arrays containing `width` initialized elements.
    let out_vcols = unsafe { std::slice::from_raw_parts_mut(out_vcols, width) };
    // SAFETY: The caller guarantees that this pointer addresses `width`
    // initialized elements and does not alias `out_vcols`.
    let out_chars = unsafe { std::slice::from_raw_parts_mut(out_chars, width) };
    if !fold_column.copy_to(out_chars, out_vcols) {
        return start_col;
    }

    start_col
        .checked_add(libc::c_int::try_from(width).unwrap_or(0))
        .unwrap_or(start_col)
}

/// Writes one rendered glyph and returns its screen-cell width.
///
/// Invalid positions, insufficient space, and virtual-column overflow leave
/// the line buffer unchanged and return zero.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_put_rendered_glyph(
    col: libc::c_int,
    sc: VimSCharT,
    attr: VimSAttrT,
    vcol: VimColNrT,
) -> libc::c_int {
    let Ok(col) = usize::try_from(col) else {
        return 0;
    };
    let mut line_buffer = LineBufferGuard::acquire();
    let glyph_map = lock_glyph_mapping();

    line_buffer
        .put_rendered_glyph(col, sc, attr, vcol, &glyph_map)
        .and_then(|width| libc::c_int::try_from(width).ok())
        .unwrap_or(0)
}

/// Copies `len` values from a C allocation into Rust-owned storage.
///
/// # Safety
///
/// For a nonzero `len`, `items` must point to `len` readable initialized
/// values.
unsafe fn copy_c_slice<T: Copy>(items: *const T, len: usize) -> Option<Vec<T>> {
    if len == 0 {
        return Some(Vec::new());
    }
    if items.is_null() {
        return None;
    }

    // SAFETY: The caller guarantees that `items` covers `len` initialized
    // values.
    Some(unsafe { std::slice::from_raw_parts(items, len) }.to_vec())
}

/// Copies and validates a C variable-tab-stop representation.
///
/// # Safety
///
/// A non-null `variable_tabstops` must address a readable count followed by
/// that many initialized widths.
unsafe fn tab_stops_from_c(
    fixed_width: OptInt,
    variable_tabstops: *const VimColNrT,
) -> Option<TabStops> {
    let variable_widths = if variable_tabstops.is_null() {
        Vec::new()
    } else {
        // SAFETY: The caller guarantees that the count is readable.
        let count = usize::try_from(unsafe { *variable_tabstops }).ok()?;
        if count == 0 {
            Vec::new()
        } else {
            // SAFETY: The caller guarantees that the count is followed by
            // `count` initialized widths.
            unsafe { copy_c_slice(variable_tabstops.add(1), count)? }
        }
    };

    TabStops::try_from_options(fixed_width, &variable_widths).ok()
}

/// C-compatible state returned after rendering a complete text run.
#[repr(C)]
pub struct CLineTextRenderResult {
    end_col: libc::c_int,
    end_vcol: VimColNrT,
    color_cols_consumed: libc::size_t,
    color_cols_exhausted: bool,
}

impl CLineTextRenderResult {
    fn unchanged(col: libc::c_int, vcol: VimColNrT) -> Self {
        Self {
            end_col: col,
            end_vcol: vcol,
            color_cols_consumed: 0,
            color_cols_exhausted: false,
        }
    }
}

/// Copies and renders one complete text run.
///
/// Invalid dimensions, pointers, or tab-stop values leave the line buffer
/// unchanged and return the original column state.
///
/// # Safety
///
/// For a nonzero `text_len`, `text` must point to that many readable bytes.
/// Every non-null `explicit_vcols` and `color_columns` pointer must address the
/// corresponding number of readable initialized values. A non-null
/// `variable_tabstops` must address a readable count followed by that many
/// initialized widths. These allocations must remain valid for this call and
/// must not alias Rust-owned line-buffer storage.
///
/// The global line buffer must be initialized. This function must run on
/// Neovim's main thread while the highlight tables are not otherwise accessed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn linebuf_render_text(
    col: libc::c_int,
    text: *const libc::c_char,
    text_len: libc::size_t,
    max_col: libc::c_int,
    attr: VimSAttrT,
    inc_vcol: bool,
    first_vcol: VimColNrT,
    explicit_vcols: *const VimColNrT,
    explicit_vcols_len: libc::size_t,
    color_columns: *const VimColNrT,
    color_columns_len: libc::size_t,
    color_column_attr: VimSAttrT,
    tabstop: OptInt,
    variable_tabstops: *const VimColNrT,
) -> CLineTextRenderResult {
    let unchanged = CLineTextRenderResult::unchanged(col, first_vcol);
    let (Ok(start_col), Ok(max_col)) = (usize::try_from(col), usize::try_from(max_col)) else {
        return unchanged;
    };

    // SAFETY: The caller guarantees that `text` covers `text_len` bytes.
    let Some(text) = (unsafe { copy_c_slice(text.cast::<u8>(), text_len) }) else {
        return unchanged;
    };
    let explicit_vcols = if explicit_vcols.is_null() {
        if explicit_vcols_len != 0 {
            return unchanged;
        }
        None
    } else {
        // SAFETY: The caller guarantees that `explicit_vcols` covers its
        // supplied length.
        let Some(values) = (unsafe { copy_c_slice(explicit_vcols, explicit_vcols_len) }) else {
            return unchanged;
        };
        Some(values)
    };
    let color_columns = if color_columns.is_null() {
        if color_columns_len != 0 {
            return unchanged;
        }
        None
    } else {
        // SAFETY: The caller guarantees that `color_columns` covers its
        // supplied length.
        let Some(values) = (unsafe { copy_c_slice(color_columns, color_columns_len) }) else {
            return unchanged;
        };
        Some(values)
    };
    // SAFETY: The caller guarantees the counted variable-tab-stop allocation.
    let Some(tab_stops) = (unsafe { tab_stops_from_c(tabstop, variable_tabstops) }) else {
        return unchanged;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    let mut glyph_mapping = lock_glyph_mapping();
    let virtual_columns = if inc_vcol {
        LineTextVirtualColumns::Incrementing {
            color_columns: color_columns.as_deref(),
            color_column_highlight: Highlight(color_column_attr),
        }
    } else if let Some(explicit_vcols) = explicit_vcols.as_deref() {
        LineTextVirtualColumns::Provided(explicit_vcols)
    } else {
        LineTextVirtualColumns::Unavailable
    };
    let result = line_buffer.render_text(
        &text,
        &mut glyph_mapping,
        LineTextRenderOptions::new(
            start_col,
            max_col,
            Highlight(attr),
            first_vcol,
            virtual_columns,
            &tab_stops,
        ),
    );

    CLineTextRenderResult {
        end_col: libc::c_int::try_from(result.end_col).unwrap_or(col),
        end_vcol: result.end_virtual_column,
        color_cols_consumed: result.color_columns_consumed,
        color_cols_exhausted: result.color_columns_exhausted,
    }
}

/// Copies and renders one complete virtual-text item.
///
/// Invalid inputs leave the line buffer unchanged and return `col`.
///
/// # Safety
///
/// For a nonempty `virtual_text`, its item pointer must address every
/// initialized chunk. Each non-null chunk text pointer must address a readable
/// NUL-terminated string. A non-null `variable_tabstops` must address a
/// readable count followed by that many initialized widths. These allocations
/// must remain valid for this call and must not alias Rust-owned storage.
///
/// The global line buffer must be initialized. This function must run on
/// Neovim's main thread while the highlight tables are not otherwise accessed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn linebuf_render_virtual_text(
    col: libc::c_int,
    virtual_text: CVirtText,
    hl_mode: libc::c_int,
    max_col: libc::c_int,
    vcol: VimColNrT,
    skip_cells: libc::c_int,
    eol_hl: bool,
    tabstop: OptInt,
    variable_tabstops: *const VimColNrT,
) -> libc::c_int {
    let (Ok(start_col), Ok(max_col), Ok(skip_cells)) = (
        usize::try_from(col),
        usize::try_from(max_col),
        usize::try_from(skip_cells),
    ) else {
        return col;
    };

    // SAFETY: The caller guarantees that the vector describes initialized
    // virtual-text chunks.
    let Some(raw_chunks) = (unsafe { virtual_text.as_slice() }) else {
        return col;
    };
    let mut chunks = Vec::with_capacity(raw_chunks.len());
    let mut highlight = Highlight::default();
    for chunk in raw_chunks {
        if chunk.hl_id > 0 {
            highlight = highlight.combine(Highlight::from_group(chunk.hl_id));
        }
        if chunk.text.is_null() {
            continue;
        }

        // SAFETY: The caller guarantees that each non-null text pointer
        // addresses a readable NUL-terminated string.
        let text = unsafe { CStr::from_ptr(chunk.text) }.to_bytes().to_vec();
        chunks.push(VirtualTextChunk::new(text, highlight));
        highlight = Highlight::default();
    }
    let virtual_text = VirtualText::new(chunks);

    // SAFETY: The caller guarantees the counted variable-tab-stop allocation.
    let Some(tab_stops) = (unsafe { tab_stops_from_c(tabstop, variable_tabstops) }) else {
        return col;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    let mut glyph_mapping = lock_glyph_mapping();
    let end_col = virtual_text.render(
        &mut line_buffer,
        &mut glyph_mapping,
        VirtualTextRenderOptions::new(
            start_col,
            max_col,
            vcol,
            skip_cells,
            eol_hl,
            &tab_stops,
            VirtualTextHighlightMode::from(hl_mode),
        ),
    );

    libc::c_int::try_from(end_col).unwrap_or(col)
}

/// Updates one line-buffer cell and returns the following column.
///
/// `replace_char` controls whether `sc` replaces the current character. The
/// attribute and virtual column are always updated when the cell is valid.
/// A negative or out-of-bounds column leaves the buffer unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_update_cell(
    col: libc::c_int,
    sc: VimSCharT,
    replace_char: bool,
    attr: VimSAttrT,
    vcol: VimColNrT,
) -> libc::c_int {
    if col < 0 {
        return col;
    }
    let Some(next_col) = col.checked_add(1) else {
        return col;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    if line_buffer.update_cell(col as usize, replace_char.then_some(sc), attr, vcol) {
        next_col
    } else {
        col
    }
}

/// Fills a line-buffer range with one virtual column.
///
/// Negative positions, negative widths, integer overflow, and out-of-bounds
/// ranges leave the line buffer unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_fill_vcols(start_col: libc::c_int, width: libc::c_int, vcol: VimColNrT) {
    if start_col < 0 || width < 0 {
        return;
    }
    let Some(end_col) = start_col.checked_add(width) else {
        return;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.fill_vcols(start_col as usize..end_col as usize, vcol);
}

/// Fills a line-buffer range with consecutive virtual columns.
///
/// Negative positions, negative widths, integer overflow, and out-of-bounds
/// ranges leave the line buffer unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_fill_vcols_from(
    start_col: libc::c_int,
    width: libc::c_int,
    first_vcol: VimColNrT,
) {
    if start_col < 0 || width < 0 {
        return;
    }
    let Some(end_col) = start_col.checked_add(width) else {
        return;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.fill_vcols_from(start_col as usize..end_col as usize, first_vcol);
}

/// Writes a two-cell character sequence without changing cell metadata.
///
/// A negative or out-of-bounds column leaves the line buffer unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_put_char_pair(col: libc::c_int, first: VimSCharT, second: VimSCharT) {
    let Ok(col) = usize::try_from(col) else {
        return;
    };
    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.put_chars(col, &[first, second]);
}

/// End a group of grid_line_puts calls and send the screen buffer to the UI layer.
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_flush() {
    let output = {
        let mut line_buffer = LineBufferGuard::acquire();
        with_line_grid(|line_grid| line_grid.flush(&mut line_buffer))
    };
    if let Some((target, commands)) = output {
        target.emit_lines(commands);
    }
}

/// Mirrors a right-to-left message line and flushes it when its row is valid.
///
/// This is a stopgap until message.c has been refactored to behave
#[unsafe(no_mangle)]
pub extern "C" fn grid_flush_message_line(right_left: bool) {
    let output = {
        let mut line_buffer = LineBufferGuard::acquire();
        with_line_grid(|line_grid| line_grid.flush_message_line(right_left, &mut line_buffer))
    };
    if let Some((target, commands)) = output {
        target.emit_lines(commands);
    }
}

/// # Safety
///
/// `view` must point to a valid `CGridView`, and `config` must point to a valid
/// `CGridLineSubmissionConfig`. The view's target grid must remain valid and
/// exclusively accessible for the duration of the call. Its buffer pointers
/// must satisfy the ownership and layout requirements of `ScreenGrid`. The
/// global line buffer must be initialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_put_window_line(
    view: *const CGridView,
    config: *const CGridLineSubmissionConfig,
) {
    if view.is_null() || config.is_null() {
        return;
    }

    // SAFETY: The caller guarantees that both pointers reference initialized
    // values for the duration of this call.
    let (view, config) = unsafe { (&*view, &*config) };
    let Some(row) = config.row.checked_add(view.row_offset) else {
        return;
    };
    let target = view.target;
    if target.is_null() {
        return;
    }

    let (Ok(end_col), Ok(width)) = (
        usize::try_from(config.end_col),
        usize::try_from(config.width),
    ) else {
        return;
    };
    let smooth_scroll_marker = config.put_smoothscroll_marker.then(|| {
        SmoothScrollMarker::new(config.preserve_line_number, config.smoothscroll_marker_attr)
    });

    let (output_target, commands) = {
        let mut line_buffer = LineBufferGuard::acquire();
        let Some(submission) = LineSubmission::prepare(
            &mut line_buffer,
            end_col,
            width,
            config.clear_end,
            config.right_left,
            smooth_scroll_marker,
        ) else {
            return;
        };
        let (Ok(start_col), Ok(end_col), Ok(clear_width)) = (
            libc::c_int::try_from(submission.start_col()),
            libc::c_int::try_from(submission.end_col()),
            libc::c_int::try_from(submission.clear_width()),
        ) else {
            return;
        };
        let flags = if submission.is_right_left() {
            config.flags | Slf::Rightleft as libc::c_int
        } else {
            config.flags
        };

        // SAFETY: The caller guarantees exclusive access to the target and
        // valid Rust-owned buffer pointers.
        let mut grid = unsafe { ScreenGridGuard::take(target) };
        let output_target = GridOutputTarget::new(grid.id(), grid.cols);
        let commands = grid.put_line_buffer(
            row,
            view.col_offset,
            start_col,
            end_col,
            clear_width,
            config.bg_attr,
            0,
            config.last_vcol,
            flags,
            &mut line_buffer,
        );
        (output_target, commands)
    };
    output_target.emit_lines(commands);
}

/// Clears a rectangular region of a grid view.
///
/// # Safety
///
/// `view` must point to a valid `CGridView` whose target grid remains valid
/// and exclusively accessible for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_clear(
    view: *mut CGridView,
    start_row: libc::c_int,
    end_row: libc::c_int,
    start_col: libc::c_int,
    mut end_col: libc::c_int,
    attr: libc::c_int,
) {
    let output = {
        let mut line_buffer = LineBufferGuard::acquire();
        with_line_grid(|line_grid| {
            let mut output = Vec::new();
            for row in start_row..end_row {
                // SAFETY: The caller guarantees that `view` and its target remain
                // valid and exclusively accessible for this operation.
                unsafe {
                    line_grid.begin_view(view, row, &mut line_buffer);
                }
                end_col = std::cmp::min(end_col, line_grid.max_col());
                if line_grid.row_reaches_grid_end() || start_col >= end_col {
                    line_grid.cancel();
                    break;
                }
                line_grid.set_clear_range(start_col, end_col, attr, 0);
                if let Some(update) = line_grid.flush(&mut line_buffer) {
                    output.push(update);
                }
            }
            output
        })
    };
    for (target, commands) in output {
        target.emit_lines(commands);
    }
}

/// Draws a floating-window border from a stable subset of `WinConfig`.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`.
/// `config` must point to a valid `CGridBorderConfig` whose arrays and enabled
/// virtual-text vectors satisfy the requirements of `Border::from_c`. A
/// non-null `adjustments` must point to four initialized integers. The global
/// line buffer must be initialized, and this function must run on Neovim's
/// main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_draw_border(
    grid: *mut CScreenGrid,
    config: *const CGridBorderConfig,
    adjustments: *const libc::c_int,
    winbl: libc::c_int,
    win_hl_attr_normal: libc::c_int,
) {
    if grid.is_null() || config.is_null() {
        return;
    }

    let border = {
        let mut glyph_map = lock_glyph_mapping();
        // SAFETY: The pointer and all nested border data satisfy the contract
        // documented on this function.
        unsafe {
            Border::from_c(
                &*config,
                adjustments,
                winbl,
                Highlight(win_hl_attr_normal),
                &mut glyph_map,
            )
        }
    };
    let Some(border) = border else {
        return;
    };

    let (target, commands) = {
        // SAFETY: The caller transfers exclusive ownership of the grid
        // buffers into the guard for the duration of this function.
        let mut rust_grid = unsafe { ScreenGridGuard::take(grid) };
        let target = GridOutputTarget::new(rust_grid.id(), rust_grid.cols);
        let mut line_buffer = LineBufferGuard::acquire();
        let mut canvas = BorderCanvas {
            line_grid: LineGrid::new(&mut rust_grid),
            line_buffer: &mut line_buffer,
            output: Vec::new(),
        };
        border.draw(&mut canvas);
        (target, canvas.output)
    };
    target.emit_lines(commands);
}

/// Converts ascii char to `VimSCharT`
pub fn schar_from_ascii(c: char) -> VimSCharT {
    if cfg!(target_endian = "big") {
        return (c as u32).to_be();
    }
    (c as u32).to_le()
}

/// Returns the ASCII byte represented directly by a screen character.
fn schar_to_ascii(sc: VimSCharT) -> Option<u8> {
    if cfg!(target_endian = "big") {
        if sc & 0x80FFFFFF == 0 {
            Some(sc.to_be_bytes()[0])
        } else {
            None
        }
    } else if sc < 0x80 {
        Some(sc as u8)
    } else {
        None
    }
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

fn schar_cells_impl(sc: VimSCharT, glyph_map: &GlyphMapping) -> libc::c_int {
    // Keep the common ASCII case independent of the glyph cache and mbyte code.
    let is_ascii = if cfg!(target_endian = "big") {
        sc & 0x80ff_ffff == 0
    } else {
        sc < 0x80
    };
    if is_ascii {
        return 1;
    }

    let glyph = glyph_map.resolve(sc);
    mbyte::utf_ptr2cells(glyph.as_c_str())
}

fn schar_get_first_codepoint_impl(sc: VimSCharT, glyph_map: &GlyphMapping) -> libc::c_int {
    let glyph = glyph_map.resolve(sc);
    mbyte::utf_ptr2char(glyph.as_c_str())
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

/// C representation of one complete rendered screen cell.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CGridCell {
    character: VimSCharT,
    attribute: VimSAttrT,
    virtual_column: VimColNrT,
}

impl From<GridCell> for CGridCell {
    fn from(cell: GridCell) -> Self {
        Self {
            character: cell.character,
            attribute: cell.attribute,
            virtual_column: cell.virtual_column,
        }
    }
}

/// C representation of an explicit grid-cell inspection result.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CGridCellInspectionResult {
    InvalidGrid = 0,
    MissingCell = 1,
    Found = 2,
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
}

impl CScreenGrid {
    /// Creates the temporary C representation of a Rust-owned grid identity.
    fn opaque(grid_id: GridId) -> Self {
        Self {
            handle: grid_id.as_native_handle(),
            chars: std::ptr::null_mut(),
            attrs: std::ptr::null_mut(),
            vcols: std::ptr::null_mut(),
            line_offset: std::ptr::null_mut(),
            dirty_col: std::ptr::null_mut(),
            rows: 0,
            cols: 0,
            valid: false,
            throttled: false,
        }
    }
}

/// C representation of a pending message-grid lifecycle transition.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CMessageGridTransition {
    Unchanged = 0,
    Resized = 1,
    ReleaseRequired = 2,
}

/// C representation of a completed window-grid configuration.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CWindowGridUpdate {
    grid_handle: VimHandleT,
    resized: bool,
}

/// C representation of the inputs needed to flush message-grid output.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CMessageGridScrollFlushRequest {
    position: libc::c_int,
    scrolled_lines: libc::c_int,
    screen_rows: libc::c_int,
    screen_columns: libc::c_int,
    clear_attribute: VimSAttrT,
}

impl From<MessageGridTransition> for CMessageGridTransition {
    fn from(transition: MessageGridTransition) -> Self {
        match transition {
            MessageGridTransition::Unchanged => Self::Unchanged,
            MessageGridTransition::Resized => Self::Resized,
            MessageGridTransition::ReleaseRequired => Self::ReleaseRequired,
        }
    }
}

impl From<WindowGridUpdate> for CWindowGridUpdate {
    fn from(update: WindowGridUpdate) -> Self {
        Self {
            grid_handle: update.grid_id.as_native_handle(),
            resized: update.lifecycle == GridLifecycleChange::Resized,
        }
    }
}

impl CScreenGrid {
    fn is_allocated(&self) -> bool {
        !self.chars.is_null()
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct CGridView {
    target: *mut CScreenGrid,
    row_offset: libc::c_int,
    col_offset: libc::c_int,
}

/// C representation of the options for submitting a completed window line.
#[repr(C)]
#[derive(Debug)]
pub struct CGridLineSubmissionConfig {
    row: libc::c_int,
    end_col: libc::c_int,
    width: libc::c_int,
    bg_attr: libc::c_int,
    smoothscroll_marker_attr: VimSAttrT,
    last_vcol: VimColNrT,
    flags: libc::c_int,
    clear_end: bool,
    right_left: bool,
    put_smoothscroll_marker: bool,
    preserve_line_number: bool,
}

/// C representation of one virtual-text chunk.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CVirtTextChunk {
    text: *const libc::c_char,
    hl_id: libc::c_int,
}

/// C representation of Neovim's `kvec_t(VirtTextChunk)`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CVirtText {
    size: libc::size_t,
    capacity: libc::size_t,
    items: *const CVirtTextChunk,
}

impl CVirtText {
    /// Borrows the initialized virtual-text chunks.
    ///
    /// # Safety
    ///
    /// For a nonzero size, `items` must point to that many initialized chunks
    /// and remain valid for the returned slice's lifetime.
    unsafe fn as_slice(&self) -> Option<&[CVirtTextChunk]> {
        if self.size == 0 {
            return Some(&[]);
        }
        if self.items.is_null() {
            return None;
        }
        // SAFETY: The caller guarantees that `items` describes `size`
        // initialized chunks.
        Some(unsafe { std::slice::from_raw_parts(self.items, self.size) })
    }
}

/// Stable C-facing subset of `WinConfig` needed to render a border.
#[repr(C)]
#[derive(Debug)]
pub struct CGridBorderConfig {
    chars: *const libc::c_char,
    attrs: *const libc::c_int,
    title: libc::c_int,
    title_pos: libc::c_int,
    title_chunks: CVirtText,
    title_width: libc::c_int,
    title_attr: libc::c_int,
    footer: libc::c_int,
    footer_pos: libc::c_int,
    footer_chunks: CVirtText,
    footer_width: libc::c_int,
    footer_attr: libc::c_int,
}

const BORDER_PART_COUNT: usize = 8;

/// Semantic positions in Neovim's eight-element border arrays.
#[derive(Clone, Copy)]
#[repr(usize)]
enum BorderPart {
    TopLeft = 0,
    Top = 1,
    TopRight = 2,
    Right = 3,
    BottomRight = 4,
    Bottom = 5,
    BottomLeft = 6,
    Left = 7,
}

/// A resolved Neovim highlight attribute.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Highlight(libc::c_int);

impl Highlight {
    /// Wraps a raw Neovim highlight attribute.
    pub(crate) const fn new(attribute: libc::c_int) -> Self {
        Self(attribute)
    }

    /// Returns the raw Neovim highlight attribute.
    pub(crate) const fn raw(self) -> libc::c_int {
        self.0
    }

    /// Combines this base attribute with `primary` using Neovim's highlight
    /// composition rules.
    fn combine(self, primary: Self) -> Self {
        // SAFETY: Both attributes are integer handles. The C function accepts
        // every integer value and returns a resolved attribute by value.
        Self(unsafe { hl_combine_attr(self.0, primary.0) })
    }

    /// Blends `foreground` over this background attribute.
    ///
    /// `through` requests preservation of the underlying character and is
    /// returned with the decision made by Neovim's blend rules.
    pub(crate) fn blend(self, foreground: Self, mut through: bool) -> (Self, bool) {
        // SAFETY: The attributes and initial through flag are passed by value.
        // The C function only updates the local flag and returns an attribute.
        let attr = unsafe { hl_blend_attrs(self.0, foreground.0, &mut through) };
        (Self(attr), through)
    }

    /// Applies the configured floating-window transparency.
    fn with_winblend(self, winbl: libc::c_int) -> Self {
        // SAFETY: The C function accepts the blend amount and attribute by
        // value and does not retain references.
        Self(unsafe { hl_apply_winblend(winbl, self.0) })
    }

    /// Resolves a syntax highlight group to an attribute.
    fn from_group(hl_id: libc::c_int) -> Self {
        // SAFETY: Positive IDs originate from validated virtual-text chunks.
        // The C function accepts the ID by value and retains no references.
        Self(unsafe { syn_id2attr(hl_id) })
    }
}

/// One character and highlight pair in a floating-window border style.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BorderCell {
    character: VimSCharT,
    highlight: Highlight,
}

/// The eight corner and edge cells that make up a border style.
struct BorderStyle {
    cells: [BorderCell; BORDER_PART_COUNT],
}

impl BorderStyle {
    /// Converts bounded C character arrays and attributes into owned cells.
    fn new(
        chars: &[libc::c_char],
        attrs: &[libc::c_int],
        normal: Highlight,
        glyph_map: &mut GlyphMapping,
    ) -> Option<Self> {
        if chars.len() != BORDER_PART_COUNT * MAX_SCHAR_SIZE || attrs.len() != BORDER_PART_COUNT {
            return None;
        }

        let mut cells = [BorderCell::default(); BORDER_PART_COUNT];
        for (index, (cell, raw)) in cells
            .iter_mut()
            .zip(chars.chunks_exact(MAX_SCHAR_SIZE))
            .enumerate()
        {
            let len = raw.iter().position(|byte| *byte == 0)?;
            let bytes: Vec<u8> = raw[..len]
                .iter()
                .map(|byte| byte.to_ne_bytes()[0])
                .collect();
            *cell = BorderCell {
                character: schar_from_bytes(&bytes, glyph_map),
                highlight: normal.combine(Highlight(attrs[index])),
            };
        }
        Some(Self { cells })
    }

    /// Returns the cell for one semantic border position.
    fn cell(&self, part: BorderPart) -> BorderCell {
        self.cells[part as usize]
    }
}

/// Insets describing whether each border edge occupies a grid row or column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BorderAdjustments {
    top: libc::c_int,
    right: libc::c_int,
    bottom: libc::c_int,
    left: libc::c_int,
}

impl Default for BorderAdjustments {
    fn default() -> Self {
        Self {
            top: 1,
            right: 1,
            bottom: 1,
            left: 1,
        }
    }
}

impl From<&[libc::c_int; 4]> for BorderAdjustments {
    fn from(adjustments: &[libc::c_int; 4]) -> Self {
        Self {
            top: adjustments[0],
            right: adjustments[1],
            bottom: adjustments[2],
            left: adjustments[3],
        }
    }
}

/// Horizontal alignment of a border title or footer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BorderTextAlign {
    Left,
    Center,
    Right,
}

impl TryFrom<libc::c_int> for BorderTextAlign {
    type Error = ();

    fn try_from(value: libc::c_int) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Left),
            1 => Ok(Self::Center),
            2 => Ok(Self::Right),
            _ => Err(()),
        }
    }
}

impl BorderTextAlign {
    /// Finds the first grid column for text with `text_width` cells.
    fn column(self, inner_width: libc::c_int, text_width: libc::c_int) -> libc::c_int {
        match self {
            Self::Left => 1,
            Self::Center => ((inner_width - text_width) / 2 + 1).max(1),
            Self::Right => (inner_width - text_width + 1).max(1),
        }
    }
}

/// An owned title/footer chunk with its final display highlight.
#[derive(Debug, PartialEq, Eq)]
struct BorderTextChunk {
    text: Vec<u8>,
    highlight: Highlight,
}

/// Owned, validated title or footer text ready for border rendering.
struct BorderText {
    chunks: Vec<BorderTextChunk>,
    width: libc::c_int,
    align: BorderTextAlign,
    default_highlight: Highlight,
}

impl BorderText {
    /// Copies virtual text out of its C allocation and resolves highlights.
    ///
    /// # Safety
    ///
    /// Every non-null chunk string must be NUL-terminated and readable for
    /// the duration of this call. `chunks` must satisfy `CVirtText::as_slice`.
    unsafe fn from_c(
        chunks: &CVirtText,
        width: libc::c_int,
        align: libc::c_int,
        default_highlight: Highlight,
        winbl: libc::c_int,
        normal: Highlight,
    ) -> Option<Self> {
        let align = BorderTextAlign::try_from(align).ok()?;
        let default_highlight = normal.combine(default_highlight.with_winblend(winbl));
        // SAFETY: The caller upholds the virtual-text allocation contract.
        let raw_chunks = unsafe { chunks.as_slice()? };
        let mut rendered_chunks = Vec::with_capacity(raw_chunks.len());
        let mut attr = -1;

        for chunk in raw_chunks {
            if chunk.hl_id >= 0 {
                attr = attr.max(0);
                if chunk.hl_id > 0 {
                    attr = Highlight(attr)
                        .combine(Highlight::from_group(chunk.hl_id))
                        .0;
                }
            }
            if chunk.text.is_null() {
                continue;
            }

            // SAFETY: The caller guarantees that every non-null text pointer
            // references a readable NUL-terminated string.
            let text = unsafe { CStr::from_ptr(chunk.text) }.to_bytes().to_vec();
            let highlight = if attr == -1 {
                default_highlight
            } else {
                normal.combine(Highlight(attr).with_winblend(winbl))
            };
            rendered_chunks.push(BorderTextChunk { text, highlight });
            attr = -1;
        }

        Some(Self {
            chunks: rendered_chunks,
            width,
            align,
            default_highlight,
        })
    }

    /// Removes complete leading screen characters until `overflow` cells
    /// have been consumed. Returning `None` means the entire chunk is hidden.
    fn visible_suffix<'a>(mut text: &'a [u8], overflow: &mut libc::c_int) -> Option<&'a [u8]> {
        if *overflow <= 0 {
            return Some(text);
        }

        let cells = libc::c_int::try_from(mbyte::string_cells(text)).unwrap_or(libc::c_int::MAX);
        if *overflow >= cells {
            *overflow -= cells;
            return None;
        }

        while !text.is_empty() && *overflow > 0 {
            *overflow -= mbyte::first_char_cells(text);
            let len = mbyte::first_cluster_len(text);
            text = &text[len.min(text.len())..];
        }
        Some(text)
    }

    /// Draws this title or footer over a horizontal border edge.
    fn draw(&self, canvas: &mut BorderCanvas, inner_width: libc::c_int) {
        let mut col = self.align.column(inner_width, self.width);
        let mut overflow = self.width - inner_width;

        if overflow > 0 {
            canvas.put_text(1, b"<", self.default_highlight);
            col += 1;
            overflow += 1;
        }

        for chunk in &self.chunks {
            let Some(text) = Self::visible_suffix(&chunk.text, &mut overflow) else {
                continue;
            };
            col += canvas.put_text(col, text, chunk.highlight);
        }
    }
}

/// Complete owned description of a floating-window border.
struct Border {
    style: BorderStyle,
    adjustments: BorderAdjustments,
    title: Option<BorderText>,
    footer: Option<BorderText>,
}

impl Border {
    /// Copies and validates the C border description before rendering.
    ///
    /// # Safety
    ///
    /// `config` must contain pointers to eight initialized character arrays
    /// and eight attributes. A non-null `adjustments` must reference four
    /// initialized integers. Enabled title/footer vectors must satisfy the
    /// requirements of `BorderText::from_c`.
    unsafe fn from_c(
        config: &CGridBorderConfig,
        adjustments: *const libc::c_int,
        winbl: libc::c_int,
        normal: Highlight,
        glyph_map: &mut GlyphMapping,
    ) -> Option<Self> {
        if config.chars.is_null() || config.attrs.is_null() {
            return None;
        }
        // SAFETY: The caller guarantees both fixed-size border arrays.
        let chars =
            unsafe { std::slice::from_raw_parts(config.chars, BORDER_PART_COUNT * MAX_SCHAR_SIZE) };
        // SAFETY: The caller guarantees both fixed-size border arrays.
        let attrs = unsafe { std::slice::from_raw_parts(config.attrs, BORDER_PART_COUNT) };
        let style = BorderStyle::new(chars, attrs, normal, glyph_map)?;
        let adjustments = if adjustments.is_null() {
            BorderAdjustments::default()
        } else {
            // SAFETY: The caller guarantees four initialized adjustments.
            let values = unsafe { &*adjustments.cast::<[libc::c_int; 4]>() };
            values.into()
        };
        let title = if config.title != 0 {
            // SAFETY: The caller guarantees the enabled title vector and its
            // strings remain valid for this conversion.
            Some(unsafe {
                BorderText::from_c(
                    &config.title_chunks,
                    config.title_width,
                    config.title_pos,
                    Highlight(config.title_attr),
                    winbl,
                    normal,
                )?
            })
        } else {
            None
        };
        let footer = if config.footer != 0 {
            // SAFETY: The caller guarantees the enabled footer vector and
            // its strings remain valid for this conversion.
            Some(unsafe {
                BorderText::from_c(
                    &config.footer_chunks,
                    config.footer_width,
                    config.footer_pos,
                    Highlight(config.footer_attr),
                    winbl,
                    normal,
                )?
            })
        } else {
            None
        };

        Some(Self {
            style,
            adjustments,
            title,
            footer,
        })
    }

    /// Draws all enabled border edges and their title/footer text.
    fn draw(&self, canvas: &mut BorderCanvas) {
        let inner_rows = canvas.rows() - self.adjustments.top - self.adjustments.bottom;
        let inner_cols = canvas.cols() - self.adjustments.right - self.adjustments.left;

        if self.adjustments.top != 0 {
            canvas.start_line(0);
            if self.adjustments.left != 0 {
                canvas.put_cell(0, self.style.cell(BorderPart::TopLeft));
            }
            for col in 0..inner_cols {
                canvas.put_cell(
                    col + self.adjustments.left,
                    self.style.cell(BorderPart::Top),
                );
            }
            if let Some(title) = &self.title {
                title.draw(canvas, inner_cols);
            }
            if self.adjustments.right != 0 {
                canvas.put_cell(
                    inner_cols + self.adjustments.left,
                    self.style.cell(BorderPart::TopRight),
                );
            }
            canvas.flush();
        }

        for row in 0..inner_rows {
            if self.adjustments.left != 0 {
                canvas.start_line(row + self.adjustments.top);
                canvas.put_cell(0, self.style.cell(BorderPart::Left));
                canvas.flush();
            }
            if self.adjustments.right != 0 {
                let part = if row == 0
                    && self.adjustments.top == 0
                    && self.style.cell(BorderPart::TopRight).character != 0
                {
                    BorderPart::TopRight
                } else {
                    BorderPart::Right
                };
                canvas.start_line(row + self.adjustments.top);
                canvas.put_cell(inner_cols + self.adjustments.left, self.style.cell(part));
                canvas.flush();
            }
        }

        if self.adjustments.bottom != 0 {
            canvas.start_line(inner_rows + self.adjustments.top);
            if self.adjustments.left != 0 {
                canvas.put_cell(0, self.style.cell(BorderPart::BottomLeft));
            }
            for col in 0..inner_cols {
                let part = if col == 0
                    && self.adjustments.left == 0
                    && self.style.cell(BorderPart::BottomLeft).character != 0
                {
                    BorderPart::BottomLeft
                } else {
                    BorderPart::Bottom
                };
                canvas.put_cell(col + self.adjustments.left, self.style.cell(part));
            }
            if let Some(footer) = &self.footer {
                footer.draw(canvas, inner_cols);
            }
            if self.adjustments.right != 0 {
                canvas.put_cell(
                    inner_cols + self.adjustments.left,
                    self.style.cell(BorderPart::BottomRight),
                );
            }
            canvas.flush();
        }
    }
}

/// Safe drawing facade over the stateful line-grid renderer.
struct BorderCanvas<'a> {
    line_grid: LineGrid<'a>,
    line_buffer: &'a mut LineBuffer,
    output: Vec<OwnedRawLineCommand>,
}

impl BorderCanvas<'_> {
    /// Returns the number of grid rows available to the border.
    fn rows(&self) -> libc::c_int {
        self.line_grid.rows()
    }

    /// Returns the number of grid columns available to the border.
    fn cols(&self) -> libc::c_int {
        self.line_grid.cols()
    }

    /// Starts buffering updates for one absolute grid row.
    fn start_line(&mut self, row: libc::c_int) {
        self.line_grid.begin_grid(row, 0, self.line_buffer);
    }

    /// Places one border cell in the current buffered row.
    fn put_cell(&mut self, col: libc::c_int, cell: BorderCell) {
        self.line_grid
            .put_cell(col, cell.character, cell.highlight.0, self.line_buffer);
    }

    /// Places one text chunk and returns the number of occupied cells.
    fn put_text(&mut self, col: libc::c_int, text: &[u8], highlight: Highlight) -> libc::c_int {
        libc::c_int::try_from(
            self.line_grid
                .put_text(col, text, highlight.0, self.line_buffer),
        )
        .unwrap_or(libc::c_int::MAX)
    }

    /// Commits the current buffered row to the grid.
    fn flush(&mut self) {
        self.output.extend(self.line_grid.flush(self.line_buffer));
    }
}

/// Takes temporary ownership of a C-facing boxed slice.
///
/// The raw pointer remains available as a non-owning C view until the vector
/// is transferred back with `leak_nonempty_buffer`.
///
/// # Safety
///
/// A non-null `buffer` must point to exactly `len` initialized elements from a
/// boxed slice created by `leak_nonempty_buffer`. It must not be reclaimed,
/// reallocated, or used to create another owner while the vector exists.
unsafe fn take_c_buffer<T>(buffer: *mut T, len: usize) -> Vec<T> {
    if buffer.is_null() {
        Vec::new()
    } else {
        let slice = std::ptr::slice_from_raw_parts_mut(buffer, len);
        // SAFETY: The caller guarantees that this is the unique pointer to a
        // boxed slice with exactly `len` initialized elements.
        unsafe { Box::from_raw(slice).into_vec() }
    }
}

/// Transfers a populated Rust buffer to C while preserving null as the
/// representation of an unallocated buffer.
fn leak_nonempty_buffer<T>(buffer: Vec<T>) -> *mut T {
    if buffer.is_empty() {
        std::ptr::null_mut()
    } else {
        Box::into_raw(buffer.into_boxed_slice()).cast::<T>()
    }
}

/// The codepoints that participate in shaping a single screen cell.
///
/// An absent composing codepoint is represented as `None` instead of the C
/// implementation's NUL sentinel. A zero base represents an absent neighbor.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct CellCodepoints {
    /// The first codepoint in the cell.
    base: i32,
    /// The first composing codepoint, when one is present.
    composing: Option<i32>,
}

impl CellCodepoints {
    /// Returns whether the base is in the Arabic block shaped by Neovim.
    fn is_arabic(self) -> bool {
        self.base & 0xFF00 == 0x0600
    }

    /// Returns the size of these codepoints in Neovim's UTF-8 encoding.
    fn encoded_len(self) -> usize {
        let base_len = mbyte::EncodedCodepoint::new(self.base).as_bytes().len();
        let composing_len = self
            .composing
            .map(|codepoint| mbyte::EncodedCodepoint::new(codepoint).as_bytes().len())
            .unwrap_or_default();
        base_len + composing_len
    }
}

/// Stack-backed builder for a screen-cell glyph.
///
/// Its capacity excludes the trailing NUL that `GlyphBuff` adds, preventing
/// construction of a payload that cannot fit in one screen cell.
struct GlyphBuilder {
    bytes: [u8; MAX_SCHAR_SIZE - 1],
    len: usize,
}

impl GlyphBuilder {
    /// Creates an empty builder with the maximum screen-cell payload capacity.
    fn new() -> Self {
        Self {
            bytes: [0; MAX_SCHAR_SIZE - 1],
            len: 0,
        }
    }

    /// Returns the number of payload bytes that can still be appended.
    fn remaining(&self) -> usize {
        self.bytes.len() - self.len
    }

    /// Appends one codepoint using Neovim's UTF-8 encoding.
    fn push_codepoint(&mut self, codepoint: i32) {
        let encoded = mbyte::EncodedCodepoint::new(codepoint);
        self.extend_from_slice(encoded.as_bytes());
    }

    /// Appends bytes while enforcing the screen-cell size invariant.
    fn extend_from_slice(&mut self, bytes: &[u8]) {
        assert!(bytes.len() <= self.remaining());
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    /// Converts the initialized payload into a NUL-terminated glyph.
    fn finish(self) -> GlyphBuff {
        GlyphBuff::from_bytes(&self.bytes[..self.len])
    }
}

/// Glyph buffer to store bytes for the cell value of the `ScreenGrid`.
#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord)]
struct GlyphBuff {
    len: u8,
    buff: [u8; MAX_SCHAR_SIZE],
}

impl GlyphBuff {
    fn from_bytes(bytes: &[u8]) -> Self {
        assert!(bytes.len() < MAX_SCHAR_SIZE);
        assert!(!bytes.contains(&NULL_TERMINATOR));

        let mut buff = [NULL_TERMINATOR; MAX_SCHAR_SIZE];
        buff[..bytes.len()].copy_from_slice(bytes);
        Self {
            len: bytes.len() as u8,
            buff,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buff[..self.len as usize]
    }

    fn as_c_str(&self) -> &CStr {
        self.as_c_str_from(0)
    }

    /// Returns the NUL-terminated suffix beginning at `offset`.
    fn as_c_str_from(&self, offset: usize) -> &CStr {
        assert!(offset <= self.len as usize);
        CStr::from_bytes_with_nul(&self.buff[offset..=self.len as usize])
            .expect("glyph buffer must contain exactly one trailing NUL")
    }

    /// Decodes the base and first composing codepoint used for shaping.
    fn first_two_codepoints(&self) -> CellCodepoints {
        let base = mbyte::utf_ptr2char(self.as_c_str());
        if base == 0 {
            return CellCodepoints::default();
        }

        let base_len = mbyte::utf_ptr2len(self.as_c_str());
        let composing = match mbyte::utf_ptr2char(self.as_c_str_from(base_len)) {
            0 => None,
            codepoint => Some(codepoint),
        };
        CellCodepoints { base, composing }
    }

    /// Replaces the shaping codepoints while preserving remaining modifiers.
    ///
    /// If the shaped codepoints require more space, the final modifier is
    /// removed to retain the maximum screen-cell payload size.
    fn replace_first_two_codepoints(
        &self,
        current: CellCodepoints,
        replacement: CellCodepoints,
    ) -> Self {
        let rest_start = current.encoded_len().min(self.as_bytes().len());
        let mut rest = &self.as_bytes()[rest_start..];

        let mut shaped = GlyphBuilder::new();
        shaped.push_codepoint(replacement.base);
        if let Some(composing) = replacement.composing {
            shaped.push_codepoint(composing);
        }
        if rest.len() > shaped.remaining() {
            // Shaping can increase the base character's byte length. Discarding
            // the final codepoint preserves the screen-cell size invariant.
            rest = Self::without_last_codepoint(rest);
        }
        shaped.extend_from_slice(rest);
        shaped.finish()
    }

    /// Removes the final codepoint using Neovim's extended UTF-8 boundaries.
    ///
    /// A malformed or incomplete final sequence is treated as one byte,
    /// matching `utf_cp_bounds` in the previous C implementation.
    fn without_last_codepoint(bytes: &[u8]) -> &[u8] {
        let Some(last) = bytes.len().checked_sub(1) else {
            return bytes;
        };
        if bytes[last] < 0x80 {
            return &bytes[..last];
        }

        let earliest = last.saturating_sub(5);
        let mut first = last;
        while bytes[first] & 0xC0 == 0x80 {
            if first == earliest {
                return &bytes[..last];
            }
            first -= 1;
        }

        let sequence_len = match bytes[first] {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            0xF8..=0xFB => 5,
            0xFC..=0xFD => 6,
            _ => 1,
        };
        if sequence_len == last - first + 1 {
            &bytes[..first]
        } else {
            &bytes[..last]
        }
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
    /// Interned glyphs in the order in which they were inserted.
    ///
    /// A glyph's position is the index encoded in its `VimSCharT` reference.
    glyphs: Vec<GlyphBuff>,
    /// Finds an existing encoded reference without scanning `glyphs`.
    index: BTreeMap<GlyphBuff, VimSCharT>,
}

impl GlyphMapping {
    /// Number of cached glyphs after which screen users must clear the cache.
    const CLEAR_THRESHOLD: usize = 1 << 21;

    /// Constructs a new empty `GlyphMapping`
    const fn new() -> Self {
        Self {
            glyphs: Vec::new(),
            index: BTreeMap::new(),
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
        let glyph = GlyphBuff::from_bytes(val);
        if let Some(&mapping) = self.index.get(&glyph) {
            return mapping;
        }

        let glyph_index =
            VimSCharT::try_from(self.glyphs.len()).expect("glyph index must fit in VimSCharT");
        let new_mapping = Self::to_schart(glyph_index);
        assert!(new_mapping < VimSCharT::MAX);

        self.glyphs.push(glyph.clone());
        self.index.insert(glyph, new_mapping);
        new_mapping
    }

    /// Returns the bytes from the mapping schart -> bytes
    fn get_bytes(&self, c: VimSCharT) -> Option<&GlyphBuff> {
        let index = usize::try_from(Self::extract_index(c)).ok()?;
        self.glyphs.get(index)
    }

    /// Resolves an inline screen character or a glyph-cache index to its
    /// NUL-terminated byte representation.
    fn resolve(&self, c: VimSCharT) -> Cow<'_, GlyphBuff> {
        if Self::is_glyph_mapping_schart(c) {
            self.get_bytes(c)
                .map(Cow::Borrowed)
                .unwrap_or_else(|| Cow::Owned(GlyphBuff::from_bytes(&[])))
        } else {
            let bytes = c.to_ne_bytes();
            let len = bytes
                .iter()
                .position(|byte| *byte == NULL_TERMINATOR)
                .unwrap_or(bytes.len());
            Cow::Owned(GlyphBuff::from_bytes(&bytes[..len]))
        }
    }

    /// Resolves a screen character and decodes its shaping codepoints.
    fn codepoints(&self, c: VimSCharT) -> CellCodepoints {
        self.resolve(c).first_two_codepoints()
    }

    /// Returns the length of the bytes from the mapping schart -> bytes
    fn get_len(&self, c: VimSCharT) -> Option<usize> {
        self.get_bytes(c).map(|glyph| glyph.len as usize)
    }

    /// Clears the mapping unless it has reached the cache-clear threshold.
    fn clear_if_full(&mut self) {
        if self.is_full() {
            return;
        }
        self.clear();
    }

    /// Checks if the mapping is full. Technically the mapping is full when
    /// we stored 2**24 - 1 values. However we check for 2**21
    fn is_full(&self) -> bool {
        Self::has_reached_clear_threshold(self.glyphs.len())
    }

    /// Checks the cache-clear threshold without requiring an allocated cache.
    const fn has_reached_clear_threshold(glyph_count: usize) -> bool {
        glyph_count >= Self::CLEAR_THRESHOLD
    }

    /// Clears the contents of the mapping
    fn clear(&mut self) {
        self.glyphs.clear();
        self.index.clear();
    }
}

/// A screen cell whose character can participate in Arabic shaping.
trait ArabicShapeCell {
    fn character(&self) -> VimSCharT;
    fn set_character(&mut self, character: VimSCharT);
}

impl ArabicShapeCell for VimSCharT {
    fn character(&self) -> VimSCharT {
        *self
    }

    fn set_character(&mut self, character: VimSCharT) {
        *self = character;
    }
}

impl ArabicShapeCell for GridCell {
    fn character(&self) -> VimSCharT {
        self.character
    }

    fn set_character(&mut self, character: VimSCharT) {
        self.character = character;
    }
}

/// Applies Arabic presentation shaping to a sequence of screen cells.
///
/// Glyph lookup and reinterning remain internal, so callers provide only a
/// safe mutable slice of screen characters.
struct ArabicShaper<'a> {
    glyph_map: &'a mut GlyphMapping,
}

impl<'a> ArabicShaper<'a> {
    /// Creates a shaper backed by the screen's glyph mapping.
    fn new(glyph_map: &'a mut GlyphMapping) -> Self {
        Self { glyph_map }
    }

    /// Shapes all Arabic cells in place, leaving other cells unchanged.
    fn shape<Cell: ArabicShapeCell>(&mut self, cells: &mut [Cell]) {
        let Some(start) = cells
            .iter()
            .position(|cell| schar_in_arabic_block_impl(cell.character(), self.glyph_map))
        else {
            return;
        };

        let mut before = 0;
        let mut current = self.glyph_map.codepoints(cells[start].character());
        for index in start..cells.len() {
            let after = cells
                .get(index + 1)
                .map(|cell| self.glyph_map.codepoints(cell.character()))
                .unwrap_or_default();

            self.shape_cell(&mut cells[index], current, after, before);
            before = current.base;
            current = after;
        }
    }

    /// Shapes and reinterns one cell when its base is Arabic.
    fn shape_cell<Cell: ArabicShapeCell>(
        &mut self,
        cell: &mut Cell,
        current: CellCodepoints,
        after: CellCodepoints,
        before: i32,
    ) {
        if !current.is_arabic() {
            return;
        }

        let replacement = Self::shape_codepoints(current, after, before);
        if replacement == current {
            return;
        }

        let glyph = self.glyph_map.resolve(cell.character());
        let shaped = glyph.replace_first_two_codepoints(current, replacement);
        cell.set_character(schar_from_bytes(shaped.as_bytes(), self.glyph_map));
    }

    /// Calls the remaining C shaping primitive for one cell.
    ///
    /// Screen order is the reverse of the logical joining order expected by
    /// `arabic_shape`, so `after` is passed as its previous character and
    /// `before` as its next character.
    fn shape_codepoints(
        current: CellCodepoints,
        after: CellCodepoints,
        before: i32,
    ) -> CellCodepoints {
        let mut composing = current.composing.unwrap_or_default();
        // SAFETY: `composing` is a valid writable integer for the duration of
        // the call; the remaining arguments are passed by value.
        let base = unsafe {
            arabic_shape_ffi(
                current.base,
                &raw mut composing,
                after.base,
                after.composing.unwrap_or_default(),
                before,
            )
        };
        CellCodepoints {
            base,
            composing: (composing != 0).then_some(composing),
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn alloc_initial_line_buffer() {
    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.realloc(unsafe { globals::Columns as usize });
}

/// Reports whether a C screen grid currently owns cell buffers.
///
/// # Safety
///
/// `grid` must be null or point to a valid, readable `CScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_is_allocated(grid: *const CScreenGrid) -> bool {
    let Some(cgrid) = std::ptr::NonNull::new(grid.cast_mut()) else {
        return false;
    };
    if let Some(grid_id) = permanent_grid_id(cgrid) {
        return StoredGridGuard::acquire(grid_id)
            .expect("permanent C grid identity must resolve to its Rust grid")
            .is_allocated();
    }

    // SAFETY: The caller guarantees that this legacy C grid is readable.
    unsafe { cgrid.as_ref() }.is_allocated()
}

/// Reports whether the current screen update must invalidate composed output.
///
/// This combines the requested full-clear state with the default grid's redraw
/// state without exposing that state to C.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Its
/// buffers must satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_requires_compositor_invalidation(
    grid: *mut CScreenGrid,
    clear_requested: bool,
) -> bool {
    if grid.is_null() {
        return true;
    }

    // SAFETY: The caller transfers exclusive ownership of the grid buffers to
    // the guard for the duration of this operation.
    let grid = unsafe { ScreenGridGuard::take(grid) };
    grid.requires_compositor_invalidation(clear_requested)
}

/// Revalidates invalid grid contents in preparation for a complete redraw.
///
/// A grid that does not require a redraw remains unchanged.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Its
/// buffers must satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_revalidate_for_redraw(grid: *mut CScreenGrid) {
    if grid.is_null() {
        return;
    }

    // SAFETY: The caller transfers exclusive ownership of the grid buffers to
    // the guard for the duration of this operation.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    grid.revalidate_for_redraw();
}

/// Records that a grid requires a complete redraw.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Its
/// buffers must satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_require_full_redraw(grid: *mut CScreenGrid) {
    if grid.is_null() {
        return;
    }

    // SAFETY: The caller transfers exclusive ownership of the grid buffers to
    // the guard for the duration of this operation.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    grid.require_full_redraw();
}

/// Reports whether a grid view requires a complete redraw.
///
/// A view without a target requires redraw because it cannot currently render
/// a cursor.
///
/// # Safety
///
/// `view` must be null or point to a valid `CGridView`. Its target, when
/// non-null, must be a valid, exclusively accessible `CScreenGrid` whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_view_requires_redraw(view: *const CGridView) -> bool {
    if view.is_null() {
        return true;
    }

    // SAFETY: The caller guarantees that `view` points to a valid `CGridView`.
    let view = unsafe { &*view };
    if view.target.is_null() {
        return true;
    }
    let offset = GridPosition::new(view.row_offset, view.col_offset);

    // SAFETY: The caller transfers exclusive ownership of the target grid's
    // buffers to the guard for the duration of this operation.
    let target = unsafe { ScreenGridGuard::take(view.target) };
    GridView::new(&target, offset).requires_redraw()
}

/// Reports whether a grid view's target has `width` columns.
///
/// # Safety
///
/// `view` must be null or point to a valid `CGridView`. Its target, when
/// non-null, must be a valid, exclusively accessible `CScreenGrid` whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_view_width_matches_target(
    view: *const CGridView,
    width: libc::c_int,
) -> bool {
    if view.is_null() {
        return false;
    }

    // SAFETY: The caller guarantees that `view` points to a valid `CGridView`.
    let view = unsafe { &*view };
    if view.target.is_null() {
        return false;
    }
    let offset = GridPosition::new(view.row_offset, view.col_offset);

    // SAFETY: The caller transfers exclusive ownership of the target grid's
    // buffers to the guard for the duration of this operation.
    let target = unsafe { ScreenGridGuard::take(view.target) };
    GridView::new(&target, offset).width_matches_target(width)
}

/// Moves the UI cursor through a translated grid view.
///
/// Grid ownership is restored before the cursor command is emitted.
///
/// # Safety
///
/// `view` must be null or point to a valid `CGridView`. Its target, when
/// non-null, must be a valid, exclusively accessible `CScreenGrid` whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_view_cursor_goto(
    view: *const CGridView,
    row: libc::c_int,
    column: libc::c_int,
) {
    if view.is_null() {
        return;
    }

    // SAFETY: The caller guarantees that `view` points to a valid `CGridView`.
    let view = unsafe { &*view };
    if view.target.is_null() {
        return;
    }
    let offset = GridPosition::new(view.row_offset, view.col_offset);
    let output = {
        // SAFETY: The caller transfers exclusive ownership of the target
        // grid's buffers to the guard for the duration of this operation.
        let target = unsafe { ScreenGridGuard::take(view.target) };
        GridView::new(&target, offset).plan_cursor_move(GridPosition::new(row, column))
    };
    if let Some((target, command)) = output {
        target.emit(command);
    }
}

/// Refreshes the external representation of an allocated message grid.
///
/// The resize command and position callback run after grid ownership has been
/// restored to the C representation.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Its
/// buffers must satisfy the ownership and layout requirements of `ScreenGrid`.
/// `emit_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_refresh_message_ui(
    grid: *mut CScreenGrid,
    position: libc::c_int,
    scrolled: bool,
    emit_position: CMessageGridPositionEmitter,
) {
    if grid.is_null() {
        return;
    }

    let resize = {
        // SAFETY: The caller transfers exclusive ownership of the grid buffers
        // to the guard for the duration of this operation.
        let grid = unsafe { ScreenGridGuard::take(grid) };
        grid.resize_command_if_allocated()
    };
    let Some((target, command)) = resize else {
        return;
    };

    target.emit(command);
    // SAFETY: The caller guarantees that the callback is valid. Grid ownership
    // was restored before this callback can reenter editor code.
    unsafe {
        emit_position(position, scrolled);
    }
}

/// Scrolls lines within a window grid view.
///
/// # Safety
///
/// `view` must point to a valid `CGridView`. Its target must point to a valid,
/// exclusively accessible `CScreenGrid` whose buffers satisfy the ownership
/// and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_scroll_window_lines(
    view: *const CGridView,
    row: libc::c_int,
    line_count: libc::c_int,
    view_height: libc::c_int,
    view_width: libc::c_int,
) {
    if view.is_null() {
        return;
    }
    let Some(view_size) = GridSize::try_new(view_height, view_width) else {
        return;
    };

    // SAFETY: The caller guarantees that `view` points to a valid `CGridView`.
    let view = unsafe { &*view };
    if view.target.is_null() {
        return;
    }
    let request = WindowLineScrollRequest {
        row,
        line_count,
        view_origin: GridPosition::new(view.row_offset, view.col_offset),
        view_size,
    };

    let (target, command) = {
        // SAFETY: The caller transfers exclusive ownership of the target grid
        // buffers to the guard for the duration of this operation.
        let mut grid = unsafe { ScreenGridGuard::take(view.target) };
        let target = GridOutputTarget::new(grid.id(), grid.cols);
        let command = grid.scroll_window_lines(request);
        (target, command)
    };
    if let Some(command) = command {
        target.emit(command);
    }
}

/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Any
/// existing buffers and dimensions must satisfy the ownership and layout
/// requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_alloc(
    grid: *mut CScreenGrid,
    rows: libc::c_int,
    cols: libc::c_int,
    copy: bool,
    valid: bool,
) {
    let mut line_buffer = LineBufferGuard::acquire();
    // SAFETY: The caller guarantees exclusive ownership of the C grid and its
    // existing allocations.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    let resized_grid = ScreenGrid::from_grid(&grid, rows, cols, valid, copy, &mut line_buffer);
    *grid = resized_grid;
}

/// Assigns the reserved identity to the default screen grid.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Any
/// existing buffers and dimensions must satisfy the ownership and layout
/// requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_initialize_default(grid: *mut CScreenGrid) {
    if grid.is_null() {
        return;
    }

    // SAFETY: The caller transfers exclusive ownership of the C grid and its
    // allocations to the guard for the duration of this function.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    grid.initialize_as_default();
}

/// Resizes the default screen grid when its configured dimensions changed.
///
/// Returns whether a resize was performed.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Any
/// existing buffers and dimensions must satisfy the ownership and layout
/// requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_configure_default(
    grid: *mut CScreenGrid,
    rows: libc::c_int,
    columns: libc::c_int,
) -> bool {
    if grid.is_null() {
        return false;
    }
    let Some(size) = GridSize::try_new(rows, columns) else {
        return false;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    // SAFETY: The caller transfers exclusive ownership of the C grid and its
    // allocations to the guard for the duration of this function.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    grid.configure_as_default(size, &mut line_buffer) == GridLifecycleChange::Resized
}

/// Applies the complete resize or invalidation transition for the popup-menu
/// grid.
///
/// Returns whether storage was allocated or resized.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Any
/// existing buffers and dimensions must satisfy the ownership and layout
/// requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_configure_for_popup_menu(
    grid: *mut CScreenGrid,
    rows: libc::c_int,
    columns: libc::c_int,
    contents_invalid: bool,
) -> bool {
    if grid.is_null() {
        return false;
    }
    let Some(size) = GridSize::try_new(rows, columns) else {
        return false;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    // SAFETY: The caller transfers exclusive ownership of the C grid and its
    // allocations to the guard for the duration of this function.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    grid.configure_for_popup_menu(size, contents_invalid, &mut line_buffer)
        == GridLifecycleChange::Resized
}

/// Prepares the message grid for its requested visibility and size.
///
/// A `ReleaseRequired` result leaves storage intact so C can remove the
/// compositor layer before calling `grid_release_message`.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Any
/// existing buffers and dimensions must satisfy the ownership and layout
/// requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_prepare_for_messages(
    grid: *mut CScreenGrid,
    rows: libc::c_int,
    columns: libc::c_int,
    should_allocate: bool,
    scrolled_lines: libc::c_int,
) -> CMessageGridTransition {
    if grid.is_null() {
        return CMessageGridTransition::Unchanged;
    }
    let mode = if should_allocate {
        let Some(size) = GridSize::try_new(rows, columns) else {
            return CMessageGridTransition::Unchanged;
        };
        MessageGridMode::Visible(size)
    } else {
        MessageGridMode::Hidden
    };

    let transition = {
        let mut line_buffer = LineBufferGuard::acquire();
        // SAFETY: The caller transfers exclusive ownership of the C grid and
        // its allocations to the guard for the duration of this function.
        let mut grid = unsafe { ScreenGridGuard::take(grid) };
        grid.prepare_for_messages(mode, &GRID_HANDLE_ALLOCATOR, &mut line_buffer)
    };
    if transition == MessageGridTransition::Resized {
        lock_message_scroll_state().record_resize(scrolled_lines);
    }
    transition.into()
}

/// Releases message-grid storage after compositor removal.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Any
/// existing buffers and dimensions must satisfy the ownership and layout
/// requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_release_message(grid: *mut CScreenGrid) {
    if grid.is_null() {
        return;
    }

    // SAFETY: The caller transfers exclusive ownership of the C grid and its
    // allocations to the guard for the duration of this function.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    grid.release_message();
}

/// Updates the message-grid position and its legacy adjusted view.
///
/// The position callback and view mutation occur only after the grid guard has
/// restored ownership to the C representation.
///
/// Returns whether the position was published immediately.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Its
/// buffers must satisfy the ownership and layout requirements of `ScreenGrid`.
/// `view` must point to a valid, exclusively accessible `CGridView`.
/// `emit_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_set_message_position(
    grid: *mut CScreenGrid,
    view: *mut CGridView,
    row: libc::c_int,
    scrolled: bool,
    emit_position: CMessageGridPositionEmitter,
) {
    if grid.is_null() || view.is_null() {
        return;
    }
    let update = {
        // SAFETY: The caller transfers exclusive ownership of the grid and
        // its allocations to the guard for the duration of this operation.
        let grid = unsafe { ScreenGridGuard::take(grid) };
        grid.update_message_position(row)
    };

    if update.publish {
        // SAFETY: The caller guarantees that the callback is valid. The grid
        // guard was dropped before this callback can reenter editor code.
        unsafe {
            emit_position(row, scrolled);
        }
        lock_message_scroll_state().record_position(row);
    }
    if let Some(row_offset) = update.view_row_offset {
        // SAFETY: The caller guarantees exclusive access to a valid view.
        unsafe {
            (*view).row_offset = row_offset;
        }
    }
}

/// Scrolls the message grid upward by one row.
///
/// Position callbacks and scroll events are emitted only after the grid guard
/// has restored ownership to the C representation.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Its
/// buffers must satisfy the ownership and layout requirements of `ScreenGrid`.
/// `set_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_scroll_message_up(
    grid: *mut CScreenGrid,
    current_position: libc::c_int,
    enable_throttling: bool,
    zero_cmdheight: bool,
    discount_first_scroll: bool,
    set_position: CMessageGridPositionEmitter,
) {
    if grid.is_null() {
        return;
    }
    let scroll_up = {
        // SAFETY: The caller transfers exclusive ownership of the grid and
        // its allocations to the guard for the duration of this operation.
        let mut grid = unsafe { ScreenGridGuard::take(grid) };
        grid.scroll_message_up(current_position, enable_throttling, zero_cmdheight)
    };
    let MessageGridScrollUp {
        started_throttling,
        action,
    } = scroll_up;

    if started_throttling && discount_first_scroll {
        lock_message_scroll_state().discount_first_scroll();
    }
    match action {
        MessageGridScrollUpAction::Reposition {
            position,
            scrolled,
            clear_first_row,
        } => {
            // SAFETY: The caller guarantees that the callback is valid. The
            // grid guard was dropped before this callback can reenter editor
            // code.
            unsafe {
                set_position(position, scrolled);
            }
            if clear_first_row {
                // SAFETY: The callback has returned, and the caller guarantees
                // continued exclusive access to this grid.
                let mut grid = unsafe { ScreenGridGuard::take(grid) };
                grid.clear_first_message_row();
            }
        }
        MessageGridScrollUpAction::ShiftRows { target, scroll } => {
            if let Some(scroll) = scroll {
                target.emit(scroll);
            }
        }
    }
}

/// Clears message-grid rows in preparation for a screen redraw.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Its
/// buffers must satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_prepare_message_redraw(
    grid: *mut CScreenGrid,
    rows_to_clear: libc::c_int,
    valid_rows: libc::c_int,
) {
    if grid.is_null() {
        return;
    }
    let (Ok(rows_to_clear), Ok(valid_rows)) =
        (usize::try_from(rows_to_clear), usize::try_from(valid_rows))
    else {
        return;
    };

    // SAFETY: The caller transfers exclusive ownership of the grid and its
    // allocations to the guard within this scope.
    {
        let mut grid = unsafe { ScreenGridGuard::take(grid) };
        grid.prepare_message_redraw(rows_to_clear, valid_rows);
    }
    lock_message_scroll_state().reset_progress();
}

/// Ends message scrolling, publishes its resting position, and clears hidden
/// message rows.
///
/// The position callback runs after throttling is disabled and grid ownership
/// is restored, but before hidden rows are cleared. This preserves the
/// original event order.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Its
/// buffers must satisfy the ownership and layout requirements of `ScreenGrid`.
/// `set_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_reset_message_scroll(
    grid: *mut CScreenGrid,
    position: libc::c_int,
    rows_to_clear: libc::c_int,
    set_position: CMessageGridPositionEmitter,
) {
    if grid.is_null() {
        return;
    }
    let Ok(rows_to_clear) = usize::try_from(rows_to_clear) else {
        return;
    };

    // SAFETY: The caller transfers exclusive ownership of the grid and its
    // allocations within each guard scope.
    {
        let mut grid = unsafe { ScreenGridGuard::take(grid) };
        grid.disable_message_throttling();
    }
    unsafe {
        set_position(position, false);
    }
    {
        let mut grid = unsafe { ScreenGridGuard::take(grid) };
        grid.clear_message_rows(rows_to_clear, 0);
    }
    lock_message_scroll_state().reset_progress();
}

/// Flushes pending throttled message-grid scrolling and row updates.
///
/// The message-position callback and UI commands are emitted only after the
/// grid guard has restored ownership to the C representation.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Its
/// buffers must satisfy the ownership and layout requirements of `ScreenGrid`.
/// `emit_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_flush_message_scroll(
    grid: *mut CScreenGrid,
    request: CMessageGridScrollFlushRequest,
    emit_position: CMessageGridPositionEmitter,
) {
    if grid.is_null() {
        return;
    }
    let Some(screen_size) = GridSize::try_new(request.screen_rows, request.screen_columns) else {
        return;
    };
    let request = MessageGridScrollFlushRequest {
        position: request.position,
        scrolled_lines: request.scrolled_lines,
        screen_size,
        clear_attribute: request.clear_attribute,
    };
    let state = *lock_message_scroll_state();
    let plan = {
        // SAFETY: The caller transfers exclusive ownership of the grid and
        // its allocations to the guard for the duration of this operation.
        let mut grid = unsafe { ScreenGridGuard::take(grid) };
        grid.flush_message_scroll(request, state)
    };

    let MessageGridScrollFlushPlan {
        target,
        position_update,
        scroll,
        lines,
    } = plan;
    if let Some(row) = position_update {
        // SAFETY: The caller guarantees that the callback is valid. The grid
        // guard was dropped before this callback can reenter editor code.
        unsafe {
            emit_position(row, true);
        }
    }
    if let Some(scroll) = scroll {
        target.emit(scroll);
    }
    target.emit_lines(lines);
    lock_message_scroll_state().finish_flush(request.position, request.scrolled_lines);
}

/// Applies the complete allocation and validity transition for one window
/// grid and updates its C-facing view.
///
/// Returns the configured grid identity and whether dedicated storage was
/// allocated or resized. The window subsystem uses that result for border
/// redraw and UI resize notification.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Any
/// existing buffers and dimensions must satisfy the ownership and layout
/// requirements of `ScreenGrid`. `view` must point to a valid, exclusively
/// writable `CGridView`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_configure_for_window(
    grid: *mut CScreenGrid,
    view: *mut CGridView,
    rows: libc::c_int,
    columns: libc::c_int,
    use_dedicated_grid: bool,
    row_offset: libc::c_int,
    col_offset: libc::c_int,
) -> CWindowGridUpdate {
    if grid.is_null() || view.is_null() {
        return CWindowGridUpdate {
            grid_handle: 0,
            resized: false,
        };
    }
    let mode = if use_dedicated_grid {
        let Some(size) = GridSize::try_new(rows, columns) else {
            return CWindowGridUpdate {
                grid_handle: 0,
                resized: false,
            };
        };
        WindowGridMode::Dedicated(size)
    } else {
        WindowGridMode::Shared
    };

    let update = {
        let mut line_buffer = LineBufferGuard::acquire();
        // SAFETY: The caller transfers exclusive ownership of the C grid and
        // its allocations to the guard for the duration of this operation.
        let mut rust_grid = unsafe { ScreenGridGuard::take(grid) };
        rust_grid.configure_window_view(
            mode,
            GridPosition::new(row_offset, col_offset),
            &mut line_buffer,
        )
    };

    // SAFETY: The caller guarantees exclusive writable access to `view`.
    unsafe {
        (*view).target = match update.view.target {
            WindowGridViewTarget::Dedicated => grid,
            WindowGridViewTarget::Shared => &raw mut default_grid,
        };
        (*view).row_offset = update.view.offset.row;
        (*view).col_offset = update.view.offset.column;
    }

    update.into()
}

/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid`. Its
/// non-null buffer pointers must own compatible, unreclaimed Rust allocations.
/// It may instead be an opaque identity registered with the permanent store.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_free(grid: *mut CScreenGrid) {
    // SAFETY: The caller guarantees exclusive access to this grid identity.
    if unsafe { ScreenGridGuard::release_permanent(grid) } {
        return;
    }

    // SAFETY: The caller transfers ownership of every grid allocation.
    let _owned_grid = unsafe { ScreenGrid::take_from_cgrid(&mut *grid) };

    // SAFETY: The allocations are now owned by `_owned_grid`; clear the
    // non-owning C views before `_owned_grid` releases them.
    unsafe {
        (*grid).chars = std::ptr::null_mut();
        (*grid).attrs = std::ptr::null_mut();
        (*grid).vcols = std::ptr::null_mut();
        (*grid).line_offset = std::ptr::null_mut();
        (*grid).dirty_col = std::ptr::null_mut();
    }
}

/// Releases all process-global grid resources during editor shutdown.
///
/// # Safety
///
/// This must only be called by `free_all_mem` on Neovim's main thread, after
/// all other users of the grids, line buffer, and glyph mapping have stopped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_free_all_mem() {
    // SAFETY: The caller guarantees exclusive shutdown-time access to both
    // global grids and their Rust-owned allocations.
    unsafe {
        grid_free(&raw mut default_grid);
        grid_free(&raw mut msg_grid);
    }

    *lock_message_scroll_state() = MessageScrollState::new();

    // The caller guarantees that no line-buffer user remains.
    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.release();
    drop(line_buffer);

    // Moving the mapping out of the mutex releases all of its allocations.
    let glyph_mapping = {
        let mut mapping = lock_glyph_mapping();
        std::mem::replace(&mut *mapping, GlyphMapping::new())
    };
    drop(glyph_mapping);
}

/// Clears every allocated logical row.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid` whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_clear_all(grid: *mut CScreenGrid, valid: bool) -> bool {
    if grid.is_null() {
        return false;
    }

    // SAFETY: The caller transfers exclusive ownership of the grid buffers to
    // the guard.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    grid.clear_all(valid)
}

/// Clears a half-open range of complete logical rows.
///
/// Rows beyond the allocated grid are ignored.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid` whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_clear_rows(
    grid: *mut CScreenGrid,
    start_row: libc::c_int,
    end_row: libc::c_int,
    valid: bool,
) {
    if grid.is_null() {
        return;
    }
    let (Ok(start_row), Ok(end_row)) = (usize::try_from(start_row), usize::try_from(end_row))
    else {
        return;
    };

    // SAFETY: The caller transfers exclusive ownership of the grid buffers to
    // the guard.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    grid.clear_rows(start_row..end_row, valid);
}

/// @param buf need not be NUL terminated, but may not contain embedded NULs.
///
/// caller must ensure len < MAX_SCHAR_SIZE (not =, as NUL needs a byte)
#[unsafe(no_mangle)]
pub extern "C" fn schar_from_buf(buf: *const libc::c_char, len: libc::size_t) -> VimSCharT {
    assert!(len < MAX_SCHAR_SIZE);
    let buf = buf.cast::<u8>();
    let s = if buf.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(buf, len) }
    };
    let mut glyph_mapping = lock_glyph_mapping();
    schar_from_bytes(s, &mut glyph_mapping)
}

/// # Safety
///
/// `buf` must be null or point to a valid NUL-terminated string that remains
/// readable for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn schar_from_str(buf: *const libc::c_char) -> VimSCharT {
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
    let is_full = lock_glyph_mapping().is_full();
    if is_full {
        schar_cache_clear();
        return true;
    }
    false
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_cache_clear() {
    unsafe { decor_check_invalid_glyphs() };
    lock_glyph_mapping().clear();
    if unsafe { check_chars_options() } {
        panic!("Erroc checking chars options");
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_high(sc: VimSCharT) -> bool {
    GlyphMapping::is_glyph_mapping_schart(sc)
}

/// advance buf_out. do NOT set final NUL
///
/// # Safety
///
/// `buf_out` must point to a valid writable pointer. That pointer must
/// reference writable storage for the bytes represented by `sc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn schar_get_adv(
    buf_out: *mut *mut libc::c_char,
    sc: VimSCharT,
) -> libc::size_t {
    let glyph_mapping = lock_glyph_mapping();
    let glyph = glyph_mapping.resolve(sc);
    let bytes = glyph.as_bytes();
    let len = bytes.len();
    let slice = unsafe { std::slice::from_raw_parts_mut((*buf_out).cast::<u8>(), len) };
    slice.copy_from_slice(bytes);
    unsafe {
        *buf_out = (*buf_out).add(len);
    }
    len
}

/// sets final NUL
///
/// # Safety
///
/// `buf_out` must point to writable storage for the bytes represented by `sc`
/// and a trailing NUL byte.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn schar_get(mut buf_out: *mut libc::c_char, sc: VimSCharT) -> libc::size_t {
    unsafe {
        let len = schar_get_adv(&raw mut buf_out, sc);
        *buf_out = NULL_TERMINATOR as libc::c_char;
        len
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_cells(sc: VimSCharT) -> libc::c_int {
    let glyph_mapping = lock_glyph_mapping();
    schar_cells_impl(sc, &glyph_mapping)
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_get_first_codepoint(sc: VimSCharT) -> libc::c_int {
    let glyph_mapping = lock_glyph_mapping();
    schar_get_first_codepoint_impl(sc, &glyph_mapping)
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_len(sc: VimSCharT) -> libc::size_t {
    let glyph_mapping = lock_glyph_mapping();
    schar_len_impl(sc, &glyph_mapping)
}

#[unsafe(no_mangle)]
pub extern "C" fn schar_in_arabic_block(sc: VimSCharT) -> bool {
    let glyph_mapping = lock_glyph_mapping();
    schar_in_arabic_block_impl(sc, &glyph_mapping)
}

/// Shape `cols` screen characters for Arabic display.
///
/// # Safety
///
/// For a positive `cols`, `buf` must point to `cols` initialized screen
/// characters that are exclusively writable for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn line_do_arabic_shape(buf: *mut VimSCharT, cols: libc::c_int) {
    if buf.is_null() || cols <= 0 {
        return;
    }

    // SAFETY: The caller guarantees that `buf` references `cols` initialized,
    // exclusively writable elements. `cols` was checked to be positive.
    let buf = unsafe { std::slice::from_raw_parts_mut(buf, cols as usize) };
    let mut glyph_mapping = lock_glyph_mapping();
    ArabicShaper::new(&mut glyph_mapping).shape(buf);
}

/// @return ascii char or NULL_TERMINATOR if not ascii
#[unsafe(no_mangle)]
pub extern "C" fn schar_get_ascii(sc: VimSCharT) -> libc::c_char {
    schar_to_ascii(sc).unwrap_or(NULL_TERMINATOR) as libc::c_char
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
///
/// # Safety
///
/// `grid` must point to a valid `CGridView`. `row_offset` and `col_offset`
/// must point to valid, properly aligned writable integers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_adjust(
    grid: *const CGridView,
    row_offset: *mut libc::c_int,
    col_offset: *mut libc::c_int,
) -> *mut CScreenGrid {
    unsafe {
        assert!(!grid.is_null(), "view grid is nullptr");

        *row_offset += (*grid).row_offset;
        *col_offset += (*grid).col_offset;

        (*grid).target
    }
}

/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid` whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_invalidate(grid: *mut CScreenGrid) {
    // SAFETY: The caller transfers exclusive ownership of the grid buffers to
    // the guard.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    grid.invalidate();
}

/// Marks a logical grid row as invalid.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid` whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_invalidate_row(grid: *mut CScreenGrid, row: libc::c_int) {
    if grid.is_null() {
        return;
    }
    let Ok(row) = usize::try_from(row) else {
        return;
    };

    // SAFETY: The caller transfers exclusive ownership of the grid buffers to
    // the guard.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    grid.invalidate_row(row);
}

/// Reads one cell through a grid view after validating its target identity.
///
/// Returns `false` when the view has no target, its target does not match
/// `expected_grid`, coordinate translation overflows, or the translated
/// position does not identify an allocated cell.
///
/// # Safety
///
/// `view` must point to a valid `CGridView` whose target, when non-null, is a
/// valid, exclusively accessible `CScreenGrid`. The target's buffers must
/// satisfy the ownership and layout requirements of `ScreenGrid`. `output`
/// must point to valid, properly aligned writable `CGridCell` storage that
/// does not alias the target grid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_view_get_cell(
    view: *const CGridView,
    expected_grid: VimHandleT,
    row: libc::c_int,
    column: libc::c_int,
    output: *mut CGridCell,
) -> bool {
    if view.is_null() || output.is_null() {
        return false;
    }

    // SAFETY: The caller guarantees that `view` points to a valid `CGridView`.
    let view = unsafe { &*view };
    if view.target.is_null() {
        return false;
    }
    let offset = GridPosition::new(view.row_offset, view.col_offset);

    // SAFETY: The caller transfers exclusive ownership of the target grid's
    // buffers into the guard for the duration of this operation.
    let target = unsafe { ScreenGridGuard::take(view.target) };
    let view = GridView::new(&target, offset);
    let Some(cell) = view.cell_for_grid(GridId::new(expected_grid), GridPosition::new(row, column))
    else {
        return false;
    };

    // SAFETY: The output pointer was checked for null above and the caller
    // guarantees valid writable storage.
    unsafe {
        output.write(CGridCell::from(cell));
    }
    true
}

/// Selects the requested API grid and reads one complete logical cell.
///
/// An invalid window-grid identity is distinct from a valid grid whose
/// coordinates do not identify a cell.
///
/// # Safety
///
/// `default_grid` and `popup_grid` must point to distinct, valid, exclusively
/// accessible `CScreenGrid` values whose buffers satisfy the ownership and
/// layout requirements of `ScreenGrid`. `window_grid` must be null or point to
/// a third such grid. `output` must point to valid, properly aligned writable
/// `CGridCell` storage that does not alias any grid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_inspect_cell(
    default_grid_ptr: *mut CScreenGrid,
    popup_grid: *mut CScreenGrid,
    window_grid: *mut CScreenGrid,
    requested_grid: i64,
    row: i64,
    column: i64,
    output: *mut CGridCell,
) -> CGridCellInspectionResult {
    if default_grid_ptr.is_null() || popup_grid.is_null() || output.is_null() {
        return CGridCellInspectionResult::InvalidGrid;
    }

    // SAFETY: The caller transfers exclusive ownership of each distinct grid's
    // buffers into its guard for the duration of this operation.
    let default_guard = unsafe { ScreenGridGuard::take(default_grid_ptr) };
    // SAFETY: The caller guarantees that the popup grid is valid and distinct
    // from the default grid.
    let popup_grid = unsafe { ScreenGridGuard::take(popup_grid) };
    let window_grid = std::ptr::NonNull::new(window_grid).map(|window_grid| {
        // SAFETY: The caller guarantees that a non-null window grid is valid
        // and distinct from the default and popup grids.
        unsafe { ScreenGridGuard::take(window_grid.as_ptr()) }
    });

    let target = match GridInspectionTarget::from_requested_handle(requested_grid, popup_grid.id())
    {
        Ok(target) => target,
        Err(InvalidGridId) => return CGridCellInspectionResult::InvalidGrid,
    };
    let inspector = GridCellInspector::new(&default_guard, &popup_grid, window_grid.as_deref());
    match inspector.cell(target, row, column) {
        Err(InvalidGridId) => CGridCellInspectionResult::InvalidGrid,
        Ok(None) => CGridCellInspectionResult::MissingCell,
        Ok(Some(cell)) => {
            // SAFETY: The output pointer was checked for null above and the
            // caller guarantees valid writable storage.
            unsafe {
                output.write(CGridCell::from(cell));
            }
            CGridCellInspectionResult::Found
        }
    }
}

/// Put a unicode character in a screen cell.
#[unsafe(no_mangle)]
pub extern "C" fn schar_from_char(mut c: libc::c_int) -> VimSCharT {
    if c >= 0x200000 {
        // TODO(bfredl): this must NEVER happen, even if the file contained overlong sequences
        c = 0xFFFD;
    }
    let mut bytes = (0 as VimSCharT).to_le_bytes();
    let encoded = mbyte::EncodedCodepoint::new(c);
    bytes[..encoded.as_bytes().len()].copy_from_slice(encoded.as_bytes());
    VimSCharT::from_le_bytes(bytes)
}

/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid` whose
/// non-null buffers satisfy the ownership and layout requirements of
/// `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_assign_handle(grid: *mut CScreenGrid) -> VimHandleT {
    // SAFETY: The caller transfers exclusive ownership of the grid buffers to
    // the guard.
    let mut grid = unsafe { ScreenGridGuard::take(grid) };
    GRID_HANDLE_ALLOCATOR.assign(&mut grid.handle);
    grid.id().as_native_handle()
}

/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid` whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_ins_lines(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    line_count: libc::c_int,
    end: libc::c_int,
    col: libc::c_int,
    width: libc::c_int,
) {
    let (target, command) = {
        // SAFETY: The caller transfers exclusive ownership of the grid buffers
        // to the guard.
        let mut grid = unsafe { ScreenGridGuard::take(grid) };
        let target = GridOutputTarget::new(grid.id(), grid.cols);
        let command = grid.ins_lines(row, line_count, end, col, width);
        (target, command)
    };
    if let Some(command) = command {
        target.emit(command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn characters(line_buffer: &LineBuffer) -> Vec<VimSCharT> {
        line_buffer
            .cells
            .iter()
            .map(|cell| cell.character)
            .collect()
    }

    fn attributes(line_buffer: &LineBuffer) -> Vec<VimSAttrT> {
        line_buffer
            .cells
            .iter()
            .map(|cell| cell.attribute)
            .collect()
    }

    fn virtual_columns(line_buffer: &LineBuffer) -> Vec<VimColNrT> {
        line_buffer
            .cells
            .iter()
            .map(|cell| cell.virtual_column)
            .collect()
    }

    fn set_characters(line_buffer: &mut LineBuffer, characters: &[VimSCharT]) {
        assert_eq!(line_buffer.len(), characters.len());
        for (cell, &character) in line_buffer.cells.iter_mut().zip(characters) {
            cell.character = character;
        }
    }

    fn set_attributes(line_buffer: &mut LineBuffer, attributes: &[VimSAttrT]) {
        assert_eq!(line_buffer.len(), attributes.len());
        for (cell, &attribute) in line_buffer.cells.iter_mut().zip(attributes) {
            cell.attribute = attribute;
        }
    }

    fn set_virtual_columns(line_buffer: &mut LineBuffer, virtual_columns: &[VimColNrT]) {
        assert_eq!(line_buffer.len(), virtual_columns.len());
        for (cell, &virtual_column) in line_buffer.cells.iter_mut().zip(virtual_columns) {
            cell.virtual_column = virtual_column;
        }
    }

    #[test]
    fn grid_handle_allocator_assigns_each_handle_once() {
        let allocator = GridHandleAllocator::new(DEFAULT_GRID_HANDLE);
        let mut first = 0;
        let mut second = 0;

        allocator.assign(&mut first);
        allocator.assign(&mut first);
        allocator.assign(&mut second);

        assert_eq!(first, DEFAULT_GRID_HANDLE + 1);
        assert_eq!(second, DEFAULT_GRID_HANDLE + 2);
    }

    #[test]
    fn inserting_lines_returns_upward_scroll_command() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);

        let command = grid.ins_lines(0, 1, 2, 0, 3).unwrap();
        let UiCommand::GridScroll { region, delta } = command else {
            panic!("line insertion must return a scroll command");
        };

        assert_eq!(region, GridRegion::try_new(0..2, 0..3).unwrap());
        assert_eq!(delta, GridDelta::new(-1, 0));
    }

    #[test]
    fn window_line_scrolling_clips_to_grid_and_view() {
        let mut line_buffer = LineBuffer::new(6);
        let mut grid = ScreenGrid::new(4, 6, true, &mut line_buffer);
        let request = WindowLineScrollRequest {
            row: 0,
            line_count: 1,
            view_origin: GridPosition::new(1, 2),
            view_size: GridSize::try_new(3, 5).unwrap(),
        };

        let command = grid.scroll_window_lines(request).unwrap();
        let UiCommand::GridScroll { region, delta } = command else {
            panic!("window line insertion must return a scroll command");
        };

        assert_eq!(region, GridRegion::try_new(1..4, 2..6).unwrap());
        assert_eq!(delta, GridDelta::new(-1, 0));
        assert!(
            grid.scroll_window_lines(WindowLineScrollRequest { row: 2, ..request })
                .is_none()
        );
        assert!(
            grid.scroll_window_lines(WindowLineScrollRequest {
                line_count: i32::MIN,
                ..request
            })
            .is_none()
        );
    }

    #[test]
    fn deleting_lines_returns_downward_scroll_command() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);

        let command = grid.del_lines(0, 1, 2, 0, 3).unwrap();
        let UiCommand::GridScroll { region, delta } = command else {
            panic!("line deletion must return a scroll command");
        };

        assert_eq!(region, GridRegion::try_new(0..2, 0..3).unwrap());
        assert_eq!(delta, GridDelta::new(1, 0));
    }

    #[test]
    fn window_grid_resize_preserves_valid_contents() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        grid.handle = 7;
        grid.valid = true;
        grid.chars[0] = schar_from_ascii('x');
        grid.attrs[0] = 12;
        grid.vcols[0] = 4;

        let update = grid.configure_window_view(
            WindowGridMode::Dedicated(GridSize::try_new(2, 3).unwrap()),
            GridPosition::new(1, 2),
            &mut line_buffer,
        );

        assert_eq!(update.lifecycle, GridLifecycleChange::Resized);
        assert_eq!(update.grid_id, GridId::new(7));
        assert_eq!(
            update.view,
            WindowGridViewSelection {
                target: WindowGridViewTarget::Dedicated,
                offset: GridPosition::new(1, 2),
            }
        );
        assert_eq!(grid.handle, 7);
        assert_eq!((grid.rows, grid.cols), (2, 3));
        assert!(grid.valid);
        assert_eq!(grid.chars[0], schar_from_ascii('x'));
        assert_eq!(grid.attrs[0], 12);
        assert_eq!(grid.vcols[0], 4);
        assert_eq!(grid.attrs[2], -1);
    }

    #[test]
    fn window_grid_resize_discards_invalid_contents() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        grid.chars[0] = schar_from_ascii('x');
        grid.attrs[0] = 12;

        let change = grid.configure_for_window(
            WindowGridMode::Dedicated(GridSize::try_new(2, 3).unwrap()),
            &mut line_buffer,
        );

        assert_eq!(change, GridLifecycleChange::Resized);
        assert!(grid.valid);
        assert_eq!(grid.chars[0], schar_from_ascii(' '));
        assert_eq!(grid.attrs[0], -1);
    }

    #[test]
    fn window_grid_configuration_revalidates_existing_storage() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        grid.attrs.fill(3);

        let change = grid.configure_for_window(
            WindowGridMode::Dedicated(GridSize::try_new(1, 2).unwrap()),
            &mut line_buffer,
        );

        assert_eq!(change, GridLifecycleChange::Revalidated);
        assert!(grid.valid);
        assert_eq!(grid.attrs, [-1, -1]);
    }

    #[test]
    fn shared_window_grid_releases_dedicated_storage() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        grid.handle = 7;
        grid.valid = true;

        let update = grid.configure_window_view(
            WindowGridMode::Shared,
            GridPosition::new(3, 4),
            &mut line_buffer,
        );

        assert_eq!(update.lifecycle, GridLifecycleChange::Released);
        assert_eq!(update.grid_id, GridId::new(7));
        assert_eq!(
            update.view,
            WindowGridViewSelection {
                target: WindowGridViewTarget::Shared,
                offset: GridPosition::new(3, 4),
            }
        );
        assert_eq!(grid.handle, 7);
        assert!(!grid.is_allocated());
        assert!(!grid.valid);
        assert_eq!(
            grid.configure_for_window(WindowGridMode::Shared, &mut line_buffer),
            GridLifecycleChange::Unchanged
        );
    }

    #[test]
    fn default_grid_configuration_owns_identity_and_resize_decisions() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        grid.chars[0] = schar_from_ascii('x');
        grid.attrs[0] = 12;

        grid.initialize_as_default();
        let change = grid.configure_as_default(GridSize::try_new(2, 3).unwrap(), &mut line_buffer);

        assert_eq!(change, GridLifecycleChange::Resized);
        assert_eq!(grid.handle, DEFAULT_GRID_HANDLE);
        assert_eq!((grid.rows, grid.cols), (2, 3));
        assert_eq!(grid.chars[0], schar_from_ascii('x'));
        assert_eq!(grid.attrs[0], 12);
        assert_eq!(
            grid.configure_as_default(GridSize::try_new(2, 3).unwrap(), &mut line_buffer,),
            GridLifecycleChange::Unchanged
        );
        assert_eq!(
            grid.configure_as_default(GridSize::try_new(0, 3).unwrap(), &mut line_buffer,),
            GridLifecycleChange::Unchanged
        );
    }

    #[test]
    fn popup_grid_resize_preserves_cells_when_position_is_valid() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        grid.handle = 9;
        grid.chars[0] = schar_from_ascii('x');
        grid.attrs[0] = 12;

        let change = grid.configure_for_popup_menu(
            GridSize::try_new(2, 3).unwrap(),
            false,
            &mut line_buffer,
        );

        assert_eq!(change, GridLifecycleChange::Resized);
        assert_eq!(grid.handle, 9);
        assert_eq!((grid.rows, grid.cols), (2, 3));
        assert_eq!(grid.chars[0], schar_from_ascii('x'));
        assert_eq!(grid.attrs[0], 12);
        assert_eq!(grid.attrs[2], -1);
    }

    #[test]
    fn popup_grid_configuration_invalidates_moved_contents() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        grid.chars[0] = schar_from_ascii('x');
        grid.attrs.fill(12);

        let change =
            grid.configure_for_popup_menu(GridSize::try_new(1, 2).unwrap(), true, &mut line_buffer);

        assert_eq!(change, GridLifecycleChange::Invalidated);
        assert_eq!(grid.chars[0], schar_from_ascii('x'));
        assert_eq!(grid.attrs, [-1, -1]);
        assert_eq!(
            grid.configure_for_popup_menu(
                GridSize::try_new(1, 2).unwrap(),
                false,
                &mut line_buffer,
            ),
            GridLifecycleChange::Unchanged
        );
    }

    #[test]
    fn message_grid_preparation_allocates_clean_tracked_storage() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, false, &mut line_buffer);
        grid.chars[0] = schar_from_ascii('x');
        grid.attrs[0] = 12;
        grid.throttled = true;
        let allocator = GridHandleAllocator::new(DEFAULT_GRID_HANDLE);

        let transition = grid.prepare_for_messages(
            MessageGridMode::Visible(GridSize::try_new(2, 3).unwrap()),
            &allocator,
            &mut line_buffer,
        );

        assert_eq!(transition, MessageGridTransition::Resized);
        assert_eq!(grid.handle, DEFAULT_GRID_HANDLE + 1);
        assert_eq!((grid.rows, grid.cols), (2, 3));
        assert_eq!(grid.chars[0], schar_from_ascii(' '));
        assert_eq!(grid.attrs, [0; 6]);
        assert_eq!(
            grid.dirty_columns.as_ref().map(|columns| &*columns.columns),
            Some([0, 0].as_slice())
        );
        assert!(!grid.throttled);
    }

    #[test]
    fn message_grid_release_waits_for_compositor_removal() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        grid.throttled = true;
        let allocator = GridHandleAllocator::new(DEFAULT_GRID_HANDLE);

        let transition =
            grid.prepare_for_messages(MessageGridMode::Hidden, &allocator, &mut line_buffer);

        assert_eq!(transition, MessageGridTransition::ReleaseRequired);
        assert!(grid.is_allocated());
        assert!(grid.throttled);

        grid.release_message();

        assert!(!grid.is_allocated());
        assert!(!grid.throttled);
        assert_eq!(
            grid.prepare_for_messages(MessageGridMode::Hidden, &allocator, &mut line_buffer,),
            MessageGridTransition::Unchanged
        );
    }

    #[test]
    fn throttled_line_changes_do_not_return_scroll_commands() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);
        grid.throttled = true;

        assert!(grid.ins_lines(0, 1, 2, 0, 3).is_none());
        assert!(grid.del_lines(0, 1, 2, 0, 3).is_none());
    }

    #[test]
    fn message_scroll_up_returns_reposition_before_zero_cmdheight_clear() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);

        let plan = grid.scroll_message_up(2, true, true);

        let MessageGridScrollUp {
            started_throttling,
            action:
                MessageGridScrollUpAction::Reposition {
                    position,
                    scrolled,
                    clear_first_row,
                },
        } = plan
        else {
            panic!("positive message-grid position must be moved");
        };
        assert_eq!(position, 1);
        assert!(!scrolled);
        assert!(clear_first_row);
        assert!(started_throttling);
        assert!(grid.throttled);
    }

    #[test]
    fn message_position_update_controls_publication_and_view_offset() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);

        assert_eq!(
            grid.update_message_position(2),
            MessageGridPositionUpdate {
                publish: true,
                view_row_offset: Some(-2),
            }
        );

        grid.throttled = true;
        assert_eq!(
            grid.update_message_position(1),
            MessageGridPositionUpdate {
                publish: false,
                view_row_offset: Some(-1),
            }
        );

        grid.throttled = false;
        grid.release_storage();
        assert_eq!(
            grid.update_message_position(0),
            MessageGridPositionUpdate {
                publish: true,
                view_row_offset: None,
            }
        );
    }

    #[test]
    fn preparing_message_redraw_clears_rows_with_expected_validity() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(3, 2, true, &mut line_buffer);
        grid.chars.fill(schar_from_ascii('x'));
        grid.attrs.fill(7);
        grid.vcols.fill(4);
        grid.throttled = true;

        grid.prepare_message_redraw(2, 1);

        assert!(!grid.throttled);
        assert_eq!(
            grid.chars,
            [
                schar_from_ascii(' '),
                schar_from_ascii(' '),
                schar_from_ascii(' '),
                schar_from_ascii(' '),
                schar_from_ascii('x'),
                schar_from_ascii('x'),
            ]
        );
        assert_eq!(grid.attrs, [0, 0, -1, -1, 7, 7]);
        assert_eq!(grid.vcols, [-1, -1, -1, -1, 4, 4]);
    }

    #[test]
    fn message_scroll_up_shifts_rows_and_dirty_columns() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(3, 2, true, &mut line_buffer);
        grid.handle = 7;
        grid.reset_dirty_columns();
        grid.mark_dirty_column(1, 2);
        let first = grid.line_offset[0];
        let second = grid.line_offset[1];
        let third = grid.line_offset[2];
        grid.chars[first] = schar_from_ascii('a');
        grid.chars[second] = schar_from_ascii('b');
        grid.chars[third] = schar_from_ascii('c');

        let plan = grid.scroll_message_up(0, false, false);

        let MessageGridScrollUp {
            started_throttling,
            action:
                MessageGridScrollUpAction::ShiftRows {
                    scroll: Some(scroll),
                    ..
                },
        } = plan
        else {
            panic!("stationary message grid must shift its rows");
        };
        assert!(!started_throttling);
        let UiCommand::GridScroll { region, delta } = scroll else {
            panic!("unthrottled row shift must emit a scroll command");
        };
        assert_eq!(region, GridRegion::try_new(0..3, 0..2).unwrap());
        assert_eq!(delta, GridDelta::new(1, 0));
        assert_eq!(grid.cell(0, 0).unwrap().character, schar_from_ascii('b'));
        assert_eq!(grid.cell(1, 0).unwrap().character, schar_from_ascii('c'));
        assert_eq!(grid.cell(2, 0).unwrap().character, schar_from_ascii(' '));
        assert_eq!(
            grid.dirty_columns.as_ref().map(|columns| &*columns.columns),
            Some([2, 0, 0].as_slice())
        );
    }

    #[test]
    fn dirty_columns_track_scroll_and_clear() {
        let mut dirty_columns = DirtyColumns::new(3);

        dirty_columns.mark(0, 2);
        dirty_columns.mark(0, 1);
        dirty_columns.mark(1, 3);
        assert_eq!(&*dirty_columns.columns, [2, 3, 0]);

        dirty_columns.scroll_up();
        assert_eq!(&*dirty_columns.columns, [3, 0, 0]);
        assert_eq!(dirty_columns.take(0), Some(3));
        assert_eq!(&*dirty_columns.columns, [0, 0, 0]);
        assert_eq!(dirty_columns.take(3), None);
    }

    #[test]
    fn message_scroll_flush_owns_pending_output_and_resets_dirty_rows() {
        let mut line_buffer = LineBuffer::new(4);
        let mut grid = ScreenGrid::new(3, 4, true, &mut line_buffer);
        grid.handle = 7;
        grid.reset_dirty_columns();
        grid.throttled = true;
        grid.mark_dirty_column(1, 3);
        grid.mark_dirty_column(2, 2);

        let plan = grid.flush_message_scroll(
            MessageGridScrollFlushRequest {
                position: 0,
                scrolled_lines: 3,
                screen_size: GridSize::try_new(3, 4).unwrap(),
                clear_attribute: 9,
            },
            MessageScrollState {
                position_at_last_flush: 2,
                scrolled_lines_at_last_flush: 0,
                scroll_discount: 0,
            },
        );

        assert!(!grid.throttled);
        assert_eq!(plan.position_update, Some(0));
        let scroll = plan.scroll.unwrap();
        let UiCommand::GridScroll { region, delta } = scroll else {
            panic!("message flush must produce a grid-scroll command");
        };
        assert_eq!(region, GridRegion::try_new(0..3, 0..4).unwrap());
        assert_eq!(delta, GridDelta::new(1, 0));
        assert_eq!(
            plan.lines
                .iter()
                .map(|line| line.columns.clone())
                .collect::<Vec<_>>(),
            [0..0, 0..3, 0..2]
        );
        assert_eq!(
            grid.dirty_columns.as_ref().map(|columns| &*columns.columns),
            Some([0, 0, 0].as_slice())
        );
    }

    #[test]
    fn message_scroll_state_tracks_complete_throttling_lifecycle() {
        let mut state = MessageScrollState::new();

        state.record_position(4);
        state.record_resize(3);
        state.discount_first_scroll();
        assert_eq!(
            state,
            MessageScrollState {
                position_at_last_flush: 4,
                scrolled_lines_at_last_flush: 2,
                scroll_discount: 1,
            }
        );

        state.finish_flush(0, 6);
        state.reset_progress();
        assert_eq!(
            state,
            MessageScrollState {
                position_at_last_flush: 0,
                scrolled_lines_at_last_flush: 0,
                scroll_discount: 0,
            }
        );
    }

    #[test]
    fn line_buffer_allocates_initialized_storage() {
        let line_buffer = LineBuffer::new(4);

        assert_eq!(line_buffer.cells, [GridCell::default(); 4]);
        assert_eq!(line_buffer.mirror_scratch, [GridCell::default(); 4]);
        assert_eq!(line_buffer.len(), 4);
    }

    #[test]
    fn line_buffer_fill_resets_visible_prefix() {
        let mut line_buffer = LineBuffer::new(5);
        set_characters(&mut line_buffer, &[schar_from_ascii('x'); 5]);
        set_attributes(&mut line_buffer, &[7; 5]);
        set_virtual_columns(&mut line_buffer, &[9; 5]);
        let scratch_cell = GridCell::new(11, 11, 11);
        line_buffer.mirror_scratch.fill(scratch_cell);

        line_buffer.fill(0..3, schar_from_ascii(' '), 0, -1);

        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii(' '),
                schar_from_ascii(' '),
                schar_from_ascii(' '),
                schar_from_ascii('x'),
                schar_from_ascii('x'),
            ]
        );
        assert_eq!(attributes(&line_buffer), [0, 0, 0, 7, 7]);
        assert_eq!(virtual_columns(&line_buffer), [-1, -1, -1, 9, 9]);
        assert_eq!(line_buffer.mirror_scratch, [scratch_cell; 5]);
    }

    #[test]
    fn line_buffer_fills_chars_and_attrs_without_changing_vcols() {
        let mut line_buffer = LineBuffer::new(5);
        set_characters(&mut line_buffer, &[schar_from_ascii('x'); 5]);
        set_attributes(&mut line_buffer, &[7; 5]);
        set_virtual_columns(&mut line_buffer, &[1, 2, 3, 4, 5]);

        assert!(line_buffer.fill_chars_and_attrs(1..4, schar_from_ascii(' '), 42,));

        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii('x'),
                schar_from_ascii(' '),
                schar_from_ascii(' '),
                schar_from_ascii(' '),
                schar_from_ascii('x'),
            ]
        );
        assert_eq!(attributes(&line_buffer), [7, 42, 42, 42, 7]);
        assert_eq!(virtual_columns(&line_buffer), [1, 2, 3, 4, 5]);

        assert!(!line_buffer.fill_chars_and_attrs(4..6, 0, 0));
    }

    #[test]
    fn line_buffer_puts_chars_without_changing_metadata() {
        let mut line_buffer = LineBuffer::new(4);
        set_characters(&mut line_buffer, &[schar_from_ascii(' '); 4]);
        set_attributes(&mut line_buffer, &[1, 2, 3, 4]);
        set_virtual_columns(&mut line_buffer, &[5, 6, 7, 8]);

        assert!(line_buffer.put_chars(1, &[schar_from_ascii('S'), schar_from_ascii('G')],));

        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii(' '),
                schar_from_ascii('S'),
                schar_from_ascii('G'),
                schar_from_ascii(' '),
            ]
        );
        assert_eq!(attributes(&line_buffer), [1, 2, 3, 4]);
        assert_eq!(virtual_columns(&line_buffer), [5, 6, 7, 8]);
        assert!(!line_buffer.put_chars(3, &[1, 2]));
    }

    #[test]
    fn line_buffer_updates_cell_with_optional_character() {
        let mut line_buffer = LineBuffer::new(3);
        set_characters(
            &mut line_buffer,
            &[
                schar_from_ascii('a'),
                schar_from_ascii('b'),
                schar_from_ascii('c'),
            ],
        );
        set_attributes(&mut line_buffer, &[1, 2, 3]);
        set_virtual_columns(&mut line_buffer, &[4, 5, 6]);

        assert!(line_buffer.update_cell(1, Some(schar_from_ascii('x')), 20, 50));
        assert!(line_buffer.update_cell(2, None, 30, 60));

        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii('a'),
                schar_from_ascii('x'),
                schar_from_ascii('c'),
            ]
        );
        assert_eq!(attributes(&line_buffer), [1, 20, 30]);
        assert_eq!(virtual_columns(&line_buffer), [4, 50, 60]);
        assert!(!line_buffer.update_cell(3, Some(0), 0, 0));
    }

    #[test]
    fn line_buffer_puts_single_width_rendered_glyph() {
        let glyph_map = GlyphMapping::new();
        let mut line_buffer = LineBuffer::new(3);
        set_characters(&mut line_buffer, &[schar_from_ascii(' '); 3]);
        set_attributes(&mut line_buffer, &[1, 2, 3]);
        set_virtual_columns(&mut line_buffer, &[4, 5, 6]);

        assert_eq!(
            line_buffer.put_rendered_glyph(1, schar_from_ascii('x'), 20, 50, &glyph_map,),
            Some(1)
        );
        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii(' '),
                schar_from_ascii('x'),
                schar_from_ascii(' '),
            ]
        );
        assert_eq!(attributes(&line_buffer), [1, 20, 3]);
        assert_eq!(virtual_columns(&line_buffer), [4, 50, 6]);
    }

    #[test]
    fn line_buffer_puts_double_width_rendered_glyph() {
        let mut glyph_map = GlyphMapping::new();
        let glyph = super::schar_from_bytes("界".as_bytes(), &mut glyph_map);
        let mut line_buffer = LineBuffer::new(4);
        set_characters(&mut line_buffer, &[schar_from_ascii(' '); 4]);
        set_attributes(&mut line_buffer, &[1, 2, 3, 4]);
        set_virtual_columns(&mut line_buffer, &[5, 6, 7, 8]);

        assert_eq!(
            line_buffer.put_rendered_glyph(1, glyph, 20, 50, &glyph_map),
            Some(2)
        );
        assert_eq!(
            characters(&line_buffer),
            [schar_from_ascii(' '), glyph, 0, schar_from_ascii(' '),]
        );
        assert_eq!(attributes(&line_buffer), [1, 20, 20, 4]);
        assert_eq!(virtual_columns(&line_buffer), [5, 50, 51, 8]);

        let original = format!("{line_buffer:?}");
        assert_eq!(
            line_buffer.put_rendered_glyph(3, glyph, 30, 60, &glyph_map),
            None
        );
        assert_eq!(format!("{line_buffer:?}"), original);
    }

    #[test]
    fn text_glyph_decodes_characters_tabs_and_clipping() {
        let mut glyph_map = GlyphMapping::new();

        assert_eq!(
            TextGlyph::from_text(b"x", 4, 0, &mut glyph_map),
            Some(TextGlyph {
                contents: TextGlyphContents::Character(schar_from_ascii('x')),
                width: 1,
                bytes_consumed: 1,
            })
        );
        assert_eq!(
            TextGlyph::from_text(b"\t", 2, 4, &mut glyph_map),
            Some(TextGlyph {
                contents: TextGlyphContents::Spaces,
                width: 2,
                bytes_consumed: 1,
            })
        );

        let wide = super::schar_from_bytes("界".as_bytes(), &mut glyph_map);
        assert_eq!(
            TextGlyph::from_text("界".as_bytes(), 2, 0, &mut glyph_map),
            Some(TextGlyph {
                contents: TextGlyphContents::Character(wide),
                width: 2,
                bytes_consumed: "界".len(),
            })
        );
        assert_eq!(
            TextGlyph::from_text("界".as_bytes(), 1, 0, &mut glyph_map),
            Some(TextGlyph {
                contents: TextGlyphContents::Spaces,
                width: 1,
                bytes_consumed: 0,
            })
        );

        let combining = TextGlyph::screen_character("\u{301}".as_bytes(), &mut glyph_map);
        assert_eq!(
            glyph_map.resolve(combining).as_bytes(),
            " \u{301}".as_bytes()
        );
    }

    #[test]
    fn line_buffer_puts_text_glyph_with_provided_virtual_columns() {
        let glyph = schar_from_ascii('w');
        let mut line_buffer = LineBuffer::new(5);
        set_characters(
            &mut line_buffer,
            &[
                schar_from_ascii(' '),
                schar_from_ascii(' '),
                schar_from_ascii(' '),
                0,
                schar_from_ascii('x'),
            ],
        );
        set_attributes(&mut line_buffer, &[1, 2, 3, 4, 5]);
        set_virtual_columns(&mut line_buffer, &[6, 7, 8, 9, 10]);

        assert!(line_buffer.put_text_glyph(
            1,
            TextGlyph {
                contents: TextGlyphContents::Character(glyph),
                width: 2,
                bytes_consumed: 1,
            },
            20,
            VirtualColumns::Provided(&[30, 40]),
            3,
            false,
        ));

        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii(' '),
                glyph,
                0,
                schar_from_ascii(' '),
                schar_from_ascii('x')
            ]
        );
        assert_eq!(attributes(&line_buffer), [1, 20, 20, 4, 5]);
        assert_eq!(virtual_columns(&line_buffer), [6, 30, 40, 9, 10]);
    }

    #[test]
    fn line_buffer_overlays_text_across_double_width_cells() {
        let first = schar_from_ascii('w');
        let second = schar_from_ascii('x');
        let mut line_buffer = LineBuffer::new(5);
        set_characters(
            &mut line_buffer,
            &[first, 0, schar_from_ascii('c'), second, 0],
        );
        set_attributes(&mut line_buffer, &[1, 2, 3, 4, 5]);
        set_virtual_columns(&mut line_buffer, &[6, 7, 8, 9, 10]);
        let glyph = |character| TextGlyph {
            contents: TextGlyphContents::Character(character),
            width: 1,
            bytes_consumed: 1,
        };

        assert!(line_buffer.put_virtual_text_glyph(
            1,
            glyph(schar_from_ascii('a')),
            20,
            VirtualTextHighlightMode::Replace,
            false,
            4,
        ));
        assert!(line_buffer.put_virtual_text_glyph(
            3,
            glyph(schar_from_ascii('b')),
            30,
            VirtualTextHighlightMode::Replace,
            false,
            2,
        ));

        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii(' '),
                schar_from_ascii('a'),
                schar_from_ascii('c'),
                schar_from_ascii('b'),
                schar_from_ascii(' '),
            ]
        );
        assert_eq!(attributes(&line_buffer), [1, 20, 3, 30, 5]);
        assert_eq!(virtual_columns(&line_buffer), [6, 7, 8, 9, 10]);
    }

    #[test]
    fn line_buffer_fills_consecutive_virtual_columns() {
        let mut line_buffer = LineBuffer::new(5);
        set_characters(&mut line_buffer, &[schar_from_ascii('x'); 5]);
        set_attributes(&mut line_buffer, &[7; 5]);
        set_virtual_columns(&mut line_buffer, &[9; 5]);

        assert!(line_buffer.fill_vcols_from(1..4, 20));

        assert_eq!(characters(&line_buffer), [schar_from_ascii('x'); 5]);
        assert_eq!(attributes(&line_buffer), [7; 5]);
        assert_eq!(virtual_columns(&line_buffer), [9, 20, 21, 22, 9]);

        assert!(!line_buffer.fill_vcols_from(4..6, 0));
        assert!(!line_buffer.fill_vcols_from(0..2, VimColNrT::MAX));
        assert_eq!(virtual_columns(&line_buffer), [9, 20, 21, 22, 9]);
    }

    #[test]
    fn line_buffer_fills_constant_virtual_columns() {
        let mut line_buffer = LineBuffer::new(5);
        set_characters(&mut line_buffer, &[schar_from_ascii('x'); 5]);
        set_attributes(&mut line_buffer, &[7; 5]);
        set_virtual_columns(&mut line_buffer, &[9; 5]);

        assert!(line_buffer.fill_vcols(1..4, 20));

        assert_eq!(characters(&line_buffer), [schar_from_ascii('x'); 5]);
        assert_eq!(attributes(&line_buffer), [7; 5]);
        assert_eq!(virtual_columns(&line_buffer), [9, 20, 20, 20, 9]);
        assert!(!line_buffer.fill_vcols(4..6, 0));
    }

    #[test]
    fn line_buffer_release_clears_rust_storage() {
        let mut line_buffer = LineBuffer::new(4);

        line_buffer.release();

        assert!(line_buffer.is_empty());
        assert!(line_buffer.mirror_scratch.is_empty());
    }

    #[test]
    fn line_buffer_places_left_truncation_marker() {
        let mut line_buffer = LineBuffer::new(5);
        set_characters(
            &mut line_buffer,
            &[
                schar_from_ascii('a'),
                schar_from_ascii('w'),
                0,
                schar_from_ascii('b'),
                schar_from_ascii('c'),
            ],
        );
        set_attributes(&mut line_buffer, &[7; 5]);

        line_buffer.put_truncation_marker(1..4, TruncationSide::Left, None, 42);

        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii('a'),
                schar_from_ascii('<'),
                schar_from_ascii(' '),
                schar_from_ascii('b'),
                schar_from_ascii('c'),
            ]
        );
        assert_eq!(attributes(&line_buffer), [7, 42, 7, 7, 7]);

        // A marker at the left edge must not inspect or modify the preceding
        // cell, even when the marker cell initially contains NUL.
        line_buffer.cells[1].character = 0;
        line_buffer.put_truncation_marker(1..4, TruncationSide::Left, None, 42);
        assert_eq!(line_buffer.cells[0].character, schar_from_ascii('a'));
        assert_eq!(line_buffer.cells[1].character, schar_from_ascii('<'));
    }

    #[test]
    fn line_buffer_places_right_truncation_marker() {
        let mut line_buffer = LineBuffer::new(5);
        set_characters(
            &mut line_buffer,
            &[
                schar_from_ascii('a'),
                schar_from_ascii('b'),
                schar_from_ascii('w'),
                0,
                schar_from_ascii('c'),
            ],
        );
        set_attributes(&mut line_buffer, &[7; 5]);

        line_buffer.put_truncation_marker(
            1..4,
            TruncationSide::Right,
            Some(schar_from_ascii('*')),
            42,
        );

        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii('a'),
                schar_from_ascii('b'),
                schar_from_ascii(' '),
                schar_from_ascii('*'),
                schar_from_ascii('c'),
            ]
        );
        assert_eq!(attributes(&line_buffer), [7, 7, 7, 42, 7]);
    }

    #[test]
    fn line_buffer_puts_smoothscroll_marker() {
        let mut line_buffer = LineBuffer::new(5);
        set_characters(
            &mut line_buffer,
            &[
                schar_from_ascii('a'),
                schar_from_ascii('b'),
                schar_from_ascii('w'),
                0,
                schar_from_ascii('c'),
            ],
        );
        set_attributes(&mut line_buffer, &[7; 5]);
        set_virtual_columns(&mut line_buffer, &[1, 2, 3, 4, 5]);

        assert!(line_buffer.put_smoothscroll_marker(5, false, 42));

        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii('<'),
                schar_from_ascii('<'),
                schar_from_ascii('<'),
                schar_from_ascii(' '),
                schar_from_ascii('c'),
            ]
        );
        assert_eq!(attributes(&line_buffer), [42, 42, 42, 7, 7]);
        assert_eq!(virtual_columns(&line_buffer), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn line_buffer_puts_smoothscroll_marker_after_line_number() {
        let mut line_buffer = LineBuffer::new(7);
        set_characters(
            &mut line_buffer,
            &[
                schar_from_ascii('1'),
                schar_from_ascii('2'),
                schar_from_ascii('3'),
                schar_from_ascii('w'),
                0,
                schar_from_ascii('c'),
                schar_from_ascii('d'),
            ],
        );
        set_attributes(&mut line_buffer, &[7; 7]);

        assert!(line_buffer.put_smoothscroll_marker(7, true, 42));

        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii('1'),
                schar_from_ascii('2'),
                schar_from_ascii('3'),
                schar_from_ascii('<'),
                schar_from_ascii('<'),
                schar_from_ascii('<'),
                schar_from_ascii('d'),
            ]
        );
        assert_eq!(attributes(&line_buffer), [7, 7, 7, 42, 42, 42, 7]);

        assert!(!line_buffer.put_smoothscroll_marker(8, false, 0));
    }

    #[test]
    fn clearing_row_follows_logical_line_offsets() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);
        grid.chars = ['a', 'b', 'c', 'd', 'e', 'f']
            .map(schar_from_ascii)
            .to_vec();
        grid.attrs = vec![1, 2, 3, 4, 5, 6];
        grid.vcols = vec![1, 2, 3, 4, 5, 6];
        grid.line_offset.swap(0, 1);

        grid.clear_row(0, 2, false);

        assert_eq!(
            grid.chars,
            ['a', 'b', 'c', ' ', ' ', 'f']
                .map(schar_from_ascii)
                .to_vec()
        );
        assert_eq!(grid.attrs, [1, 2, 3, -1, -1, 6]);
        assert_eq!(grid.vcols, [1, 2, 3, -1, -1, 6]);
    }

    #[test]
    fn clearing_complete_rows_uses_grid_dimensions_and_logical_offsets() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);
        grid.chars = "abcdef".chars().map(schar_from_ascii).collect();
        grid.attrs = vec![1, 2, 3, 4, 5, 6];
        grid.vcols = vec![1, 2, 3, 4, 5, 6];
        grid.line_offset.swap(0, 1);

        assert!(grid.clear_rows(0..1, false));
        assert_eq!(
            grid.chars,
            "abc   ".chars().map(schar_from_ascii).collect::<Vec<_>>()
        );
        assert_eq!(grid.attrs, [1, 2, 3, -1, -1, -1]);
        assert_eq!(grid.vcols, [1, 2, 3, -1, -1, -1]);

        assert!(grid.clear_all(true));
        assert_eq!(grid.chars, vec![schar_from_ascii(' '); 6]);
        assert_eq!(grid.attrs, [0; 6]);
        assert_eq!(grid.vcols, [-1; 6]);

        grid.release_storage();
        assert!(!grid.clear_all(true));
    }

    #[test]
    fn resize_output_uses_only_allocated_grid_dimensions() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);
        grid.handle = 7;

        let (target, command) = grid.resize_command_if_allocated().unwrap();
        assert_eq!(target, GridOutputTarget::new(GridId::new(7), 3));
        let UiCommand::GridResize(size) = command else {
            panic!("allocated grid must produce a resize command");
        };
        assert_eq!(size, GridSize::try_new(2, 3).unwrap());

        grid.release_storage();

        assert!(grid.resize_command_if_allocated().is_none());
    }

    #[test]
    fn redraw_state_controls_compositor_and_view_decisions() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        grid.attrs.fill(7);

        assert!(grid.requires_compositor_invalidation(false));
        grid.revalidate_for_redraw();
        assert_eq!(grid.attrs, [-1, -1]);
        assert!(!grid.requires_compositor_invalidation(false));
        assert!(grid.requires_compositor_invalidation(true));
        {
            let view = GridView::new(&grid, GridPosition::new(0, 0));
            assert!(!view.requires_redraw());
        }

        grid.require_full_redraw();

        assert!(grid.requires_compositor_invalidation(false));
        let view = GridView::new(&grid, GridPosition::new(0, 0));
        assert!(view.requires_redraw());
    }

    #[test]
    fn invalidating_row_follows_logical_line_offsets() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);
        grid.attrs = vec![1, 2, 3, 4, 5, 6];
        grid.line_offset.swap(0, 1);

        grid.invalidate_row(0);

        assert!(grid.is_invalid_row(0));
        assert!(!grid.is_invalid_row(1));
        assert_eq!(grid.attrs, [1, 2, 3, -1, 5, 6]);
    }

    #[test]
    fn reading_cells_follows_logical_line_offsets_and_validates_coordinates() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);
        grid.chars = "abcdef".chars().map(schar_from_ascii).collect();
        grid.attrs = vec![1, 2, 3, 4, 5, 6];
        grid.vcols = vec![10, 11, 12, 20, 21, 22];
        grid.line_offset.swap(0, 1);

        assert_eq!(
            grid.cell(0, 1),
            Some(GridCell::new(schar_from_ascii('e'), 5, 21))
        );
        assert_eq!(
            grid.cell(1, 2),
            Some(GridCell::new(schar_from_ascii('c'), 3, 12))
        );
        assert_eq!(grid.cell(2, 0), None);
        assert_eq!(grid.cell(0, 3), None);
    }

    #[test]
    fn cell_inspection_selects_default_popup_and_window_grids() {
        let mut line_buffer = LineBuffer::new(1);
        let mut default = ScreenGrid::new(1, 1, true, &mut line_buffer);
        default.handle = DEFAULT_GRID_HANDLE;
        default.chars[0] = schar_from_ascii('d');

        let mut popup_grid = ScreenGrid::new(1, 1, true, &mut line_buffer);
        popup_grid.handle = 7;
        popup_grid.chars[0] = schar_from_ascii('p');

        let mut window_grid = ScreenGrid::new(1, 1, true, &mut line_buffer);
        window_grid.handle = 8;
        window_grid.chars[0] = schar_from_ascii('w');

        let inspector = GridCellInspector::new(&default, &popup_grid, Some(&window_grid));

        let default_target =
            GridInspectionTarget::from_requested_handle(1, popup_grid.id()).unwrap();
        let popup_target = GridInspectionTarget::from_requested_handle(7, popup_grid.id()).unwrap();
        let window_target =
            GridInspectionTarget::from_requested_handle(8, popup_grid.id()).unwrap();

        assert_eq!(
            inspector
                .cell(default_target, 0, 0)
                .unwrap()
                .unwrap()
                .character,
            schar_from_ascii('d')
        );
        assert_eq!(
            inspector
                .cell(popup_target, 0, 0)
                .unwrap()
                .unwrap()
                .character,
            schar_from_ascii('p')
        );
        assert_eq!(
            inspector
                .cell(window_target, 0, 0)
                .unwrap()
                .unwrap()
                .character,
            schar_from_ascii('w')
        );
        assert_eq!(inspector.cell(default_target, -1, 0), Ok(None));
        assert_eq!(inspector.cell(default_target, 0, 1), Ok(None));
    }

    #[test]
    fn cell_inspection_rejects_unknown_or_unallocated_window_grids() {
        let mut line_buffer = LineBuffer::new(1);
        let default = ScreenGrid::new(1, 1, true, &mut line_buffer);
        let mut popup_grid = ScreenGrid::new(1, 1, true, &mut line_buffer);
        popup_grid.handle = 7;
        let mut window_grid = ScreenGrid::new(1, 1, true, &mut line_buffer);
        window_grid.handle = 8;
        window_grid.release_storage();

        let inspector = GridCellInspector::new(&default, &popup_grid, Some(&window_grid));
        let unallocated_window =
            GridInspectionTarget::from_requested_handle(8, popup_grid.id()).unwrap();
        let unknown_window =
            GridInspectionTarget::from_requested_handle(9, popup_grid.id()).unwrap();

        assert_eq!(inspector.cell(unallocated_window, 0, 0), Err(InvalidGridId));
        assert_eq!(inspector.cell(unknown_window, 0, 0), Err(InvalidGridId));
    }

    #[test]
    fn grid_view_translates_cells_and_validates_target_identity() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);
        grid.handle = 7;
        grid.chars = "abcdef".chars().map(schar_from_ascii).collect();
        grid.attrs = vec![1, 2, 3, 4, 5, 6];
        grid.vcols = vec![10, 11, 12, 20, 21, 22];
        let view = GridView::new(&grid, GridPosition::new(1, 1));

        assert!(view.width_matches_target(3));
        assert!(!view.width_matches_target(2));
        let (target, command) = view.plan_cursor_move(GridPosition::new(0, 1)).unwrap();
        assert_eq!(target, GridOutputTarget::new(GridId::new(7), 3));
        let UiCommand::GridCursorGoto(position) = command else {
            panic!("a view cursor move must produce a cursor command");
        };
        assert_eq!(position, GridPosition::new(1, 2));

        assert_eq!(
            view.cell_for_grid(GridId::new(7), GridPosition::new(0, 1)),
            Some(GridCell::new(schar_from_ascii('f'), 6, 22))
        );
        assert_eq!(
            view.cell_for_grid(GridId::new(8), GridPosition::new(0, 1)),
            None
        );
        assert_eq!(
            view.cell_for_grid(GridId::new(7), GridPosition::new(-2, 0)),
            None
        );
        assert_eq!(
            view.cell_for_grid(GridId::new(7), GridPosition::new(i32::MAX, 0)),
            None
        );
    }

    #[test]
    fn c_grid_reports_whether_cell_buffers_are_allocated() {
        let mut line_buffer = LineBuffer::new(1);
        let grid = ScreenGrid::new(1, 1, true, &mut line_buffer);
        // SAFETY: Ownership of every grid allocation is transferred to
        // `cgrid`.
        let mut cgrid = unsafe { grid.to_cgrid() };

        // SAFETY: `cgrid` is valid and remains readable for this call.
        assert!(unsafe { grid_is_allocated(&raw const cgrid) });
        // SAFETY: `grid_is_allocated` explicitly accepts a null pointer.
        assert!(!unsafe { grid_is_allocated(std::ptr::null()) });

        // SAFETY: `cgrid` still uniquely owns every transferred allocation.
        let _grid = unsafe { ScreenGrid::take_from_cgrid(&mut cgrid) };
    }

    #[test]
    fn screen_grid_guard_restores_grid_on_drop() {
        let mut line_buffer = LineBuffer::new(2);
        let grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        // SAFETY: Ownership of every grid allocation is transferred to
        // `cgrid`.
        let mut cgrid = unsafe { grid.to_cgrid() };

        {
            // SAFETY: `cgrid` exclusively owns valid grid allocations and
            // remains alive until the guard is dropped.
            let mut grid = unsafe { ScreenGridGuard::take(&raw mut cgrid) };
            grid.chars[0] = schar_from_ascii('x');
        }

        // SAFETY: The guard returned ownership to `cgrid`.
        let restored_grid = unsafe { ScreenGrid::take_from_cgrid(&mut cgrid) };
        assert_eq!(
            restored_grid.chars.as_slice(),
            &[schar_from_ascii('x'), schar_from_ascii(' ')]
        );
    }

    #[test]
    fn screen_grid_guard_resolves_permanent_rust_storage() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        grid.handle = 70_001;
        // SAFETY: Ownership of every grid allocation is transferred to
        // `cgrid`.
        let mut cgrid = unsafe { grid.to_cgrid() };

        // SAFETY: `cgrid` exclusively owns valid grid allocations and remains
        // at a stable address until the permanent binding is released.
        let grid_id = unsafe { ScreenGridGuard::take(&raw mut cgrid) }
            .store_permanently()
            .unwrap();

        assert_eq!(grid_id, GridId::new(70_001));
        assert_eq!(cgrid.handle, 70_001);
        assert!(cgrid.chars.is_null());
        assert!(cgrid.attrs.is_null());
        assert!(cgrid.vcols.is_null());
        assert!(cgrid.line_offset.is_null());

        // SAFETY: `cgrid` remains a valid opaque identity for the permanently
        // stored grid.
        assert!(unsafe { grid_is_allocated(&raw const cgrid) });

        {
            // SAFETY: The stable C address identifies the registered Rust
            // grid, and this scope has exclusive access to it.
            let mut grid = unsafe { ScreenGridGuard::take(&raw mut cgrid) };
            grid.chars[0] = schar_from_ascii('x');
        }
        {
            // SAFETY: The previous guard was dropped before reacquiring the
            // permanently stored grid.
            let grid = unsafe { ScreenGridGuard::take(&raw mut cgrid) };
            assert_eq!(grid.chars[0], schar_from_ascii('x'));
        }

        let mut line_grid = CLineGrid::new();
        // SAFETY: The stable C address identifies the registered Rust grid and
        // remains valid for the complete line operation.
        unsafe {
            line_grid.begin_raw_grid(&raw mut cgrid, 0, 0, &mut line_buffer);
        }
        assert_eq!(line_grid.max_col(), 2);
        assert!(!line_grid.row_reaches_grid_end());
        line_grid.cancel();

        // SAFETY: No grid guard remains, and `cgrid` is exclusively accessible.
        assert!(unsafe { ScreenGridGuard::release_permanent(&raw mut cgrid) });
    }

    #[test]
    fn dirty_columns_round_trip_through_c_grid() {
        let mut line_buffer = LineBuffer::new(4);
        let mut grid = ScreenGrid::new(2, 4, true, &mut line_buffer);
        assert!(grid.dirty_columns.is_none());

        grid.reset_dirty_columns();
        grid.mark_dirty_column(1, 3);

        // SAFETY: This test transfers every allocation to `cgrid` exactly once.
        let mut cgrid = unsafe { grid.to_cgrid() };
        assert!(!cgrid.dirty_col.is_null());

        let original_pointers = (
            cgrid.chars,
            cgrid.attrs,
            cgrid.vcols,
            cgrid.line_offset,
            cgrid.dirty_col,
        );

        // SAFETY: `cgrid` uniquely owns allocations created by `to_cgrid`.
        let grid = unsafe { ScreenGrid::take_from_cgrid(&mut cgrid) };
        assert_eq!(cgrid.chars, original_pointers.0);
        assert_eq!(cgrid.attrs, original_pointers.1);
        assert_eq!(cgrid.vcols, original_pointers.2);
        assert_eq!(cgrid.line_offset, original_pointers.3);
        assert_eq!(cgrid.dirty_col, original_pointers.4);
        assert_eq!(
            grid.dirty_columns
                .as_ref()
                .map(|dirty_columns| &*dirty_columns.columns),
            Some([0, 3].as_slice())
        );
    }

    #[test]
    fn assigning_handle_preserves_unallocated_grid_buffers() {
        let mut cgrid = CScreenGrid {
            handle: 0,
            chars: std::ptr::null_mut(),
            attrs: std::ptr::null_mut(),
            vcols: std::ptr::null_mut(),
            line_offset: std::ptr::null_mut(),
            dirty_col: std::ptr::null_mut(),
            rows: 8,
            cols: 23,
            valid: false,
            throttled: false,
        };

        let allocator = GridHandleAllocator::new(DEFAULT_GRID_HANDLE);
        // SAFETY: All buffers are null, so there are no allocations to take.
        let mut grid = unsafe { ScreenGrid::take_from_cgrid(&mut cgrid) };
        allocator.assign(&mut grid.handle);
        let cgrid = unsafe { grid.to_cgrid() };

        assert_eq!(cgrid.handle, 2);
        assert!(cgrid.chars.is_null());
        assert!(cgrid.attrs.is_null());
        assert!(cgrid.vcols.is_null());
        assert!(cgrid.line_offset.is_null());
    }

    #[test]
    fn border_text_alignment_columns() {
        assert_eq!(BorderTextAlign::Left.column(9, 5), 1);
        assert_eq!(BorderTextAlign::Center.column(9, 5), 3);
        assert_eq!(BorderTextAlign::Right.column(9, 5), 5);
        assert_eq!(BorderTextAlign::Center.column(9, 12), 1);
        assert_eq!(BorderTextAlign::Right.column(9, 12), 1);
    }

    #[test]
    fn border_text_clips_complete_screen_characters() {
        let mut overflow = 5;
        assert_eq!(BorderText::visible_suffix(b"abcd", &mut overflow), None);
        assert_eq!(overflow, 1);
        assert_eq!(
            BorderText::visible_suffix("界xyz".as_bytes(), &mut overflow),
            Some(b"xyz".as_slice())
        );
        assert_eq!(overflow, -1);
    }

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
    fn glyph_mapping_insert() {
        let mut gm = GlyphMapping::new();
        let bytes = [0x73, 0xcc, 0xb8, 0xcc, 0x89, 0xcc, 0xbf];

        let mapping = gm.insert(&bytes);
        assert_eq!(mapping, GlyphMapping::to_schart(0));
        assert_eq!(gm.get_bytes(mapping), Some(&GlyphBuff::from_bytes(&bytes)));

        let existing_mapping = gm.insert(&bytes);
        assert_eq!(mapping, existing_mapping);
        assert_eq!(gm.glyphs.len(), 1);
        assert_eq!(gm.index.len(), 1);

        let next_mapping = gm.insert(b"another");
        assert_eq!(next_mapping, GlyphMapping::to_schart(1));

        gm.clear();
        assert!(gm.glyphs.is_empty());
        assert!(gm.index.is_empty());
        assert_eq!(gm.insert(&bytes), GlyphMapping::to_schart(0));
    }
    #[test]
    fn glyph_mapping_fullness_threshold() {
        let gm = GlyphMapping::new();
        assert!(!gm.is_full());
        assert!(!GlyphMapping::has_reached_clear_threshold(
            GlyphMapping::CLEAR_THRESHOLD - 1
        ));
        assert!(GlyphMapping::has_reached_clear_threshold(
            GlyphMapping::CLEAR_THRESHOLD
        ));
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
    fn glyph_mapping_resolve() {
        let mut gm = GlyphMapping::new();
        gm.insert(&[0x73, 0xcc, 0xb8, 0xcc, 0x89, 0xcc, 0xbf]);
        let test_cases = [
            ([0x41, 0, 0, 0], GlyphBuff::from_bytes(&[0x41])),
            ([0xFF, 1, 0, 0], GlyphBuff::from_bytes(&[])),
            (
                [0xFF, 0, 0, 0],
                GlyphBuff::from_bytes(&[0x73, 0xcc, 0xb8, 0xcc, 0x89, 0xcc, 0xbf]),
            ),
        ];
        for (t, e) in &test_cases {
            assert_eq!(gm.resolve(u32::from_ne_bytes(*t)).as_ref(), e);
        }
    }

    #[test]
    fn glyph_buffer_views() {
        let mut gm = GlyphMapping::new();

        let two_byte_schar = super::schar_from_bytes("é".as_bytes(), &mut gm);
        let two_byte_glyph = gm.resolve(two_byte_schar);
        assert_eq!(two_byte_glyph.as_bytes(), "é".as_bytes());
        assert_eq!(two_byte_glyph.as_c_str().to_bytes(), "é".as_bytes());

        let max_bytes = [b'a'; MAX_SCHAR_SIZE - 1];
        let max_schar = super::schar_from_bytes(&max_bytes, &mut gm);
        let max_glyph = gm.resolve(max_schar);
        assert_eq!(max_glyph.as_bytes(), max_bytes);
        assert_eq!(max_glyph.as_c_str().to_bytes(), max_bytes);
    }

    #[test]
    fn schar_cells_ascii_fast_path() {
        let mut gm = GlyphMapping::new();
        let sc = super::schar_from_bytes(b"A", &mut gm);
        assert_eq!(super::schar_cells_impl(sc, &gm), 1);
    }

    #[test]
    fn schar_get_first_codepoint() {
        let mut gm = GlyphMapping::new();
        let test_cases: &[(&[u8], libc::c_int)] = &[
            (b"", 0),
            (b"A", 0x41),
            ("é".as_bytes(), 0xe9),
            ("界".as_bytes(), 0x754c),
            ("界\u{301}".as_bytes(), 0x754c),
            (&[0xc2, 0x7f], 0xc2),
        ];

        for &(bytes, expected_codepoint) in test_cases {
            let sc = super::schar_from_bytes(bytes, &mut gm);
            assert_eq!(
                super::schar_get_first_codepoint_impl(sc, &gm),
                expected_codepoint
            );
        }
    }

    #[test]
    fn line_do_arabic_shape() {
        let mut gm = GlyphMapping::new();
        let mut line =
            ["س", "ل", "م"].map(|glyph| super::schar_from_bytes(glyph.as_bytes(), &mut gm));

        ArabicShaper::new(&mut gm).shape(&mut line);

        let actual = line.map(|sc| gm.resolve(sc).as_bytes().to_vec());
        let expected = ["ﺲ", "ﻠ", "ﻣ"].map(|glyph| glyph.as_bytes().to_vec());
        assert_eq!(actual, expected);
    }

    #[test]
    fn line_do_arabic_shape_discards_overflowing_codepoint() {
        let mut gm = GlyphMapping::new();
        let input = "م့̀́̂̃̄̅̆̇̈̉̊̋̌";
        assert_eq!(input.len(), MAX_SCHAR_SIZE - 1);
        let mut line = [super::schar_from_bytes(input.as_bytes(), &mut gm)];

        ArabicShaper::new(&mut gm).shape(&mut line);

        assert_eq!(gm.resolve(line[0]).as_bytes(), "ﻡ̀́̂̃̄̅̆̇̈̉̊̋̌".as_bytes());
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
            assert_eq!(super::schar_len_impl(u32::from_le_bytes(*t), &gm), *e);
        }
    }

    #[test]
    fn line_buffer_mirrors_a_nonzero_range() {
        let mut lb = LineBuffer::new(8);
        set_characters(&mut lb, &[9, 8, 1, 0, 2, 3, 7, 6]);
        set_attributes(&mut lb, &[10, 20, 30, 40, 50, 60, 70, 80]);
        set_virtual_columns(&mut lb, &[1, 2, 3, 4, 5, 6, 7, 8]);

        lb.mirror(2, 6, 8);

        assert_eq!(characters(&lb), [9, 8, 3, 2, 1, 0, 7, 6]);
        assert_eq!(attributes(&lb), [10, 20, 60, 50, 40, 30, 70, 80]);
        assert_eq!(virtual_columns(&lb), [1, 2, 6, 5, 4, 3, 7, 8]);
    }
}
