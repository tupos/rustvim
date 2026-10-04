//! Safe coordination of popup-menu grid storage and compositor placement.

#![forbid(unsafe_code)]

use super::ComposedPosition;
use super::GridContext;
use super::GridId;
use super::GridLifecycleChange;
use super::GridPosition;
use super::GridSurfaceOutput;
use crate::ui_compositor::CompositorContext;
use crate::ui_compositor::CompositorError;
use crate::ui_compositor::CompositorGridPlacement;
use crate::ui_compositor::CompositorGridPlacementRequest;
use crate::ui_compositor::CompositorResult;
use crate::ui_compositor::CompositorRuntime;
use crate::ui_compositor::RegisteredGridPlacement;

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

/// Policy controlling whether popup-menu position must be published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PopupMenuPublicationPolicy {
    pub(super) force: bool,
    pub(super) multigrid_enabled: bool,
    pub(super) drawn: bool,
    pub(super) external: bool,
}

/// Target used to interpret the popup-menu anchor coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PopupMenuAnchorGrid {
    Global,
    Grid(GridId),
}

impl PopupMenuAnchorGrid {
    /// Returns the native UI-protocol grid handle.
    pub(super) const fn native_handle(self) -> i32 {
        match self {
            Self::Global => -1,
            Self::Grid(grid_id) => grid_id.as_native_handle(),
        }
    }
}

/// Editor geometry needed to construct a popup-menu position event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PopupMenuPositionGeometry {
    pub(super) above: bool,
    pub(super) height: i32,
    pub(super) anchor_grid: Option<PopupMenuAnchorGrid>,
    pub(super) origin: ComposedPosition,
    pub(super) window_offset: GridPosition,
}

/// Validated request to publish the popup-menu position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PopupMenuPositionRequest {
    pub(super) grid_id: Option<GridId>,
    pub(super) publication: PopupMenuPublicationPolicy,
    pub(super) geometry: PopupMenuPositionGeometry,
    pub(super) z_index: i32,
}

/// Complete request for hiding a popup-menu surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PopupMenuHideRequest {
    pub(super) grid_id: GridId,
    pub(super) message_grid_id: Option<GridId>,
    pub(super) publish_ui_events: bool,
    pub(super) runtime: CompositorRuntime,
}

/// Popup-menu corner used by the final window-position event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PopupMenuAnchor {
    NorthWest,
    SouthWest,
}

/// Owned popup-menu position event produced by safe Rust.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PopupMenuPositionEvent {
    pub(super) grid_id: GridId,
    pub(super) anchor: PopupMenuAnchor,
    pub(super) anchor_grid: PopupMenuAnchorGrid,
    pub(super) anchor_position: GridPosition,
    pub(super) screen_position: ComposedPosition,
    pub(super) z_index: i32,
    pub(super) compositor_index: usize,
}

/// Receives popup-menu-specific lifecycle events.
pub(super) trait PopupMenuSurfaceOutput: GridSurfaceOutput {
    /// Publishes that the popup-menu window closed.
    fn emit_window_close(&mut self, grid_id: GridId);

    /// Publishes the popup-menu's current position and layer index.
    fn emit_position(&mut self, event: PopupMenuPositionEvent);
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

    /// Removes the popup-menu layer and releases its cell storage.
    pub(super) fn hide(
        &self,
        request: PopupMenuHideRequest,
        output: &mut impl PopupMenuSurfaceOutput,
    ) -> CompositorResult<()> {
        self.compositor.remove_registered_grid(
            self.grids,
            request.grid_id,
            request.message_grid_id,
            request.runtime,
            output,
        )?;

        if request.publish_ui_events {
            output.emit_window_close(request.grid_id);
            output.emit_grid_destroy(request.grid_id);
        }

        self.grids
            .lock_store()
            .grid_mut(request.grid_id)
            .expect("popup-menu grid must remain permanently registered")
            .release_storage();
        Ok(())
    }

    /// Publishes the popup-menu position when its layer index changed.
    pub(super) fn publish_position(
        &self,
        request: PopupMenuPositionRequest,
        output: &mut impl PopupMenuSurfaceOutput,
    ) -> CompositorResult<()> {
        if !request.publication.force
            && (!request.publication.multigrid_enabled
                || !request.publication.drawn
                || request.publication.external
                || request.grid_id.is_none())
        {
            return Ok(());
        }

        let grid_id = request.grid_id.ok_or(CompositorError::InvalidLayerState)?;
        let Some(compositor_index) = self
            .compositor
            .take_registered_grid_publication(grid_id, !request.publication.force)?
        else {
            return Ok(());
        };
        let anchor_grid = request
            .geometry
            .anchor_grid
            .ok_or(CompositorError::InvalidLayerState)?;

        let row_offset = if request.geometry.above {
            request
                .geometry
                .height
                .checked_neg()
                .ok_or(CompositorError::InvalidGeometry)?
        } else {
            0
        };
        let anchor_row = request
            .geometry
            .origin
            .row
            .checked_sub(row_offset)
            .and_then(|row| row.checked_sub(request.geometry.window_offset.row))
            .ok_or(CompositorError::InvalidGeometry)?;
        let anchor_column = request
            .geometry
            .origin
            .column
            .checked_sub(request.geometry.window_offset.column)
            .ok_or(CompositorError::InvalidGeometry)?;

        output.emit_position(PopupMenuPositionEvent {
            grid_id,
            anchor: if request.geometry.above {
                PopupMenuAnchor::SouthWest
            } else {
                PopupMenuAnchor::NorthWest
            },
            anchor_grid,
            anchor_position: GridPosition::new(anchor_row, anchor_column),
            screen_position: request.geometry.origin,
            z_index: request.z_index,
            compositor_index,
        });
        Ok(())
    }

    /// Recomposes the screen area occupied by the popup-menu layer.
    pub(super) fn recompose(
        &self,
        grid_id: GridId,
        message_grid_id: Option<GridId>,
        runtime: CompositorRuntime,
        output: &mut impl GridSurfaceOutput,
    ) -> CompositorResult<()> {
        self.compositor.recompose_registered_grid(
            self.grids,
            grid_id,
            message_grid_id,
            runtime,
            output,
        )
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
