// vim: sw=4
mod cell;
mod coordinates;
mod ffi;
mod fold_column;
mod line_buffer;
mod line_submission;
mod message_surface;
mod popup_surface;
mod row;
mod store;
mod surface;
mod tabstops;
mod virtual_text;
mod window_surface;

pub(crate) use ffi::screen_character::schar_from_buf;

use crate::{
    context::registered_context,
    defines::OptInt,
    generated::enums::{RdbFlags, Slf},
    mbyte,
    ui::{
        self, GridOutputTarget,
        output::{OwnedRawLineCommand, UiCommand},
    },
};
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::BTreeMap,
    ffi::CStr,
    ops::Range,
    sync::{
        Mutex, MutexGuard,
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
use message_surface::{MessagePositionPolicy, MessagePositionRequest};
pub(crate) use message_surface::{MessageSurface, MessageSurfaceOutput};
pub(crate) use popup_surface::PopupMenuSurface;
use popup_surface::{
    PopupMenuAnchor, PopupMenuAnchorGrid, PopupMenuHideRequest, PopupMenuPositionEvent,
    PopupMenuPositionGeometry, PopupMenuPositionRequest, PopupMenuPublicationPolicy,
    PopupMenuSurfaceOutput, PopupMenuSurfaceRequest,
};
pub(crate) use row::{GridRow, GridRowSegment};
use store::StoredGridGuard;
pub(crate) use store::{GridStore, lock_grid_store};
pub(crate) use surface::GridSurfaceOutput;
use tabstops::TabStops;
use virtual_text::{VirtualText, VirtualTextChunk, VirtualTextRenderOptions};
pub(crate) use window_surface::WindowSurface;
use window_surface::{
    FloatAnchorPosition, WindowFloatAnchor, WindowFloatPlacementRequest,
    WindowFloatPlacementUpdate, WindowFloatPositionEvent, WindowSurfaceHideRequest,
    WindowSurfaceOutput, WindowSurfaceRequest, WindowSurfaceUpdate,
};

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
}

/// Storage mode selected by the window subsystem for one screen grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WindowGridMode {
    /// Render into storage owned by this window grid.
    Dedicated(GridSize),
    /// Render through a view into the shared default grid.
    Shared,
}

/// Persistent description used to construct a borrowed grid view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GridViewDescriptor {
    target: GridId,
    offset: GridPosition,
}

impl GridViewDescriptor {
    /// Describes a view of `target` beginning at `offset`.
    const fn new(target: GridId, offset: GridPosition) -> Self {
        Self { target, offset }
    }
}

/// Complete result of configuring a window grid and its view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WindowGridUpdate {
    grid_id: GridId,
    lifecycle: GridLifecycleChange,
    view: GridViewDescriptor,
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

/// Persistent state owned by the message-grid subsystem.
struct MessageGridState {
    view: GridViewDescriptor,
    scroll: MessageScrollState,
}

impl MessageGridState {
    /// Creates the legacy message view over the shared default grid with
    /// initial scroll bookkeeping.
    const fn new() -> Self {
        Self {
            view: GridViewDescriptor::new(DEFAULT_GRID_ID, GridPosition::new(0, 0)),
            scroll: MessageScrollState::new(),
        }
    }

    /// Returns the descriptor used to borrow the current target grid.
    const fn view(&self) -> GridViewDescriptor {
        self.view
    }

    /// Selects the allocated message grid without changing the current offset.
    fn activate_view(&mut self, grid_id: GridId) {
        self.view.target = grid_id;
    }

    /// Restores the legacy view over the shared default grid.
    ///
    /// The column offset is intentionally retained to preserve the previous
    /// transition exactly.
    fn reset_view(&mut self) {
        self.view.target = DEFAULT_GRID_ID;
        self.view.offset.row = 0;
    }

    /// Updates the row translation after moving the message grid.
    fn set_view_row_offset(&mut self, row_offset: i32) {
        self.view.offset.row = row_offset;
    }
}

/// Acquires the persistent message-grid state.
fn lock_message_grid_state() -> MutexGuard<'static, MessageGridState> {
    registered_context().grid().lock_message_state()
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
type CMessageGridPositionEmitter = unsafe extern "C" fn(
    grid_handle: VimHandleT,
    row: libc::c_int,
    scrolled: bool,
    separator_character: VimSCharT,
    z_index: libc::c_int,
    compositor_index: libc::c_int,
);

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

/// Acquires access to the context-owned glyph mapping.
fn lock_glyph_mapping() -> MutexGuard<'static, GlyphMapping> {
    registered_context().grid().lock_glyph_mapping()
}

const DEFAULT_GRID_HANDLE: VimHandleT = 1;
pub(crate) const DEFAULT_GRID_ID: GridId = GridId::new(DEFAULT_GRID_HANDLE);

/// Assigns monotonically increasing handles to newly created grids.
struct GridHandleAllocator {
    last: AtomicI32,
}

impl GridHandleAllocator {
    /// Creates an allocator whose next handle follows `last_assigned`.
    const fn new(last_assigned: GridId) -> Self {
        Self {
            last: AtomicI32::new(last_assigned.0),
        }
    }

    /// Allocates the next stable grid identity.
    fn allocate(&self) -> GridId {
        // Relaxed ordering is sufficient because the counter only provides
        // unique values; access to each grid is synchronized separately.
        let previous = self
            .last
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
                last.checked_add(1)
            })
            .expect("grid handle space exhausted");
        GridId::new(previous + 1)
    }
}

/// State owned by the grid subsystem for the lifetime of the editor.
pub(crate) struct GridContext {
    store: Mutex<GridStore>,
    message_state: Mutex<MessageGridState>,
    glyph_mapping: Mutex<GlyphMapping>,
    handle_allocator: GridHandleAllocator,
    line_buffer: Mutex<LineBuffer>,
}

impl GridContext {
    /// Constructs empty grid-subsystem state before grid initialization.
    pub(crate) fn new() -> Self {
        Self {
            store: Mutex::new(GridStore::new()),
            message_state: Mutex::new(MessageGridState::new()),
            glyph_mapping: Mutex::new(GlyphMapping::new()),
            handle_allocator: GridHandleAllocator::new(DEFAULT_GRID_ID),
            line_buffer: Mutex::new(LineBuffer::empty()),
        }
    }

    /// Acquires permanent screen-grid storage.
    pub(crate) fn lock_store(&self) -> MutexGuard<'_, GridStore> {
        self.store
            .try_lock()
            .expect("permanent grid store is already borrowed")
    }

    /// Acquires persistent message-grid state.
    fn lock_message_state(&self) -> MutexGuard<'_, MessageGridState> {
        self.message_state
            .try_lock()
            .expect("message-grid state is already borrowed")
    }

    /// Acquires the screen-character glyph mapping.
    fn lock_glyph_mapping(&self) -> MutexGuard<'_, GlyphMapping> {
        self.glyph_mapping
            .lock()
            .expect("glyph mapping mutex must not be poisoned")
    }

    /// Allocates the next stable screen-grid identity.
    fn allocate_grid_id(&self) -> GridId {
        self.handle_allocator.allocate()
    }

    /// Acquires the transitional shared line buffer.
    fn lock_line_buffer(&self) -> MutexGuard<'_, LineBuffer> {
        self.line_buffer
            .try_lock()
            .expect("line buffer is already borrowed")
    }
}

/// Allocates the next stable screen-grid identity.
fn allocate_grid_id() -> GridId {
    registered_context().grid().allocate_grid_id()
}

unsafe extern "C" {
    static mut full_screen: bool;
    static mut rdb_flags: libc::c_uint;

    static mut p_arshape: libc::c_int;
    static mut p_tbidi: libc::c_int;

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
    handle: VimHandleT,

    chars: Vec<VimSCharT>,
    attrs: Vec<VimSAttrT>,
    vcols: Vec<VimColNrT>,
    line_offset: Vec<usize>,

    /// Dirty-column tracking while output is throttled.
    ///
    /// `None` means that tracking is disabled for this grid.
    dirty_columns: Option<DirtyColumns>,

    /// The nonnegative dimensions of the grid.
    size: GridSize,

    /// The state of the grid is valid. Otherwise it needs to be redrawn.
    valid: bool,

    /// only draw internally and don't send updates yet to the compositor or
    /// external UI.
    throttled: bool,
}

impl ScreenGrid {
    /// Creates an identified grid with empty dimensions and cell storage.
    fn new_empty(grid_id: GridId) -> Self {
        Self {
            handle: grid_id.as_native_handle(),
            chars: Vec::new(),
            attrs: Vec::new(),
            vcols: Vec::new(),
            line_offset: Vec::new(),
            dirty_columns: None,
            size: GridSize {
                height: 0,
                width: 0,
            },
            valid: false,
            throttled: false,
        }
    }

    pub fn new(rows: i32, cols: i32, valid: bool, line_buffer: &mut LineBuffer) -> Self {
        let size =
            GridSize::try_new(rows, cols).expect("screen-grid dimensions must be nonnegative");
        let row_count =
            usize::try_from(size.height).expect("screen-grid height must fit native indices");
        let column_count =
            usize::try_from(size.width).expect("screen-grid width must fit native indices");
        let num_cells = row_count
            .checked_mul(column_count)
            .expect("screen-grid cell count must fit native indices");

        let mut grid = ScreenGrid {
            handle: 0,
            chars: vec![schar_from_ascii(' '); num_cells],
            attrs: vec![Self::attrs_fill(valid); num_cells],
            vcols: vec![-1; num_cells],
            line_offset: Vec::with_capacity(row_count),
            dirty_columns: None,
            size,
            valid: false,
            throttled: false,
        };
        for i in 0..row_count {
            let offset = i * column_count;
            grid.line_offset.push(offset);
        }

        grid.realloc_line_buffer(line_buffer);

        grid
    }

    /// Returns this grid's stable Rust identity.
    pub(crate) fn id(&self) -> GridId {
        GridId::new(self.handle)
    }

    /// Replaces this grid's identity in tests that construct standalone grids.
    #[cfg(test)]
    pub(crate) fn set_id_for_test(&mut self, grid_id: GridId) {
        assert!(grid_id.as_native_handle() > 0);
        self.handle = grid_id.as_native_handle();
    }

    /// Returns the nonnegative dimensions of this grid.
    pub(crate) const fn size(&self) -> GridSize {
        self.size
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

    fn from_grid(
        grid: &ScreenGrid,
        size: GridSize,
        valid: bool,
        copy: bool,
        line_buffer: &mut LineBuffer,
    ) -> Self {
        let mut new_grid = grid.clone();

        let row_count =
            usize::try_from(size.height).expect("screen-grid height must fit native indices");
        let column_count =
            usize::try_from(size.width).expect("screen-grid width must fit native indices");
        let num_cells = row_count
            .checked_mul(column_count)
            .expect("screen-grid cell count must fit native indices");

        new_grid.chars = vec![schar_from_ascii(' '); num_cells];
        new_grid.attrs = vec![Self::attrs_fill(valid); num_cells];
        new_grid.vcols = vec![-1; num_cells];
        new_grid.line_offset = Vec::with_capacity(row_count);
        new_grid.size = size;
        if let Some(dirty_columns) = &mut new_grid.dirty_columns {
            dirty_columns.resize(row_count);
        }

        for i in 0..row_count {
            let offset = i * column_count;
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
            WindowGridMode::Dedicated(size) if !self.is_allocated() || self.size != size => {
                let copy_contents = self.valid;
                *self = Self::from_grid(self, size, false, copy_contents, line_buffer);
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
            WindowGridMode::Dedicated(_) => self.id(),
            WindowGridMode::Shared => DEFAULT_GRID_ID,
        };
        let lifecycle = self.configure_for_window(mode, line_buffer);

        WindowGridUpdate {
            grid_id: self.id(),
            lifecycle,
            view: GridViewDescriptor::new(target, offset),
        }
    }

    /// Resizes the default grid when its configured dimensions have changed.
    fn configure_as_default(
        &mut self,
        size: GridSize,
        line_buffer: &mut LineBuffer,
    ) -> GridLifecycleChange {
        if size.height == 0 || size.width == 0 || (self.is_allocated() && self.size == size) {
            return GridLifecycleChange::Unchanged;
        }

        *self = Self::from_grid(self, size, true, true, line_buffer);
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
        if !self.is_allocated() || self.size != size {
            *self = Self::from_grid(self, size, false, !contents_invalid, line_buffer);
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
        line_buffer: &mut LineBuffer,
    ) -> MessageGridTransition {
        match mode {
            MessageGridMode::Visible(size) if !self.is_allocated() || self.size != size => {
                *self = Self::from_grid(self, size, true, false, line_buffer);
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

        let target = GridOutputTarget::new(self.id(), self.size.width);
        let scroll = self.del_lines(0, 1, self.size.height, 0, self.size.width);
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
            usize::try_from(self.size.width).expect("allocated grid width must fit native indices");
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
            usize::try_from(self.size.height)
                .expect("allocated grid height must fit native indices"),
        );
        let width =
            usize::try_from(self.size.width).expect("allocated grid width must fit native indices");
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
        let target = GridOutputTarget::new(self.id(), self.size.width);
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
        let line_delta = std::cmp::min(scrolled_delta, self.size.height);

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
                0 <= row && row < self.size.height,
                "dirty message row must belong to the message grid"
            );
            let dirty_column = self.take_dirty_column(row as usize).unwrap_or_default();
            plan.lines.push(ui::ui_line_r(
                self,
                row,
                false,
                0,
                dirty_column,
                self.size.width,
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
        let target = GridOutputTarget::new(self.id(), self.size.width);
        Some((target, UiCommand::GridResize(self.size)))
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
        let ucols = usize::try_from(self.size.width).expect("grid width must fit native indices");
        if ucols <= line_buffer.len() {
            return;
        }
        line_buffer.realloc(ucols);
    }

    fn copy_grid_content_from_old_grid(&mut self, src: &ScreenGrid, new_row: usize) {
        if new_row >= src.size.height as usize || src.chars.is_empty() {
            return;
        }
        let len = usize::try_from(std::cmp::min(self.size.width, src.size.width))
            .expect("grid width must fit native indices");
        let range = self.line_offset[new_row]..self.line_offset[new_row] + len;
        let src_range = src.line_offset[new_row]..src.line_offset[new_row] + len;
        self.chars[range.clone()].copy_from_slice(&src.chars[src_range.clone()]);
        self.attrs[range.clone()].copy_from_slice(&src.attrs[src_range.clone()]);
        self.vcols[range.clone()].copy_from_slice(&src.vcols[src_range.clone()]);
    }

    /// Enables dirty-column tracking and resets every row to clean.
    fn reset_dirty_columns(&mut self) {
        self.dirty_columns = Some(DirtyColumns::new(
            usize::try_from(self.size.height).expect("grid height must fit native indices"),
        ));
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
        let (Ok(row_count), Ok(width)) = (
            usize::try_from(self.size.height),
            usize::try_from(self.size.width),
        ) else {
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
        let Ok(row_count) = usize::try_from(self.size.height) else {
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

    /// Resolves logical cell coordinates into the shared storage index.
    fn cell_index(&self, row: usize, column: usize) -> Option<usize> {
        let width = usize::try_from(self.size.width).ok()?;
        if column >= width {
            return None;
        }
        let offset = self.line_offset.get(row)?.checked_add(column)?;
        (offset < self.chars.len() && offset < self.attrs.len() && offset < self.vcols.len())
            .then_some(offset)
    }

    /// Returns the complete contents of one logical grid cell.
    pub(crate) fn cell(&self, row: usize, column: usize) -> Option<GridCell> {
        let offset = self.cell_index(row, column)?;
        Some(GridCell::new(
            self.chars[offset],
            self.attrs[offset],
            self.vcols[offset],
        ))
    }

    /// Replaces one complete logical cell for tests outside the grid module.
    #[cfg(test)]
    pub(crate) fn set_cell_for_test(&mut self, row: usize, column: usize, cell: GridCell) -> bool {
        let Some(offset) = self.cell_index(row, column) else {
            return false;
        };
        self.chars[offset] = cell.character;
        self.attrs[offset] = cell.attribute;
        self.vcols[offset] = cell.virtual_column;
        true
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
        assert!(0 <= row && row < self.size.height);
        let is_right_left_set = flags & Slf::Rightleft as i32 != 0;
        let is_inc_vcol_set = flags & Slf::IncVcol as i32 != 0;
        let is_wrap_set = flags & Slf::Wrap as i32 != 0;

        let mut clear_next = false;
        if endcol > self.size.width {
            endcol = self.size.width;
        }
        if row >= self.size.height || col_offset >= self.size.width {
            return output;
        }
        let row = row as usize;
        let invalid_row =
            self.id() != GridId::new(DEFAULT_GRID_HANDLE) && self.is_invalid_row(row) && col == 0;
        let offset_to = self.line_offset[row] + col_offset as usize;
        let max_offset_to = self.line_offset[row] + self.size.width as usize;

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
            .size
            .width
            .checked_sub(request.view_origin.column)?
            .min(request.view_size.width);
        let visible_height = self
            .size
            .height
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

            if self.size.width != width {
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
                self.clear_line(temp, self.size.width as usize, false);
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

            if self.size.width != width {
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
                self.clear_line(temp, self.size.width as usize, false);
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
        window_grid_id: Option<GridId>,
    ) -> Result<Self, InvalidGridId> {
        let requested_grid_id = i32::try_from(requested_handle).ok().map(GridId::new);
        if requested_grid_id == Some(popup_grid_id) {
            return Ok(Self::PopupMenu);
        }
        if requested_handle <= i64::from(DEFAULT_GRID_HANDLE) {
            return Ok(Self::Default);
        }
        match requested_grid_id {
            Some(grid_id) if Some(grid_id) == window_grid_id => Ok(Self::Window(grid_id)),
            _ => Err(InvalidGridId),
        }
    }
}

/// Indicates that no supported grid identity matches an inspection request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InvalidGridId;

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

    /// Translates a target-grid position into coordinates relative to this
    /// view.
    fn local_position(&self, position: GridPosition) -> Option<GridPosition> {
        Some(GridPosition::new(
            position.row.checked_sub(self.offset.row)?,
            position.column.checked_sub(self.offset.column)?,
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
        let target = GridOutputTarget::new(self.target.id(), self.target.size().width);
        Some((target, UiCommand::GridCursorGoto(position)))
    }

    /// Reports whether the target grid has `width` columns.
    fn width_matches_target(&self, width: i32) -> bool {
        self.target.size().width == width
    }

    /// Reports whether this view has a target that requires a complete redraw.
    fn requires_redraw(&self) -> bool {
        self.target.requires_redraw()
    }
}

/// Creates an empty Rust-owned grid with a stable identity.
fn register_empty_grid(grid_id: GridId) {
    lock_grid_store()
        .register_empty(grid_id)
        .expect("new grid identity must be valid and unique");
}

/// Removes one permanently stored grid and retires its stable identity.
fn release_permanent_grid(grid_id: GridId) {
    lock_grid_store()
        .remove(grid_id)
        .expect("permanent grid identity must resolve to its Rust grid");
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
/// Exclusive access to the context-owned Rust line buffer.
struct LineBufferGuard {
    line_buffer: MutexGuard<'static, LineBuffer>,
}

impl LineBufferGuard {
    /// Borrows the shared line buffer for one non-reentrant rendering
    /// operation.
    fn acquire() -> Self {
        Self {
            line_buffer: registered_context().grid().lock_line_buffer(),
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
        self.grid.size.height
    }

    /// Returns the number of columns in the borrowed grid.
    fn cols(&self) -> i32 {
        self.grid.size.width
    }

    /// Begins buffering a line belonging to the borrowed Rust grid.
    fn begin_grid(&mut self, row: i32, col: i32, line_buf: &mut LineBuffer) {
        self.state.begin(self.grid.size.width, row, col, line_buf);
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

    /// Schedules a half-open range to be cleared when the line is committed.
    fn set_clear_range(
        &mut self,
        start_col: i32,
        end_col: i32,
        background_attribute: VimSAttrT,
        clear_attribute: VimSAttrT,
    ) {
        self.state
            .set_clear_range(start_col, end_col, background_attribute, clear_attribute);
    }

    /// Ends the current line operation and returns its UI output.
    fn flush(&mut self, line_buf: &mut LineBuffer) -> Vec<OwnedRawLineCommand> {
        if self.state.finish() {
            self.state.write_line(&mut *self.grid, line_buf)
        } else {
            Vec::new()
        }
    }

    /// Returns the final usable column of the active line.
    fn max_col(&self) -> i32 {
        self.state.max_col
    }

    /// Reports whether the active row reaches or exceeds the borrowed grid.
    fn row_reaches_grid_end(&self) -> bool {
        self.state.row >= self.grid.size.height
    }

    /// Discards the active operation without writing buffered cells.
    fn cancel(&mut self) {
        self.state.active = false;
    }
}

/// Adapter for the legacy C API, whose line operation spans several calls.
///
/// C cannot retain a Rust borrow between those calls, so this adapter stores
/// the target's stable identity and borrows the permanent grid only for each
/// complete operation. All implementation state remains in `LineState`.
struct CLineGrid {
    target: Option<GridId>,
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

    /// Returns the active grid identity.
    fn target(&self) -> GridId {
        self.target.expect("line operation has no target grid")
    }

    /// Begins a line operation on a permanently stored grid.
    ///
    /// Must be matched with a grid_line_flush call before moving to another
    /// line. The matching flush must occur on the same thread.
    fn begin(&mut self, grid_id: GridId, row: i32, col: i32, line_buf: &mut LineBuffer) {
        assert!(self.target.is_none());
        let cols = StoredGridGuard::acquire(grid_id)
            .expect("line grid identity must resolve to permanent storage")
            .size()
            .width;
        self.state.begin(cols, row, col, line_buf);
        self.target = Some(grid_id);
    }

    /// Reads a cell from the active permanently stored grid.
    fn read_cell(&mut self, col: i32) -> (VimSCharT, VimSAttrT) {
        let grid = StoredGridGuard::acquire(self.target())
            .expect("line grid identity must resolve to permanent storage");
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
        let grid = StoredGridGuard::acquire(self.target())
            .expect("line grid identity must resolve to permanent storage");
        let output_target = GridOutputTarget::new(grid.id(), grid.size().width);
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
        let grid_id = self
            .target
            .take()
            .expect("line operation has no target grid");
        if !self.state.finish() {
            return None;
        }

        let (output_target, commands) = {
            let mut grid = StoredGridGuard::acquire(grid_id)
                .expect("line grid identity must resolve to permanent storage");
            let output_target = GridOutputTarget::new(grid.id(), grid.size().width);
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
        let rows = StoredGridGuard::acquire(self.target())
            .expect("line grid identity must resolve to permanent storage")
            .size()
            .height;
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
        let rows = StoredGridGuard::acquire(self.target())
            .expect("line grid identity must resolve to permanent storage")
            .size()
            .height;
        self.state.row >= rows
    }

    /// Discards the active operation without writing its buffered contents.
    fn cancel(&mut self) {
        self.target = None;
        self.state.active = false;
    }
}

/// Converts ascii char to `VimSCharT`
pub fn schar_from_ascii(c: char) -> VimSCharT {
    if cfg!(target_endian = "big") {
        return (c as u32).to_be();
    }
    (c as u32).to_le()
}

/// Converts one Unicode codepoint into an inline screen character.
pub(crate) fn schar_from_char(mut c: i32) -> VimSCharT {
    if c >= 0x200000 {
        // TODO(bfredl): this must NEVER happen, even if the file contained overlong sequences
        c = 0xFFFD;
    }
    let mut bytes = (0 as VimSCharT).to_le_bytes();
    let encoded = mbyte::EncodedCodepoint::new(c);
    bytes[..encoded.as_bytes().len()].copy_from_slice(encoded.as_bytes());
    VimSCharT::from_le_bytes(bytes)
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

/// C representation of a pending message-grid lifecycle transition.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CMessageGridTransition {
    Unchanged = 0,
    Resized = 1,
    ReleaseRequired = 2,
}

/// C representation of message-position UI policy.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CMessageGridPositionConfig {
    separator_character: VimSCharT,
    z_index: libc::c_int,
}

/// C representation of one upward message-surface scroll request.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CMessageSurfaceScrollRequest {
    grid_handle: VimHandleT,
    current_position: libc::c_int,
    screen_rows: libc::c_int,
    screen_columns: libc::c_int,
    enable_throttling: bool,
    zero_cmdheight: bool,
    discount_first_scroll: bool,
    position: CMessageGridPositionConfig,
    clear_attribute: VimSAttrT,
}

/// C representation of one complete message-surface lifecycle request.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CMessageSurfaceRequest {
    grid_handle: VimHandleT,
    screen_rows: libc::c_int,
    screen_columns: libc::c_int,
    should_allocate: bool,
    scrolled_lines: libc::c_int,
    pager_active: bool,
    command_height: libc::c_int,
    current_position: libc::c_int,
    position: CMessageGridPositionConfig,
    blending: bool,
    current_window_grid_handle: VimHandleT,
    clear_attribute: VimSAttrT,
}

/// C representation of editor-owned state following a message-surface update.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CMessageSurfaceUpdate {
    grid_handle: VimHandleT,
    position: libc::c_int,
    redraw_cmdline: bool,
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

#[cfg(test)]
mod tests {
    use super::ffi::lifecycle::{
        CWindowSurfaceRequest, grid_configure_window_surface, grid_free, grid_is_allocated,
        grid_require_full_redraw, grid_requires_compositor_invalidation,
        grid_revalidate_for_redraw,
    };
    use super::ffi::message::{grid_prepare_for_messages, grid_release_message};
    use super::ffi::operations::{grid_assign_handle, grid_inspect_cell, grid_invalidate};
    use super::ffi::view::{grid_view_has_target, grid_view_targets_default};
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

    fn configure_stored_popup_grid(
        grid_handle: i32,
        rows: i32,
        columns: i32,
        contents_invalid: bool,
    ) -> bool {
        let size = GridSize::try_new(rows, columns).unwrap();
        let mut line_buffer = LineBufferGuard::acquire();
        let mut grid = StoredGridGuard::acquire(GridId::new(grid_handle)).unwrap();
        grid.configure_for_popup_menu(size, contents_invalid, &mut line_buffer)
            == GridLifecycleChange::Resized
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
    fn grid_handle_allocator_returns_monotonic_identities() {
        let allocator = GridHandleAllocator::new(DEFAULT_GRID_ID);

        assert_eq!(allocator.allocate(), GridId::new(DEFAULT_GRID_HANDLE + 1));
        assert_eq!(allocator.allocate(), GridId::new(DEFAULT_GRID_HANDLE + 2));
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
            GridViewDescriptor::new(GridId::new(7), GridPosition::new(1, 2))
        );
        assert_eq!(grid.handle, 7);
        assert_eq!(grid.size(), GridSize::try_new(2, 3).unwrap());
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
            GridViewDescriptor::new(DEFAULT_GRID_ID, GridPosition::new(3, 4))
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
    fn window_grid_updates_resolve_permanent_rust_storage() {
        {
            let mut store = lock_grid_store();
            if store.grid(DEFAULT_GRID_ID).is_err() {
                store.register_empty(DEFAULT_GRID_ID).unwrap();
            }
        }
        let assigned_handle = grid_assign_handle();
        let grid_id = GridId::new(assigned_handle);
        let shared_update = grid_configure_window_surface(CWindowSurfaceRequest {
            grid_handle: assigned_handle,
            rows: 0,
            columns: 0,
            use_dedicated_grid: false,
            row_offset: 3,
            column_offset: 4,
            force_dimensions_publication: false,
            bordered_float: false,
        });

        assert_eq!(grid_id, GridId::new(assigned_handle));
        assert_eq!(shared_update.grid_handle, assigned_handle);
        assert!(shared_update.row_offset_changed);
        assert!(!shared_update.redraw_border);
        assert!(!shared_update.republish_cursor);
        assert!(grid_view_has_target(assigned_handle));
        assert!(grid_view_targets_default(assigned_handle));
        assert_eq!(
            lock_grid_store().window_view_descriptor(grid_id).unwrap(),
            GridViewDescriptor::new(DEFAULT_GRID_ID, GridPosition::new(3, 4))
        );
        {
            let grid = StoredGridGuard::acquire(grid_id).unwrap();
            assert!(!grid.is_allocated());
        }

        let dedicated_update = grid_configure_window_surface(CWindowSurfaceRequest {
            grid_handle: assigned_handle,
            rows: 2,
            columns: 3,
            use_dedicated_grid: true,
            row_offset: 1,
            column_offset: 2,
            force_dimensions_publication: false,
            bordered_float: true,
        });

        assert_eq!(dedicated_update.grid_handle, assigned_handle);
        assert!(dedicated_update.row_offset_changed);
        assert!(dedicated_update.redraw_border);
        assert!(dedicated_update.republish_cursor);
        assert!(!grid_view_targets_default(assigned_handle));
        assert_eq!(
            lock_grid_store().window_view_descriptor(grid_id).unwrap(),
            GridViewDescriptor::new(grid_id, GridPosition::new(1, 2))
        );

        {
            let mut grid = StoredGridGuard::acquire(grid_id).unwrap();
            assert_eq!(grid.size(), GridSize::try_new(2, 3).unwrap());
            assert!(grid.is_allocated());
            grid.attrs.fill(7);
        }

        let forced_update = grid_configure_window_surface(CWindowSurfaceRequest {
            grid_handle: assigned_handle,
            rows: 2,
            columns: 3,
            use_dedicated_grid: true,
            row_offset: 1,
            column_offset: 2,
            force_dimensions_publication: true,
            bordered_float: true,
        });
        assert!(!forced_update.row_offset_changed);
        assert!(!forced_update.redraw_border);
        assert!(forced_update.republish_cursor);

        grid_invalidate(assigned_handle);
        {
            let grid = StoredGridGuard::acquire(grid_id).unwrap();
            assert!(grid.attrs.iter().all(|&attribute| attribute == -1));
        }

        grid_free(assigned_handle);
        assert!(StoredGridGuard::acquire(grid_id).is_err());
        assert_eq!(
            lock_grid_store().window_view_descriptor(grid_id),
            Err(store::GridStoreError::MissingWindowView(grid_id))
        );
    }

    #[test]
    fn retired_window_grid_identity_is_not_reused() {
        let first_handle = grid_assign_handle();
        grid_free(first_handle);

        assert!(StoredGridGuard::acquire(GridId::new(first_handle)).is_err());

        let second_handle = grid_assign_handle();

        assert_ne!(second_handle, first_handle);
        assert!(StoredGridGuard::acquire(GridId::new(second_handle)).is_ok());

        grid_free(second_handle);
    }

    #[test]
    fn default_grid_configuration_owns_identity_and_resize_decisions() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new_empty(DEFAULT_GRID_ID);
        assert_eq!(
            grid.configure_as_default(GridSize::try_new(1, 2).unwrap(), &mut line_buffer),
            GridLifecycleChange::Resized
        );
        grid.chars[0] = schar_from_ascii('x');
        grid.attrs[0] = 12;

        let change = grid.configure_as_default(GridSize::try_new(2, 3).unwrap(), &mut line_buffer);

        assert_eq!(change, GridLifecycleChange::Resized);
        assert_eq!(grid.handle, DEFAULT_GRID_HANDLE);
        assert_eq!(grid.size(), GridSize::try_new(2, 3).unwrap());
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
        assert_eq!(grid.size(), GridSize::try_new(2, 3).unwrap());
        assert_eq!(grid.chars[0], schar_from_ascii('x'));
        assert_eq!(grid.attrs[0], 12);
        assert_eq!(grid.attrs[2], -1);

        grid.release_storage();

        assert_eq!(grid.id(), GridId::new(9));
        assert!(!grid.is_allocated());
        assert_eq!(
            grid.configure_for_popup_menu(
                GridSize::try_new(2, 3).unwrap(),
                false,
                &mut line_buffer,
            ),
            GridLifecycleChange::Resized
        );
        assert_eq!(grid.id(), GridId::new(9));
        assert!(grid.is_allocated());
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
    fn popup_grid_storage_release_retains_permanent_identity() {
        let grid_handle = grid_assign_handle();

        assert!(configure_stored_popup_grid(grid_handle, 2, 3, false));
        {
            let grid = StoredGridGuard::acquire(GridId::new(grid_handle)).unwrap();
            assert_eq!(grid.size(), GridSize::try_new(2, 3).unwrap());
            assert!(grid.is_allocated());
        }

        StoredGridGuard::acquire(GridId::new(grid_handle))
            .unwrap()
            .release_storage();
        assert!(!grid_is_allocated(grid_handle));

        release_permanent_grid(GridId::new(grid_handle));
    }

    #[test]
    fn message_grid_preparation_allocates_clean_tracked_storage() {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, false, &mut line_buffer);
        grid.chars[0] = schar_from_ascii('x');
        grid.attrs[0] = 12;
        grid.throttled = true;
        grid.handle = DEFAULT_GRID_HANDLE + 1;

        let transition = grid.prepare_for_messages(
            MessageGridMode::Visible(GridSize::try_new(2, 3).unwrap()),
            &mut line_buffer,
        );

        assert_eq!(transition, MessageGridTransition::Resized);
        assert_eq!(grid.handle, DEFAULT_GRID_HANDLE + 1);
        assert_eq!(grid.size(), GridSize::try_new(2, 3).unwrap());
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

        let transition = grid.prepare_for_messages(MessageGridMode::Hidden, &mut line_buffer);

        assert_eq!(transition, MessageGridTransition::ReleaseRequired);
        assert!(grid.is_allocated());
        assert!(grid.throttled);

        grid.release_message();

        assert!(!grid.is_allocated());
        assert!(!grid.throttled);
        assert_eq!(
            grid.prepare_for_messages(MessageGridMode::Hidden, &mut line_buffer),
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
    fn cell_inspection_interprets_grid_handles() {
        let popup_grid_id = GridId::new(7);
        let window_grid_id = GridId::new(8);
        assert_eq!(
            GridInspectionTarget::from_requested_handle(1, popup_grid_id, Some(window_grid_id)),
            Ok(GridInspectionTarget::Default)
        );
        assert_eq!(
            GridInspectionTarget::from_requested_handle(7, popup_grid_id, Some(window_grid_id)),
            Ok(GridInspectionTarget::PopupMenu)
        );
        assert_eq!(
            GridInspectionTarget::from_requested_handle(8, popup_grid_id, Some(window_grid_id)),
            Ok(GridInspectionTarget::Window(GridId::new(8)))
        );
        assert_eq!(
            GridInspectionTarget::from_requested_handle(9, popup_grid_id, Some(window_grid_id)),
            Err(InvalidGridId)
        );
        assert_eq!(
            GridInspectionTarget::from_requested_handle(8, popup_grid_id, None),
            Err(InvalidGridId)
        );
        assert_eq!(
            GridInspectionTarget::from_requested_handle(
                i64::from(i32::MAX) + 1,
                popup_grid_id,
                Some(window_grid_id),
            ),
            Err(InvalidGridId)
        );
    }

    #[test]
    fn cell_inspection_reads_a_permanently_stored_window_grid() {
        let grid_handle = grid_assign_handle();
        assert!(configure_stored_popup_grid(grid_handle, 1, 1, false));
        {
            let mut grid = StoredGridGuard::acquire(GridId::new(grid_handle)).unwrap();
            grid.chars[0] = schar_from_ascii('x');
            grid.attrs[0] = 12;
            grid.vcols[0] = 34;
        }
        let mut cell = CGridCell::default();

        // SAFETY: `cell` is valid writable storage.
        let result = unsafe {
            grid_inspect_cell(0, grid_handle, i64::from(grid_handle), 0, 0, &raw mut cell)
        };

        assert_eq!(result, CGridCellInspectionResult::Found);
        assert_eq!(
            cell,
            CGridCell {
                character: schar_from_ascii('x'),
                attribute: 12,
                virtual_column: 34,
            }
        );

        grid_free(grid_handle);
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

        assert_eq!(
            view.local_position(GridPosition::new(2, 3)),
            Some(GridPosition::new(1, 2))
        );
        assert_eq!(view.local_position(GridPosition::new(i32::MIN, 0)), None);
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
    fn stored_grid_reports_whether_cell_buffers_are_allocated() {
        let grid_handle = grid_assign_handle();

        assert!(!grid_is_allocated(grid_handle));
        assert!(configure_stored_popup_grid(grid_handle, 1, 1, false));
        assert!(grid_is_allocated(grid_handle));
        assert!(grid_requires_compositor_invalidation(grid_handle, false));
        grid_revalidate_for_redraw(grid_handle);
        assert!(!grid_requires_compositor_invalidation(grid_handle, false));
        assert!(grid_requires_compositor_invalidation(grid_handle, true));
        grid_require_full_redraw(grid_handle);
        assert!(grid_requires_compositor_invalidation(grid_handle, false));

        {
            let mut grid = StoredGridGuard::acquire(GridId::new(grid_handle)).unwrap();
            grid.release_storage();
        }
        assert!(!grid_is_allocated(grid_handle));

        grid_free(grid_handle);
        assert!(!grid_is_allocated(grid_handle));
        assert!(!grid_is_allocated(0));
        assert!(grid_requires_compositor_invalidation(grid_handle, false));
        grid_revalidate_for_redraw(grid_handle);
        grid_require_full_redraw(grid_handle);
    }

    #[test]
    fn assigned_message_grid_remains_permanent_through_preparation() {
        let mut line_buffer = LineBuffer::new(3);
        let assigned_handle = grid_assign_handle();
        let grid_id = GridId::new(assigned_handle);

        let transition = grid_prepare_for_messages(assigned_handle, 2, 3, true, 0);

        assert_eq!(grid_id, GridId::new(assigned_handle));
        assert_eq!(transition, CMessageGridTransition::Resized);
        assert!(grid_is_allocated(assigned_handle));

        {
            let mut grid = StoredGridGuard::acquire(grid_id).unwrap();
            grid.chars[0] = schar_from_ascii('x');
            assert_eq!(grid.size(), GridSize::try_new(2, 3).unwrap());
        }

        {
            let grid = StoredGridGuard::acquire(grid_id).unwrap();
            assert_eq!(grid.chars[0], schar_from_ascii('x'));
        }

        let mut line_grid = CLineGrid::new();
        line_grid.begin(grid_id, 0, 0, &mut line_buffer);
        assert_eq!(line_grid.max_col(), 3);
        assert!(!line_grid.row_reaches_grid_end());
        line_grid.cancel();

        // Releasing the message contents must not surrender permanent Rust
        // ownership or invalidate its stable identity.
        grid_release_message(assigned_handle);
        assert!(StoredGridGuard::acquire(grid_id).is_ok());
        assert!(!grid_is_allocated(assigned_handle));

        release_permanent_grid(grid_id);
    }

    #[test]
    fn message_grid_state_preserves_view_offsets() {
        let mut state = MessageGridState {
            view: GridViewDescriptor::new(DEFAULT_GRID_ID, GridPosition::new(-3, 2)),
            scroll: MessageScrollState::new(),
        };

        state.activate_view(GridId::new(7));
        assert_eq!(
            state.view(),
            GridViewDescriptor::new(GridId::new(7), GridPosition::new(-3, 2))
        );

        state.set_view_row_offset(-5);
        state.reset_view();
        assert_eq!(
            state.view(),
            GridViewDescriptor::new(DEFAULT_GRID_ID, GridPosition::new(0, 2))
        );
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
