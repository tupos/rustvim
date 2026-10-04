//! Safe coordination of window-grid storage, views, and resize publication.

#![forbid(unsafe_code)]

use crate::ui_compositor::{
    CompositorContext, CompositorError, CompositorGridPlacement, CompositorGridPlacementRequest,
    CompositorResult, CompositorRuntime, RegisteredGridPlacement,
};

use super::{
    ComposedPosition, GridContext, GridId, GridLifecycleChange, GridPosition, GridSurfaceOutput,
    WindowGridMode, store::GridStoreResult,
};

/// Fractional anchor coordinates retained for a floating-window UI event.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct FloatAnchorPosition {
    pub(super) row: f64,
    pub(super) column: f64,
}

impl FloatAnchorPosition {
    /// Builds finite floating-window anchor coordinates.
    pub(super) fn try_new(row: f64, column: f64) -> Option<Self> {
        (row.is_finite() && column.is_finite()).then_some(Self { row, column })
    }
}

/// Corner of a floating window attached to its anchor position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WindowFloatAnchor {
    NorthWest,
    NorthEast,
    SouthWest,
    SouthEast,
}

/// Validated input for placing one visible floating-window surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WindowFloatPlacementRequest {
    grid_id: GridId,
    window_handle: i32,
    placement: CompositorGridPlacement,
    z_index: i32,
    blending: bool,
    valid: bool,
    accepts_mouse_input: bool,
    current_window_grid_id: Option<GridId>,
    anchor: WindowFloatAnchor,
    anchor_grid_id: GridId,
    anchor_position: FloatAnchorPosition,
    multigrid_enabled: bool,
}

impl WindowFloatPlacementRequest {
    /// Builds one request from validated native Rust values.
    #[allow(clippy::too_many_arguments)]
    pub(super) const fn new(
        grid_id: GridId,
        window_handle: i32,
        placement: CompositorGridPlacement,
        z_index: i32,
        blending: bool,
        valid: bool,
        accepts_mouse_input: bool,
        current_window_grid_id: Option<GridId>,
        anchor: WindowFloatAnchor,
        anchor_grid_id: GridId,
        anchor_position: FloatAnchorPosition,
        multigrid_enabled: bool,
    ) -> Self {
        Self {
            grid_id,
            window_handle,
            placement,
            z_index,
            blending,
            valid,
            accepts_mouse_input,
            current_window_grid_id,
            anchor,
            anchor_grid_id,
            anchor_position,
            multigrid_enabled,
        }
    }
}

/// Floating-window position event produced entirely by safe Rust.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WindowFloatPositionEvent {
    pub(super) grid_id: GridId,
    pub(super) window_handle: i32,
    pub(super) anchor: WindowFloatAnchor,
    pub(super) anchor_grid_id: GridId,
    pub(super) anchor_position: FloatAnchorPosition,
    pub(super) accepts_mouse_input: bool,
    pub(super) z_index: i32,
    pub(super) compositor_index: usize,
    pub(super) screen_position: ComposedPosition,
}

/// Editor-owned actions produced after floating-window placement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowFloatPlacementUpdate {
    pub(super) republish_cursor: bool,
    pub(super) require_full_redraw: bool,
}

/// Receives window-specific UI events.
pub(super) trait WindowSurfaceOutput: GridSurfaceOutput {
    /// Publishes the current floating-window position.
    fn emit_float_position(&mut self, event: WindowFloatPositionEvent);
}

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
    compositor: &'context CompositorContext,
}

impl<'context> WindowSurface<'context> {
    /// Borrows the contexts participating in window rendering.
    pub(crate) const fn new(
        grids: &'context GridContext,
        compositor: &'context CompositorContext,
    ) -> Self {
        Self { grids, compositor }
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

    /// Places one visible floating window and publishes its multigrid event.
    pub(super) fn place_float(
        &self,
        request: WindowFloatPlacementRequest,
        message_grid_id: Option<GridId>,
        runtime: CompositorRuntime,
        output: &mut impl WindowSurfaceOutput,
    ) -> CompositorResult<WindowFloatPlacementUpdate> {
        self.compositor.place_registered_grid(
            self.grids,
            RegisteredGridPlacement::new(
                request.grid_id,
                request.current_window_grid_id,
                message_grid_id,
                CompositorGridPlacementRequest::new(
                    request.placement,
                    request.z_index,
                    request.blending,
                    request.valid,
                    false,
                    request.accepts_mouse_input,
                ),
                runtime,
            ),
            output,
        )?;

        if request.multigrid_enabled {
            let compositor_index = self
                .compositor
                .take_registered_grid_publication(request.grid_id, false)?
                .ok_or(CompositorError::InvalidLayerState)?;
            output.emit_float_position(WindowFloatPositionEvent {
                grid_id: request.grid_id,
                window_handle: request.window_handle,
                anchor: request.anchor,
                anchor_grid_id: request.anchor_grid_id,
                anchor_position: request.anchor_position,
                accepts_mouse_input: request.accepts_mouse_input,
                z_index: request.z_index,
                compositor_index,
                screen_position: request.placement.origin(),
            });
        }

        Ok(WindowFloatPlacementUpdate {
            republish_cursor: true,
            require_full_redraw: !request.valid,
        })
    }
}
