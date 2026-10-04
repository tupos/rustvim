//! C boundary adapters for popup-menu surface operations.

use super::super::ComposedGridSpace;
use super::super::ComposedPosition;
use super::super::GridId;
use super::super::GridPosition;
use super::super::GridSize;
use super::super::GridSurfaceOutput;
use super::super::PopupMenuAnchor;
use super::super::PopupMenuAnchorGrid;
use super::super::PopupMenuHideRequest;
use super::super::PopupMenuPositionEvent;
use super::super::PopupMenuPositionGeometry;
use super::super::PopupMenuPositionRequest;
use super::super::PopupMenuPublicationPolicy;
use super::super::PopupMenuSurfaceOutput;
use super::super::PopupMenuSurfaceRequest;
use super::super::VimHandleT;
use super::surface::NvimGridSurfaceOutput;
use crate::context::registered_context;
use crate::defines::CVimString;
use crate::defines::Integer;
use crate::ui::output::RawLineCells;
use crate::ui::output::RawLineCommand;
use crate::ui::output::UiCommand;
use crate::ui::output::UiOutput;
use crate::ui_compositor::CompositorGridPlacement;
use crate::ui_compositor::compositor_runtime_snapshot;
use crate::ui_compositor::current_message_grid_id;

unsafe extern "C" {
    fn ui_call_win_close(grid: Integer);
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
}

/// C representation of one complete popup-menu surface update.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CPopupMenuSurfaceRequest {
    grid_handle: VimHandleT,
    row: libc::c_int,
    column: libc::c_int,
    rows: libc::c_int,
    columns: libc::c_int,
    z_index: libc::c_int,
    blending: bool,
    contents_invalid: bool,
    current_window_grid_handle: VimHandleT,
}

/// C representation of one popup-menu position-publication request.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CPopupMenuSurfacePositionRequest {
    grid_handle: VimHandleT,
    force: bool,
    multigrid_enabled: bool,
    drawn: bool,
    external: bool,
    above: bool,
    height: libc::c_int,
    anchor_grid_handle: VimHandleT,
    row: libc::c_int,
    column: libc::c_int,
    window_row_offset: libc::c_int,
    window_column_offset: libc::c_int,
    z_index: libc::c_int,
}

impl CPopupMenuSurfacePositionRequest {
    /// Converts C-owned state into a validated native position request.
    fn try_into_native(self) -> Option<PopupMenuPositionRequest> {
        let grid_id = match self.grid_handle {
            0 => None,
            handle if handle > 0 => Some(GridId::new(handle)),
            _ => return None,
        };
        let anchor_grid = match self.anchor_grid_handle {
            -1 => Some(PopupMenuAnchorGrid::Global),
            0 => None,
            handle if handle > 0 => Some(PopupMenuAnchorGrid::Grid(GridId::new(handle))),
            _ => return None,
        };
        if self.height < 0 {
            return None;
        }

        Some(PopupMenuPositionRequest {
            grid_id,
            publication: PopupMenuPublicationPolicy {
                force: self.force,
                multigrid_enabled: self.multigrid_enabled,
                drawn: self.drawn,
                external: self.external,
            },
            geometry: PopupMenuPositionGeometry {
                above: self.above,
                height: self.height,
                anchor_grid,
                origin: ComposedPosition::new(self.row, self.column),
                window_offset: GridPosition::new(self.window_row_offset, self.window_column_offset),
            },
            z_index: self.z_index,
        })
    }
}

/// Final UI adapter for popup-menu surface operations.
struct NvimPopupMenuSurfaceOutput {
    surface: NvimGridSurfaceOutput,
}

impl NvimPopupMenuSurfaceOutput {
    /// Creates an output adapter for one synchronous popup-menu operation.
    const fn new() -> Self {
        Self {
            surface: NvimGridSurfaceOutput::new(),
        }
    }
}

impl UiOutput<ComposedGridSpace> for NvimPopupMenuSurfaceOutput {
    fn emit(&mut self, command: UiCommand<ComposedGridSpace>) {
        self.surface.emit(command);
    }

    fn emit_raw_line<Cells: RawLineCells>(&mut self, command: RawLineCommand<Cells>) {
        self.surface.emit_raw_line(command);
    }
}

impl GridSurfaceOutput for NvimPopupMenuSurfaceOutput {
    fn emit_grid_resize(&mut self, grid_id: GridId, size: GridSize) {
        self.surface.emit_grid_resize(grid_id, size);
    }

    fn emit_grid_destroy(&mut self, grid_id: GridId) {
        self.surface.emit_grid_destroy(grid_id);
    }
}

impl PopupMenuSurfaceOutput for NvimPopupMenuSurfaceOutput {
    fn emit_window_close(&mut self, grid_id: GridId) {
        // SAFETY: Safe Rust validated the identity, and the generated callback
        // is synchronous.
        unsafe {
            ui_call_win_close(Integer::from(grid_id.as_native_handle()));
        }
    }

    fn emit_position(&mut self, event: PopupMenuPositionEvent) {
        let anchor = match event.anchor {
            PopupMenuAnchor::NorthWest => b"NW",
            PopupMenuAnchor::SouthWest => b"SW",
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
                -1,
                anchor,
                Integer::from(event.anchor_grid.native_handle()),
                f64::from(event.anchor_position.row),
                f64::from(event.anchor_position.column),
                false,
                Integer::from(event.z_index),
                compositor_index,
                Integer::from(event.screen_position.row),
                Integer::from(event.screen_position.column),
            );
        }
    }
}

impl CPopupMenuSurfaceRequest {
    /// Converts C-owned policy into a validated native Rust request.
    fn try_into_native(self) -> Option<PopupMenuSurfaceRequest> {
        let grid_id = match self.grid_handle {
            0 => None,
            handle if handle > 0 => Some(GridId::new(handle)),
            _ => return None,
        };
        let current_window_grid_id = match self.current_window_grid_handle {
            0 => None,
            handle if handle > 0 => Some(GridId::new(handle)),
            _ => return None,
        };

        let placement =
            CompositorGridPlacement::try_new(self.row, self.column, self.rows, self.columns)?;

        Some(PopupMenuSurfaceRequest::new(
            grid_id,
            placement,
            self.z_index,
            self.blending,
            self.contents_invalid,
            current_window_grid_id,
        ))
    }
}

/// Applies one complete popup-menu placement and storage update.
#[unsafe(no_mangle)]
pub extern "C" fn grid_update_popup_menu_surface(request: CPopupMenuSurfaceRequest) -> VimHandleT {
    let request = request
        .try_into_native()
        .expect("popup-menu request must contain valid editor state");
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let mut output = NvimPopupMenuSurfaceOutput::new();

    registered_context()
        .popup_menu_surface()
        .update(request, current_message_grid_id(), runtime, &mut output)
        .expect("popup-menu update must preserve rendering invariants")
        .as_native_handle()
}

/// Removes the popup-menu layer and releases its grid storage.
#[unsafe(no_mangle)]
pub extern "C" fn grid_hide_popup_menu_surface(grid_handle: VimHandleT, publish_ui_events: bool) {
    if grid_handle <= 0 {
        return;
    }
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let request = PopupMenuHideRequest {
        grid_id: GridId::new(grid_handle),
        message_grid_id: current_message_grid_id(),
        publish_ui_events,
        runtime,
    };
    let mut output = NvimPopupMenuSurfaceOutput::new();

    registered_context()
        .popup_menu_surface()
        .hide(request, &mut output)
        .expect("popup-menu removal must preserve rendering invariants");
}

/// Recomposes the area occupied by the popup-menu layer.
#[unsafe(no_mangle)]
pub extern "C" fn grid_recompose_popup_menu_surface(grid_handle: VimHandleT) {
    if grid_handle <= 0 {
        return;
    }
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let mut output = NvimPopupMenuSurfaceOutput::new();

    registered_context()
        .popup_menu_surface()
        .recompose(
            GridId::new(grid_handle),
            current_message_grid_id(),
            runtime,
            &mut output,
        )
        .expect("popup-menu recomposition must preserve rendering invariants");
}

/// Publishes the popup-menu position when required by its lifecycle state.
#[unsafe(no_mangle)]
pub extern "C" fn grid_publish_popup_menu_surface_position(
    request: CPopupMenuSurfacePositionRequest,
) {
    let request = request
        .try_into_native()
        .expect("popup-menu position request must contain valid editor state");
    let mut output = NvimPopupMenuSurfaceOutput::new();

    registered_context()
        .popup_menu_surface()
        .publish_position(request, &mut output)
        .expect("popup-menu position publication must preserve rendering invariants");
}
