//! C boundary adapters for screen-grid lifecycle operations.

use crate::{
    context::registered_context,
    defines::{CVimString, Integer},
    globals,
    ui::output::{RawLineCells, RawLineCommand, UiCommand, UiOutput},
    ui_compositor::{
        CompositorGridPlacement, compositor_runtime_snapshot, current_message_grid_id,
    },
};

use super::super::{
    ComposedGridSpace, DEFAULT_GRID_ID, FloatAnchorPosition, GlyphMapping, GridId,
    GridLifecycleChange, GridPosition, GridSize, GridSurfaceOutput, LineBufferGuard,
    MessageGridState, StoredGridGuard, VimHandleT, WindowFloatAnchor, WindowFloatPlacementRequest,
    WindowFloatPlacementUpdate, WindowFloatPositionEvent, WindowGridMode, WindowSurfaceHideRequest,
    WindowSurfaceOutput, WindowSurfaceRequest, WindowSurfaceUpdate, lock_glyph_mapping,
    lock_grid_store, lock_message_grid_state, register_empty_grid, release_permanent_grid,
};

use super::surface::NvimGridSurfaceOutput;

unsafe extern "C" {
    fn ui_call_win_float_pos(
        grid: Integer,
        window: Integer,
        anchor: CVimString,
        anchor_grid: Integer,
        anchor_row: f64,
        anchor_column: f64,
        mouse_enabled: bool,
        z_index: Integer,
        compositor_index: Integer,
        screen_row: Integer,
        screen_column: Integer,
    );
    fn ui_call_win_hide(grid: Integer);
}

/// C representation of one complete window-grid configuration request.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CWindowSurfaceRequest {
    pub(crate) grid_handle: VimHandleT,
    pub(crate) rows: libc::c_int,
    pub(crate) columns: libc::c_int,
    pub(crate) use_dedicated_grid: bool,
    pub(crate) row_offset: libc::c_int,
    pub(crate) column_offset: libc::c_int,
    pub(crate) force_dimensions_publication: bool,
    pub(crate) bordered_float: bool,
}

impl CWindowSurfaceRequest {
    /// Converts C-owned window state into a validated native request.
    fn try_into_native(self) -> Option<WindowSurfaceRequest> {
        if self.grid_handle <= 0 {
            return None;
        }
        let mode = if self.use_dedicated_grid {
            WindowGridMode::Dedicated(GridSize::try_new(self.rows, self.columns)?)
        } else {
            WindowGridMode::Shared
        };

        Some(WindowSurfaceRequest::new(
            GridId::new(self.grid_handle),
            mode,
            GridPosition::new(self.row_offset, self.column_offset),
            self.force_dimensions_publication,
            self.bordered_float,
        ))
    }
}

/// C representation of editor-owned actions after window configuration.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CWindowSurfaceUpdate {
    pub(crate) grid_handle: VimHandleT,
    pub(crate) row_offset_changed: bool,
    pub(crate) redraw_border: bool,
    pub(crate) republish_cursor: bool,
}

impl CWindowSurfaceUpdate {
    /// Returns the failure result expected by the existing C caller.
    const fn invalid() -> Self {
        Self {
            grid_handle: 0,
            row_offset_changed: false,
            redraw_border: false,
            republish_cursor: false,
        }
    }
}

impl From<WindowSurfaceUpdate> for CWindowSurfaceUpdate {
    fn from(update: WindowSurfaceUpdate) -> Self {
        Self {
            grid_handle: update.grid_id.as_native_handle(),
            row_offset_changed: update.row_offset_changed,
            redraw_border: update.redraw_border,
            republish_cursor: update.republish_cursor,
        }
    }
}

/// C representation of one complete visible floating-window placement.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CWindowFloatPlacementRequest {
    grid_handle: VimHandleT,
    window_handle: VimHandleT,
    row: libc::c_int,
    column: libc::c_int,
    rows: libc::c_int,
    columns: libc::c_int,
    z_index: libc::c_int,
    blending: bool,
    valid: bool,
    accepts_mouse_input: bool,
    current_window_grid_handle: VimHandleT,
    anchor: libc::c_int,
    anchor_grid_handle: VimHandleT,
    anchor_row: f64,
    anchor_column: f64,
    multigrid_enabled: bool,
}

impl CWindowFloatPlacementRequest {
    /// Converts C-owned floating-window state into a validated native request.
    fn try_into_native(self) -> Option<WindowFloatPlacementRequest> {
        if self.grid_handle <= 0 || self.window_handle <= 0 || self.anchor_grid_handle <= 0 {
            return None;
        }
        let current_window_grid_id = match self.current_window_grid_handle {
            0 => None,
            handle if handle > 0 => Some(GridId::new(handle)),
            _ => return None,
        };
        let anchor = match self.anchor {
            0 => WindowFloatAnchor::NorthWest,
            1 => WindowFloatAnchor::NorthEast,
            2 => WindowFloatAnchor::SouthWest,
            3 => WindowFloatAnchor::SouthEast,
            _ => return None,
        };

        Some(WindowFloatPlacementRequest::new(
            GridId::new(self.grid_handle),
            self.window_handle,
            CompositorGridPlacement::try_new(self.row, self.column, self.rows, self.columns)?,
            self.z_index,
            self.blending,
            self.valid,
            self.accepts_mouse_input,
            current_window_grid_id,
            anchor,
            GridId::new(self.anchor_grid_handle),
            FloatAnchorPosition::try_new(self.anchor_row, self.anchor_column)?,
            self.multigrid_enabled,
        ))
    }
}

/// C representation of editor-owned actions after float placement.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CWindowFloatPlacementUpdate {
    republish_cursor: bool,
    require_full_redraw: bool,
}

impl From<WindowFloatPlacementUpdate> for CWindowFloatPlacementUpdate {
    fn from(update: WindowFloatPlacementUpdate) -> Self {
        Self {
            republish_cursor: update.republish_cursor,
            require_full_redraw: update.require_full_redraw,
        }
    }
}

/// Final UI adapter for complete window-surface operations.
struct NvimWindowSurfaceOutput {
    surface: NvimGridSurfaceOutput,
}

impl NvimWindowSurfaceOutput {
    /// Creates an output adapter for one synchronous window operation.
    const fn new() -> Self {
        Self {
            surface: NvimGridSurfaceOutput::new(),
        }
    }
}

impl UiOutput<ComposedGridSpace> for NvimWindowSurfaceOutput {
    fn emit(&mut self, command: UiCommand<ComposedGridSpace>) {
        self.surface.emit(command);
    }

    fn emit_raw_line<Cells: RawLineCells>(&mut self, command: RawLineCommand<Cells>) {
        self.surface.emit_raw_line(command);
    }
}

impl GridSurfaceOutput for NvimWindowSurfaceOutput {
    fn emit_grid_resize(&mut self, grid_id: GridId, size: GridSize) {
        self.surface.emit_grid_resize(grid_id, size);
    }

    fn emit_grid_destroy(&mut self, grid_id: GridId) {
        self.surface.emit_grid_destroy(grid_id);
    }
}

impl WindowSurfaceOutput for NvimWindowSurfaceOutput {
    fn emit_float_position(&mut self, event: WindowFloatPositionEvent) {
        let anchor = match event.anchor {
            WindowFloatAnchor::NorthWest => b"NW",
            WindowFloatAnchor::NorthEast => b"NE",
            WindowFloatAnchor::SouthWest => b"SW",
            WindowFloatAnchor::SouthEast => b"SE",
        };
        let anchor = CVimString {
            data: anchor.as_ptr().cast::<libc::c_char>().cast_mut(),
            size: anchor.len(),
        };
        let compositor_index = Integer::try_from(event.compositor_index)
            .expect("compositor layer index must fit the UI representation");

        // SAFETY: The anchor bytes remain readable for the complete
        // synchronous callback, and safe Rust validated all event fields.
        unsafe {
            ui_call_win_float_pos(
                Integer::from(event.grid_id.as_native_handle()),
                Integer::from(event.window_handle),
                anchor,
                Integer::from(event.anchor_grid_id.as_native_handle()),
                event.anchor_position.row,
                event.anchor_position.column,
                event.accepts_mouse_input,
                Integer::from(event.z_index),
                compositor_index,
                Integer::from(event.screen_position.row),
                Integer::from(event.screen_position.column),
            );
        }
    }

    fn emit_window_hide(&mut self, grid_id: GridId) {
        // SAFETY: Safe Rust validated the identity, and the generated callback
        // is synchronous.
        unsafe {
            ui_call_win_hide(Integer::from(grid_id.as_native_handle()));
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn alloc_initial_line_buffer() {
    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.realloc(unsafe { globals::Columns as usize });
}

/// Reports whether a stored grid currently owns cell buffers.
///
/// Missing, invalid, and retired identities are not allocated.
#[unsafe(no_mangle)]
pub extern "C" fn grid_is_allocated(grid_handle: VimHandleT) -> bool {
    if grid_handle <= 0 {
        return false;
    }

    StoredGridGuard::acquire(GridId::new(grid_handle)).is_ok_and(|grid| grid.is_allocated())
}

/// Reports whether the current screen update must invalidate composed output.
///
/// This combines the requested full-clear state with the stored grid's redraw
/// state. A missing identity conservatively requires compositor invalidation.
#[unsafe(no_mangle)]
pub extern "C" fn grid_requires_compositor_invalidation(
    grid_handle: VimHandleT,
    clear_requested: bool,
) -> bool {
    let Ok(grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
        return true;
    };

    grid.requires_compositor_invalidation(clear_requested)
}

/// Revalidates invalid grid contents in preparation for a complete redraw.
///
/// A grid that does not require a redraw remains unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn grid_revalidate_for_redraw(grid_handle: VimHandleT) {
    if let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) {
        grid.revalidate_for_redraw();
    }
}

/// Records that a grid requires a complete redraw.
#[unsafe(no_mangle)]
pub extern "C" fn grid_require_full_redraw(grid_handle: VimHandleT) {
    if let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) {
        grid.require_full_redraw();
    }
}

/// Assigns and permanently stores the default screen grid.
#[unsafe(no_mangle)]
pub extern "C" fn grid_initialize_default() {
    register_empty_grid(DEFAULT_GRID_ID);

    *lock_message_grid_state() = MessageGridState::new();
}

/// Resizes the default screen grid when its configured dimensions changed.
///
/// Returns whether a resize was performed.
///
#[unsafe(no_mangle)]
pub extern "C" fn grid_configure_default(rows: libc::c_int, columns: libc::c_int) -> bool {
    let Some(size) = GridSize::try_new(rows, columns) else {
        return false;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    let Ok(mut grid) = StoredGridGuard::acquire(DEFAULT_GRID_ID) else {
        return false;
    };
    grid.configure_as_default(size, &mut line_buffer) == GridLifecycleChange::Resized
}

/// Configures one complete window surface and applies its resize output.
#[unsafe(no_mangle)]
pub extern "C" fn grid_configure_window_surface(
    request: CWindowSurfaceRequest,
) -> CWindowSurfaceUpdate {
    let Some(request) = request.try_into_native() else {
        return CWindowSurfaceUpdate::invalid();
    };
    let mut output = NvimGridSurfaceOutput::new();

    registered_context()
        .window_surface()
        .configure(request, &mut output)
        .map(CWindowSurfaceUpdate::from)
        .unwrap_or_else(|_| CWindowSurfaceUpdate::invalid())
}

/// Places one visible floating-window surface and publishes its position.
#[unsafe(no_mangle)]
pub extern "C" fn grid_place_window_float_surface(
    request: CWindowFloatPlacementRequest,
) -> CWindowFloatPlacementUpdate {
    let request = request
        .try_into_native()
        .expect("floating-window placement must contain valid editor state");
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let mut output = NvimWindowSurfaceOutput::new();

    registered_context()
        .window_surface()
        .place_float(request, current_message_grid_id(), runtime, &mut output)
        .expect("floating-window placement must preserve rendering invariants")
        .into()
}

/// Hides one window surface and removes it from the compositor.
#[unsafe(no_mangle)]
pub extern "C" fn grid_hide_window_surface(grid_handle: VimHandleT, publish_hide: bool) {
    assert!(
        grid_handle > 0,
        "window surface must have a valid grid identity"
    );
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let mut output = NvimWindowSurfaceOutput::new();

    registered_context()
        .window_surface()
        .hide(
            WindowSurfaceHideRequest {
                grid_id: GridId::new(grid_handle),
                message_grid_id: current_message_grid_id(),
                publish_hide,
                runtime,
            },
            &mut output,
        )
        .expect("window hiding must preserve rendering invariants");
}

/// Releases one permanently stored grid by its stable identity.
#[unsafe(no_mangle)]
pub extern "C" fn grid_free(grid_handle: VimHandleT) {
    release_permanent_grid(GridId::new(grid_handle));
}

/// Releases all process-global grid resources during editor shutdown.
#[unsafe(no_mangle)]
pub extern "C" fn grid_free_all_mem() {
    lock_grid_store().clear();

    *lock_message_grid_state() = MessageGridState::new();

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
#[unsafe(no_mangle)]
pub extern "C" fn grid_clear_default(valid: bool) -> bool {
    let Ok(mut grid) = StoredGridGuard::acquire(DEFAULT_GRID_ID) else {
        return false;
    };
    grid.clear_all(valid)
}

/// Clears a half-open range of complete logical rows.
///
/// Rows beyond the allocated grid are ignored.
///
#[unsafe(no_mangle)]
pub extern "C" fn grid_clear_default_rows(
    start_row: libc::c_int,
    end_row: libc::c_int,
    valid: bool,
) {
    let (Ok(start_row), Ok(end_row)) = (usize::try_from(start_row), usize::try_from(end_row))
    else {
        return;
    };

    if let Ok(mut grid) = StoredGridGuard::acquire(DEFAULT_GRID_ID) {
        grid.clear_rows(start_row..end_row, valid);
    }
}
