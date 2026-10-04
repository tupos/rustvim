//! Safe coordination of window-grid storage, placement, and visibility.

#![forbid(unsafe_code)]

use super::ComposedPosition;
use super::GridContext;
use super::GridId;
use super::GridLifecycleChange;
use super::GridPosition;
use super::GridSize;
use super::GridSurfaceOutput;
use super::WindowGridMode;
use super::store::GridStoreError;
use super::store::GridStoreResult;
use crate::ui_compositor::CompositorContext;
use crate::ui_compositor::CompositorError;
use crate::ui_compositor::CompositorGridPlacement;
use crate::ui_compositor::CompositorGridPlacementRequest;
use crate::ui_compositor::CompositorResult;
use crate::ui_compositor::CompositorRuntime;
use crate::ui_compositor::RegisteredGridPlacement;

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

impl WindowFloatAnchor {
    /// Reports whether the window extends left from its anchor column.
    const fn is_east(self) -> bool {
        matches!(self, Self::NorthEast | Self::SouthEast)
    }

    /// Reports whether the window extends up from its anchor row.
    const fn is_south(self) -> bool {
        matches!(self, Self::SouthWest | Self::SouthEast)
    }
}

/// Window whose grid view supplies a floating window's anchor grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowFloatAnchorWindow {
    grid_id: GridId,
    fallback_origin: ComposedPosition,
}

impl WindowFloatAnchorWindow {
    /// Describes the anchor window and its C-owned fallback screen origin.
    pub(super) const fn new(grid_id: GridId, fallback_origin: ComposedPosition) -> Self {
        Self {
            grid_id,
            fallback_origin,
        }
    }
}

/// Geometry needed to resolve one floating window into composed-screen space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WindowFloatGeometry {
    anchor_position: FloatAnchorPosition,
    size: GridSize,
    screen_size: GridSize,
    reserved_rows: i32,
    fixed: bool,
}

impl WindowFloatGeometry {
    /// Builds validated floating-window geometry.
    pub(super) fn try_new(
        anchor_position: FloatAnchorPosition,
        size: GridSize,
        screen_size: GridSize,
        reserved_rows: i32,
        fixed: bool,
    ) -> Option<Self> {
        (reserved_rows >= 0).then_some(Self {
            anchor_position,
            size,
            screen_size,
            reserved_rows,
            fixed,
        })
    }

    /// Resolves anchor-relative geometry and clips it to the composed screen.
    fn resolve(
        self,
        anchor: WindowFloatAnchor,
        anchor_grid_id: GridId,
        view_offset: GridPosition,
        anchor_grid_origin: ComposedPosition,
    ) -> Option<ResolvedWindowFloatGeometry> {
        let anchor_position = FloatAnchorPosition::try_new(
            self.anchor_position.row + f64::from(view_offset.row),
            self.anchor_position.column + f64::from(view_offset.column),
        )?;
        let mut row = (anchor_position.row as i32).saturating_add(anchor_grid_origin.row);
        let mut column = (anchor_position.column as i32).saturating_add(anchor_grid_origin.column);

        if anchor.is_south() {
            row = row.saturating_sub(self.size.height);
        }
        if anchor.is_east() {
            column = column.saturating_sub(self.size.width);
        }

        let maximum_row = self
            .screen_size
            .height
            .saturating_sub(self.size.height)
            .saturating_sub(self.reserved_rows);
        row = row.min(maximum_row).max(0);
        if !self.fixed || anchor.is_east() {
            let maximum_column = self.screen_size.width.saturating_sub(self.size.width);
            column = column.min(maximum_column).max(0);
        }

        Some(ResolvedWindowFloatGeometry {
            placement: CompositorGridPlacement::try_new(
                row,
                column,
                self.size.height,
                self.size.width,
            )?,
            anchor_grid_id,
            anchor_position,
        })
    }
}

/// Compositor policy attached to one floating-window layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowFloatLayerPolicy {
    z_index: i32,
    blending: bool,
    valid: bool,
    accepts_mouse_input: bool,
    current_window_grid_id: Option<GridId>,
    hidden: bool,
    multigrid_enabled: bool,
}

impl WindowFloatLayerPolicy {
    /// Groups validated layer policy independently from float geometry.
    pub(super) const fn new(
        z_index: i32,
        blending: bool,
        valid: bool,
        accepts_mouse_input: bool,
        current_window_grid_id: Option<GridId>,
        hidden: bool,
        multigrid_enabled: bool,
    ) -> Self {
        Self {
            z_index,
            blending,
            valid,
            accepts_mouse_input,
            current_window_grid_id,
            hidden,
            multigrid_enabled,
        }
    }
}

/// Geometry resolved against the anchor view and compositor layer stack.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ResolvedWindowFloatGeometry {
    placement: CompositorGridPlacement,
    anchor_grid_id: GridId,
    anchor_position: FloatAnchorPosition,
}

/// Validated input for updating one floating-window surface.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct WindowFloatSurfaceRequest {
    grid_id: GridId,
    window_handle: i32,
    anchor: WindowFloatAnchor,
    anchor_window: Option<WindowFloatAnchorWindow>,
    geometry: WindowFloatGeometry,
    layer_policy: WindowFloatLayerPolicy,
}

impl WindowFloatSurfaceRequest {
    /// Builds one request from validated native Rust values.
    pub(super) const fn new(
        grid_id: GridId,
        window_handle: i32,
        anchor: WindowFloatAnchor,
        anchor_window: Option<WindowFloatAnchorWindow>,
        geometry: WindowFloatGeometry,
        layer_policy: WindowFloatLayerPolicy,
    ) -> Self {
        Self {
            grid_id,
            window_handle,
            anchor,
            anchor_window,
            geometry,
            layer_policy,
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

/// Editor-owned actions produced after updating a floating-window surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct WindowFloatSurfaceUpdate {
    pub(super) republish_cursor: bool,
    pub(super) require_full_redraw: bool,
    pub(super) screen_position: ComposedPosition,
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

    /// Resolves and updates one floating window, including hidden surfaces.
    pub(super) fn update_float(
        &self,
        request: WindowFloatSurfaceRequest,
        message_grid_id: Option<GridId>,
        runtime: CompositorRuntime,
        output: &mut impl WindowSurfaceOutput,
    ) -> WindowSurfaceResult<WindowFloatSurfaceUpdate> {
        let resolved = self.resolve_float_geometry(request)?;

        if request.layer_policy.hidden {
            self.hide(
                WindowSurfaceHideRequest {
                    grid_id: request.grid_id,
                    message_grid_id,
                    publish_hide: request.layer_policy.multigrid_enabled,
                    runtime,
                },
                output,
            )?;
            return Ok(WindowFloatSurfaceUpdate {
                republish_cursor: false,
                require_full_redraw: false,
                screen_position: resolved.placement.origin(),
            });
        }

        self.compositor.place_registered_grid(
            self.grids,
            RegisteredGridPlacement::new(
                request.grid_id,
                request.layer_policy.current_window_grid_id,
                message_grid_id,
                CompositorGridPlacementRequest::new(
                    resolved.placement,
                    request.layer_policy.z_index,
                    request.layer_policy.blending,
                    request.layer_policy.valid,
                    false,
                    request.layer_policy.accepts_mouse_input,
                ),
                runtime,
            ),
            output,
        )?;

        if request.layer_policy.multigrid_enabled {
            let compositor_index = self
                .compositor
                .take_registered_grid_publication(request.grid_id, false)?
                .ok_or(CompositorError::InvalidLayerState)?;
            output.emit_float_position(WindowFloatPositionEvent {
                grid_id: request.grid_id,
                window_handle: request.window_handle,
                anchor: request.anchor,
                anchor_grid_id: resolved.anchor_grid_id,
                anchor_position: resolved.anchor_position,
                accepts_mouse_input: request.layer_policy.accepts_mouse_input,
                z_index: request.layer_policy.z_index,
                compositor_index,
                screen_position: resolved.placement.origin(),
            });
        }

        Ok(WindowFloatSurfaceUpdate {
            republish_cursor: true,
            require_full_redraw: !request.layer_policy.valid,
            screen_position: resolved.placement.origin(),
        })
    }

    /// Resolves a float's anchor view, compositor origin, and screen bounds.
    fn resolve_float_geometry(
        &self,
        request: WindowFloatSurfaceRequest,
    ) -> WindowSurfaceResult<ResolvedWindowFloatGeometry> {
        let (anchor_grid_id, view_offset, anchor_grid_origin) =
            if let Some(anchor_window) = request.anchor_window {
                let descriptor = self
                    .grids
                    .lock_store()
                    .window_view_descriptor(anchor_window.grid_id)?;
                let origin = if descriptor.target == super::DEFAULT_GRID_ID {
                    ComposedPosition::new(0, 0)
                } else {
                    self.compositor
                        .registered_grid_origin(descriptor.target)?
                        .unwrap_or(anchor_window.fallback_origin)
                };
                (descriptor.target, descriptor.offset, origin)
            } else {
                (
                    super::DEFAULT_GRID_ID,
                    GridPosition::new(0, 0),
                    ComposedPosition::new(0, 0),
                )
            };

        request
            .geometry
            .resolve(
                request.anchor,
                anchor_grid_id,
                view_offset,
                anchor_grid_origin,
            )
            .ok_or(WindowSurfaceError::Compositor(
                CompositorError::InvalidGeometry,
            ))
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
