//! Safe coordination of window-grid storage, views, and resize publication.

#![forbid(unsafe_code)]

use super::{
    GridContext, GridId, GridLifecycleChange, GridPosition, GridSurfaceOutput, WindowGridMode,
    store::GridStoreResult,
};

/// Validated input for one complete window-grid configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowSurfaceRequest {
    grid_id: GridId,
    mode: WindowGridMode,
    view_offset: GridPosition,
    force_dimensions_publication: bool,
    bordered_float: bool,
}

impl WindowSurfaceRequest {
    /// Builds a request from validated native Rust values.
    pub(super) const fn new(
        grid_id: GridId,
        mode: WindowGridMode,
        view_offset: GridPosition,
        force_dimensions_publication: bool,
        bordered_float: bool,
    ) -> Self {
        Self {
            grid_id,
            mode,
            view_offset,
            force_dimensions_publication,
            bordered_float,
        }
    }
}

/// Editor-owned actions produced by window-grid configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowSurfaceUpdate {
    pub(super) grid_id: GridId,
    pub(super) row_offset_changed: bool,
    pub(super) redraw_border: bool,
    pub(super) republish_cursor: bool,
}

/// Safe coordinator for window-grid storage and view state.
pub(crate) struct WindowSurface<'context> {
    grids: &'context GridContext,
}

impl<'context> WindowSurface<'context> {
    /// Borrows the grid context participating in window rendering.
    pub(crate) const fn new(grids: &'context GridContext) -> Self {
        Self { grids }
    }

    /// Configures storage and view state, then publishes changed dimensions.
    pub(super) fn configure(
        &self,
        request: WindowSurfaceRequest,
        output: &mut impl GridSurfaceOutput,
    ) -> GridStoreResult<WindowSurfaceUpdate> {
        let (configuration, previous_row_offset) = {
            let mut line_buffer = self.grids.lock_line_buffer();
            let mut store = self.grids.lock_store();
            let previous_row_offset = store
                .window_view_descriptor(request.grid_id)
                .map(|descriptor| descriptor.offset.row)
                .unwrap_or(0);
            let configuration = {
                let grid = store.grid_mut(request.grid_id)?;
                grid.configure_window_view(request.mode, request.view_offset, &mut line_buffer)
            };
            store.set_window_view(request.grid_id, configuration.view)?;
            (configuration, previous_row_offset)
        };

        let published_size = match request.mode {
            WindowGridMode::Dedicated(size)
                if request.force_dimensions_publication
                    || configuration.lifecycle == GridLifecycleChange::Resized =>
            {
                output.emit_grid_resize(configuration.grid_id, size);
                Some(size)
            }
            WindowGridMode::Dedicated(_) | WindowGridMode::Shared => None,
        };

        Ok(WindowSurfaceUpdate {
            grid_id: configuration.grid_id,
            row_offset_changed: previous_row_offset != configuration.view.offset.row,
            redraw_border: request.bordered_float
                && configuration.lifecycle == GridLifecycleChange::Resized,
            republish_cursor: published_size.is_some(),
        })
    }
}
