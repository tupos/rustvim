//! C boundary adapters for screen-grid lifecycle operations.

use super::super::ComposedGridSpace;
use super::super::ComposedPosition;
use super::super::DEFAULT_GRID_ID;
use super::super::FloatAnchorPosition;
use super::super::GridId;
use super::super::GridLifecycleChange;
use super::super::GridPosition;
use super::super::GridSize;
use super::super::GridSurfaceOutput;
use super::super::LineBufferGuard;
use super::super::StoredGridGuard;
use super::super::VimHandleT;
use super::super::WindowFloatAnchor;
use super::super::WindowFloatAnchorWindow;
use super::super::WindowFloatConversionRequest;
use super::super::WindowFloatGeometry;
use super::super::WindowFloatLayerPolicy;
use super::super::WindowFloatPositionEvent;
use super::super::WindowFloatSurfaceRequest;
use super::super::WindowFloatSurfaceUpdate;
use super::super::WindowFloatToSplitUpdate;
use super::super::WindowGridMode;
use super::super::WindowPositionFlushRequest;
use super::super::WindowPositionFlushUpdate;
use super::super::WindowSurfaceCloseRequest;
use super::super::WindowSurfaceDestroyRequest;
use super::super::WindowSurfaceHideRequest;
use super::super::WindowSurfaceOutput;
use super::super::WindowSurfaceRequest;
use super::super::WindowSurfaceUpdate;
use super::surface::NvimGridSurfaceOutput;
use crate::context::registered_context;
use crate::defines::CVimString;
use crate::defines::Integer;
use crate::globals;
use crate::ui::output::RawLineCells;
use crate::ui::output::RawLineCommand;
use crate::ui::output::UiCommand;
use crate::ui::output::UiOutput;
use crate::ui_compositor::compositor_runtime_snapshot;
use crate::ui_compositor::current_message_grid_id;

type CWindowPositionEmitter = unsafe extern "C" fn(VimHandleT, bool);

unsafe extern "C" {
    fn ui_call_win_float_pos(
        grid: Integer,
        window: VimHandleT,
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
    fn ui_call_win_close(grid: Integer);
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

/// C representation of one complete floating-window surface update.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CWindowFloatSurfaceRequest {
    grid_handle: VimHandleT,
    window_handle: VimHandleT,
    anchor_row: f64,
    anchor_column: f64,
    rows: libc::c_int,
    columns: libc::c_int,
    screen_rows: libc::c_int,
    screen_columns: libc::c_int,
    reserved_rows: libc::c_int,
    z_index: libc::c_int,
    blending: bool,
    valid: bool,
    accepts_mouse_input: bool,
    current_window_grid_handle: VimHandleT,
    anchor: libc::c_int,
    anchor_window_grid_handle: VimHandleT,
    anchor_window_row: libc::c_int,
    anchor_window_column: libc::c_int,
    fixed: bool,
    hidden: bool,
    multigrid_enabled: bool,
}

impl CWindowFloatSurfaceRequest {
    /// Converts C-owned floating-window state into a validated native request.
    fn try_into_native(self) -> Option<WindowFloatSurfaceRequest> {
        if self.grid_handle <= 0 || self.window_handle <= 0 {
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
        let anchor_window = match self.anchor_window_grid_handle {
            0 => None,
            handle if handle > 0 => Some(WindowFloatAnchorWindow::new(
                GridId::new(handle),
                ComposedPosition::new(self.anchor_window_row, self.anchor_window_column),
            )),
            _ => return None,
        };
        let geometry = WindowFloatGeometry::try_new(
            FloatAnchorPosition::try_new(self.anchor_row, self.anchor_column)?,
            GridSize::try_new(self.rows, self.columns)?,
            GridSize::try_new(self.screen_rows, self.screen_columns)?,
            self.reserved_rows,
            self.fixed,
        )?;
        let layer_policy = WindowFloatLayerPolicy::new(
            self.z_index,
            self.blending,
            self.valid,
            self.accepts_mouse_input,
            current_window_grid_id,
            self.hidden,
            self.multigrid_enabled,
        );

        Some(WindowFloatSurfaceRequest::new(
            GridId::new(self.grid_handle),
            self.window_handle,
            anchor,
            anchor_window,
            geometry,
            layer_policy,
        ))
    }
}

/// C representation of editor-owned actions after a float-surface update.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CWindowFloatSurfaceUpdate {
    republish_cursor: bool,
    require_full_redraw: bool,
    screen_row: libc::c_int,
    screen_column: libc::c_int,
}

impl From<WindowFloatSurfaceUpdate> for CWindowFloatSurfaceUpdate {
    fn from(update: WindowFloatSurfaceUpdate) -> Self {
        Self {
            republish_cursor: update.republish_cursor,
            require_full_redraw: update.require_full_redraw,
            screen_row: update.screen_position.row,
            screen_column: update.screen_position.column,
        }
    }
}

/// C representation of editor-owned state after float-to-split conversion.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CWindowFloatToSplitUpdate {
    grid_handle: VimHandleT,
    republish_position: bool,
}

impl From<WindowFloatToSplitUpdate> for CWindowFloatToSplitUpdate {
    fn from(update: WindowFloatToSplitUpdate) -> Self {
        Self {
            grid_handle: update.grid_id.as_native_handle(),
            republish_position: update.republish_position,
        }
    }
}

/// Final UI adapter for complete window-surface operations.
struct NvimWindowSurfaceOutput {
    surface: NvimGridSurfaceOutput,
    position_emitter: Option<CWindowPositionEmitter>,
    validate_position: bool,
}

impl NvimWindowSurfaceOutput {
    /// Creates an output adapter for one synchronous window operation.
    const fn new() -> Self {
        Self {
            surface: NvimGridSurfaceOutput::new(),
            position_emitter: None,
            validate_position: false,
        }
    }

    /// Creates an output adapter that can refresh one C-owned window.
    const fn for_position_flush(
        position_emitter: CWindowPositionEmitter,
        validate_position: bool,
    ) -> Self {
        Self {
            surface: NvimGridSurfaceOutput::new(),
            position_emitter: Some(position_emitter),
            validate_position,
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
                event.window_handle,
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

    fn emit_window_close(&mut self, grid_id: GridId) {
        // SAFETY: Safe Rust validated the identity, and the generated callback
        // is synchronous.
        unsafe {
            ui_call_win_close(Integer::from(grid_id.as_native_handle()));
        }
    }

    fn refresh_window_position(&mut self, grid_id: GridId) {
        let position_emitter = self
            .position_emitter
            .expect("window-position output requires its C emitter");

        // SAFETY: The FFI entrypoint validates the callback, and C resolves
        // the stable grid identity synchronously without retaining it.
        unsafe {
            position_emitter(grid_id.as_native_handle(), self.validate_position);
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

/// Resolves and updates one complete floating-window surface.
#[unsafe(no_mangle)]
pub extern "C" fn grid_update_window_float_surface(
    request: CWindowFloatSurfaceRequest,
) -> CWindowFloatSurfaceUpdate {
    let request = request
        .try_into_native()
        .expect("floating-window update must contain valid editor state");
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let mut output = NvimWindowSurfaceOutput::new();

    registered_context()
        .window_surface()
        .update_float(request, current_message_grid_id(), runtime, &mut output)
        .expect("floating-window update must preserve rendering invariants")
        .into()
}

/// Converts one floating-window surface into a split-window surface.
#[unsafe(no_mangle)]
pub extern "C" fn grid_convert_window_float_to_split(
    grid_handle: VimHandleT,
    multigrid_enabled: bool,
) -> CWindowFloatToSplitUpdate {
    assert!(
        grid_handle > 0,
        "window surface must have a valid grid identity"
    );
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let mut output = NvimWindowSurfaceOutput::new();

    registered_context()
        .window_surface()
        .convert_float_to_split(
            WindowFloatConversionRequest {
                grid_id: GridId::new(grid_handle),
                message_grid_id: current_message_grid_id(),
                multigrid_enabled,
                runtime,
            },
            &mut output,
        )
        .expect("float-to-split conversion must preserve rendering invariants")
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

/// Publishes window closure and removes a floating compositor layer.
#[unsafe(no_mangle)]
pub extern "C" fn grid_close_window_surface(
    grid_handle: VimHandleT,
    floating: bool,
    publish_close: bool,
) {
    assert!(
        grid_handle > 0,
        "window surface must have a valid grid identity"
    );
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let mut output = NvimWindowSurfaceOutput::new();

    registered_context()
        .window_surface()
        .close(
            WindowSurfaceCloseRequest {
                grid_id: GridId::new(grid_handle),
                message_grid_id: current_message_grid_id(),
                floating,
                publish_close,
                runtime,
            },
            &mut output,
        )
        .expect("window closure must preserve rendering invariants");
}

/// Publishes grid destruction, retires its identity, and optionally replaces it.
#[unsafe(no_mangle)]
pub extern "C" fn grid_destroy_window_surface(
    grid_handle: VimHandleT,
    publish_destroy: bool,
    create_replacement: bool,
) -> VimHandleT {
    assert!(
        grid_handle > 0,
        "window surface must have a valid grid identity"
    );
    let mut output = NvimWindowSurfaceOutput::new();

    registered_context()
        .window_surface()
        .destroy(
            WindowSurfaceDestroyRequest {
                grid_id: GridId::new(grid_handle),
                publish_destroy,
                create_replacement,
            },
            &mut output,
        )
        .expect("window destruction must preserve grid-store invariants")
        .map_or(0, GridId::as_native_handle)
}

/// Flushes one window's pending position and compositor publication.
#[unsafe(no_mangle)]
pub extern "C" fn grid_flush_window_surface_position(
    grid_handle: VimHandleT,
    position_changed: bool,
    current_tab: bool,
    validate: bool,
    position_emitter: Option<CWindowPositionEmitter>,
) -> bool {
    assert!(
        grid_handle > 0,
        "window surface must have a valid grid identity"
    );
    let position_emitter =
        position_emitter.expect("window-position flush requires a valid C emitter");
    let mut output = NvimWindowSurfaceOutput::for_position_flush(position_emitter, validate);

    let WindowPositionFlushUpdate {
        clear_position_changed,
    } = registered_context()
        .window_surface()
        .flush_position(
            WindowPositionFlushRequest {
                grid_id: GridId::new(grid_handle),
                position_changed,
                current_tab,
            },
            &mut output,
        )
        .expect("window-position flush must preserve compositor invariants");

    clear_position_changed
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
