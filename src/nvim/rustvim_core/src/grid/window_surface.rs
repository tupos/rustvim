//! Safe coordination of window-grid storage, placement, and visibility.

#![forbid(unsafe_code)]

use crate::ui_compositor::{
    CompositorContext, CompositorError, CompositorGridPlacement, CompositorGridPlacementRequest,
    CompositorResult, CompositorRuntime, RegisteredGridPlacement,
};

use super::{
    ComposedPosition, GridContext, GridId, GridLifecycleChange, GridPosition, GridSurfaceOutput,
    WindowGridMode,
    store::{GridStoreError, GridStoreResult},
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

/// Inputs controlling conversion of one floating window into a split.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowFloatConversionRequest {
    pub(super) grid_id: GridId,
    pub(super) message_grid_id: Option<GridId>,
    pub(super) multigrid_enabled: bool,
    pub(super) runtime: CompositorRuntime,
}

/// Editor-owned state update produced by float-to-split conversion.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowFloatToSplitUpdate {
    pub(super) grid_id: GridId,
    pub(super) republish_position: bool,
}

/// Failures produced while coordinating a complete window surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(super) enum WindowSurfaceError {
    /// The compositor could not remove or recompose the floating layer.
    #[error(transparent)]
    Compositor(#[from] CompositorError),

    /// The permanent grid store could not retire or replace the grid.
    #[error(transparent)]
    GridStore(#[from] GridStoreError),
}

/// Result of coordinating a complete window-surface operation.
type WindowSurfaceResult<T> = Result<T, WindowSurfaceError>;

/// Complete native request for closing one window surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowSurfaceCloseRequest {
    pub(super) grid_id: GridId,
    pub(super) message_grid_id: Option<GridId>,
    pub(super) floating: bool,
    pub(super) publish_close: bool,
    pub(super) runtime: CompositorRuntime,
}

/// Complete native request for destroying one window surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowSurfaceDestroyRequest {
    pub(super) grid_id: GridId,
    pub(super) publish_destroy: bool,
    pub(super) create_replacement: bool,
}

/// Inputs controlling one window-position flush.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowPositionFlushRequest {
    pub(super) grid_id: GridId,
    pub(super) position_changed: bool,
    pub(super) current_tab: bool,
}

/// Editor-owned state update produced by a window-position flush.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowPositionFlushUpdate {
    pub(super) clear_position_changed: bool,
}

/// Complete native request for hiding one window surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowSurfaceHideRequest {
    pub(super) grid_id: GridId,
    pub(super) message_grid_id: Option<GridId>,
    pub(super) publish_hide: bool,
    pub(super) runtime: CompositorRuntime,
}

/// Receives window-specific UI events.
pub(super) trait WindowSurfaceOutput: GridSurfaceOutput {
    /// Publishes the current floating-window position.
    fn emit_float_position(&mut self, event: WindowFloatPositionEvent);

    /// Publishes that a window surface is no longer visible.
    fn emit_window_hide(&mut self, grid_id: GridId);

    /// Publishes that a window has closed.
    fn emit_window_close(&mut self, grid_id: GridId);

    /// Requests refreshed position data from the C-owned window model.
    fn refresh_window_position(&mut self, grid_id: GridId);
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

    /// Removes floating state and prepares the same window for split layout.
    pub(super) fn convert_float_to_split(
        &self,
        request: WindowFloatConversionRequest,
        output: &mut impl WindowSurfaceOutput,
    ) -> WindowSurfaceResult<WindowFloatToSplitUpdate> {
        self.compositor.remove_registered_grid(
            self.grids,
            request.grid_id,
            request.message_grid_id,
            request.runtime,
            output,
        )?;

        if request.multigrid_enabled {
            return Ok(WindowFloatToSplitUpdate {
                grid_id: request.grid_id,
                republish_position: true,
            });
        }

        output.emit_window_hide(request.grid_id);
        let grid_id = self
            .destroy(
                WindowSurfaceDestroyRequest {
                    grid_id: request.grid_id,
                    publish_destroy: false,
                    create_replacement: true,
                },
                output,
            )?
            .expect("float-to-split conversion requests a replacement grid");

        Ok(WindowFloatToSplitUpdate {
            grid_id,
            republish_position: false,
        })
    }

    /// Publishes window hiding and removes the surface from composition.
    pub(super) fn hide(
        &self,
        request: WindowSurfaceHideRequest,
        output: &mut impl WindowSurfaceOutput,
    ) -> CompositorResult<()> {
        if request.publish_hide {
            output.emit_window_hide(request.grid_id);
        }

        self.compositor.remove_registered_grid(
            self.grids,
            request.grid_id,
            request.message_grid_id,
            request.runtime,
            output,
        )
    }

    /// Publishes window closure and removes its floating compositor layer.
    pub(super) fn close(
        &self,
        request: WindowSurfaceCloseRequest,
        output: &mut impl WindowSurfaceOutput,
    ) -> CompositorResult<()> {
        if request.publish_close {
            output.emit_window_close(request.grid_id);
        }

        if request.floating {
            self.compositor.remove_registered_grid(
                self.grids,
                request.grid_id,
                request.message_grid_id,
                request.runtime,
                output,
            )?;
        }
        Ok(())
    }

    /// Publishes destruction, retires the grid, and optionally replaces it.
    pub(super) fn destroy(
        &self,
        request: WindowSurfaceDestroyRequest,
        output: &mut impl WindowSurfaceOutput,
    ) -> GridStoreResult<Option<GridId>> {
        if request.publish_destroy {
            output.emit_grid_destroy(request.grid_id);
        }

        let mut store = self.grids.lock_store();
        store.remove(request.grid_id)?;
        let replacement_grid_id = if request.create_replacement {
            let grid_id = self.grids.allocate_grid_id();
            store.register_empty(grid_id)?;
            Some(grid_id)
        } else {
            None
        };
        Ok(replacement_grid_id)
    }

    /// Flushes one window position and acknowledges its layer publication.
    pub(super) fn flush_position(
        &self,
        request: WindowPositionFlushRequest,
        output: &mut impl WindowSurfaceOutput,
    ) -> CompositorResult<WindowPositionFlushUpdate> {
        let allocated = self
            .grids
            .lock_store()
            .grid(request.grid_id)
            .is_ok_and(|grid| grid.is_allocated());
        if !allocated
            || (!request.position_changed
                && !self
                    .compositor
                    .registered_grid_publication_pending(request.grid_id))
        {
            return Ok(WindowPositionFlushUpdate {
                clear_position_changed: false,
            });
        }

        if request.current_tab {
            output.refresh_window_position(request.grid_id);
        } else {
            output.emit_window_hide(request.grid_id);
        }

        if self
            .compositor
            .registered_grid_publication_pending(request.grid_id)
        {
            self.compositor
                .take_registered_grid_publication(request.grid_id, false)?
                .ok_or(CompositorError::InvalidLayerState)?;
        }

        Ok(WindowPositionFlushUpdate {
            clear_position_changed: !request.current_tab,
        })
    }
}
