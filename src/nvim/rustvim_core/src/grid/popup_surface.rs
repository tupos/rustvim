//! Safe coordination of popup-menu grid storage and compositor placement.

#![forbid(unsafe_code)]

use crate::ui_compositor::{
    CompositorContext, CompositorGridPlacement, CompositorGridPlacementRequest, CompositorResult,
    CompositorRuntime, RegisteredGridPlacement,
};

use super::{GridContext, GridId, GridLifecycleChange, GridSurfaceOutput};

/// Validated input for one complete popup-menu surface update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PopupMenuSurfaceRequest {
    grid_id: Option<GridId>,
    placement: CompositorGridPlacement,
    z_index: i32,
    blending: bool,
    contents_invalid: bool,
    current_window_grid_id: Option<GridId>,
}

impl PopupMenuSurfaceRequest {
    /// Builds a popup-menu request from validated native Rust values.
    pub(super) const fn new(
        grid_id: Option<GridId>,
        placement: CompositorGridPlacement,
        z_index: i32,
        blending: bool,
        contents_invalid: bool,
        current_window_grid_id: Option<GridId>,
    ) -> Self {
        Self {
            grid_id,
            placement,
            z_index,
            blending,
            contents_invalid,
            current_window_grid_id,
        }
    }
}

/// Safe coordinator for popup-menu storage and compositor state.
pub(crate) struct PopupMenuSurface<'context> {
    grids: &'context GridContext,
    compositor: &'context CompositorContext,
}

impl<'context> PopupMenuSurface<'context> {
    /// Borrows the contexts participating in popup-menu rendering.
    pub(crate) const fn new(
        grids: &'context GridContext,
        compositor: &'context CompositorContext,
    ) -> Self {
        Self { grids, compositor }
    }

    /// Places, configures, and publishes one popup-menu grid.
    pub(super) fn update(
        &self,
        request: PopupMenuSurfaceRequest,
        message_grid_id: Option<GridId>,
        runtime: CompositorRuntime,
        output: &mut impl GridSurfaceOutput,
    ) -> CompositorResult<GridId> {
        let grid_id = self.resolve_grid_id(request.grid_id);
        let moved = self.compositor.place_registered_grid(
            self.grids,
            RegisteredGridPlacement::new(
                grid_id,
                request.current_window_grid_id,
                message_grid_id,
                CompositorGridPlacementRequest::new(
                    request.placement,
                    request.z_index,
                    request.blending,
                    false,
                    true,
                    true,
                ),
                runtime,
            ),
            output,
        )?;

        let lifecycle = {
            let size = request.placement.size();
            let mut line_buffer = self.grids.lock_line_buffer();
            let mut store = self.grids.lock_store();
            store
                .grid_mut(grid_id)
                .expect("popup-menu grid must remain permanently registered")
                .configure_for_popup_menu(size, moved || request.contents_invalid, &mut line_buffer)
        };
        if lifecycle == GridLifecycleChange::Resized {
            output.emit_grid_resize(grid_id, request.placement.size());
        }

        Ok(grid_id)
    }

    /// Allocates and registers the popup-menu identity on first use.
    fn resolve_grid_id(&self, grid_id: Option<GridId>) -> GridId {
        grid_id.unwrap_or_else(|| {
            let grid_id = self.grids.allocate_grid_id();
            self.grids
                .lock_store()
                .register_empty(grid_id)
                .expect("newly allocated popup-menu identity must be unique");
            grid_id
        })
    }
}
