//! C boundary adapters for popup-menu surface operations.

use crate::{
    context::registered_context,
    ui_compositor::{
        CompositorGridPlacement, compositor_runtime_snapshot, current_message_grid_id,
    },
};

use super::{
    super::{GridId, PopupMenuSurfaceRequest, VimHandleT},
    surface::NvimGridSurfaceOutput,
};

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
    let mut output = NvimGridSurfaceOutput::new();

    registered_context()
        .popup_menu_surface()
        .update(request, current_message_grid_id(), runtime, &mut output)
        .expect("popup-menu update must preserve rendering invariants")
        .as_native_handle()
}
