//! Safe coordination of the message grid, compositor layer, and UI events.

use crate::{
    ui::{
        GridOutputTarget,
        output::{OwnedRawLineCommand, UiOutput},
    },
    ui_compositor::{
        CompositorContext, CompositorGridPlacement, CompositorGridPlacementRequest,
        CompositorMessagePositionRequest, CompositorResult, CompositorRuntime,
        RegisteredGridPlacement,
    },
};

use super::{
    CMessageSurfaceRequest, CMessageSurfaceUpdate, ComposedGridSpace, GridContext, GridId,
    GridSize, GridViewDescriptor, LineGrid, MessageGridMode, MessageGridPositionUpdate,
    MessageGridTransition, VimSAttrT, VimSCharT,
};

/// Appearance and stacking policy of the message surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MessageSurfaceAppearance {
    separator_character: VimSCharT,
    z_index: i32,
    blending: bool,
}

/// Validated input for one complete message-surface lifecycle update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MessageSurfaceRequest {
    grid_id: Option<GridId>,
    screen_size: GridSize,
    should_allocate: bool,
    scrolled_lines: i32,
    pager_active: bool,
    command_height: i32,
    current_position: i32,
    appearance: MessageSurfaceAppearance,
    current_window_grid_id: Option<GridId>,
    clear_attribute: VimSAttrT,
}

impl CMessageSurfaceRequest {
    /// Converts C-owned policy values into validated native Rust state.
    pub(super) fn try_into_native(self) -> Option<MessageSurfaceRequest> {
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
        let screen_size = GridSize::try_new(self.screen_rows, self.screen_columns)?;
        if self.scrolled_lines < 0
            || self.command_height < 0
            || self.command_height > screen_size.height
            || self.current_position < 0
        {
            return None;
        }

        Some(MessageSurfaceRequest {
            grid_id,
            screen_size,
            should_allocate: self.should_allocate,
            scrolled_lines: self.scrolled_lines,
            pager_active: self.pager_active,
            command_height: self.command_height,
            current_position: self.current_position,
            appearance: MessageSurfaceAppearance {
                separator_character: self.position.separator_character,
                z_index: self.position.z_index,
                blending: self.blending,
            },
            current_window_grid_id,
            clear_attribute: self.clear_attribute,
        })
    }
}

/// Editor-owned state produced by one message-surface lifecycle update.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MessageSurfaceUpdate {
    grid_id: GridId,
    position: i32,
    redraw_cmdline: bool,
}

impl From<MessageSurfaceUpdate> for CMessageSurfaceUpdate {
    fn from(update: MessageSurfaceUpdate) -> Self {
        Self {
            grid_handle: update.grid_id.as_native_handle(),
            position: update.position,
            redraw_cmdline: update.redraw_cmdline,
        }
    }
}

/// Receives owned UI effects after Rust subsystem borrows are released.
pub(crate) trait MessageSurfaceOutput: UiOutput<ComposedGridSpace> {
    /// Publishes one message-grid position to uncomposed UI clients.
    fn emit_message_position(
        &mut self,
        grid_id: GridId,
        row: i32,
        scrolled: bool,
        separator_character: VimSCharT,
        z_index: i32,
        compositor_index: usize,
    );

    /// Publishes the allocated message-grid dimensions.
    fn emit_grid_resize(&mut self, grid_id: GridId, size: GridSize);

    /// Publishes final destruction of the message grid's current storage.
    fn emit_grid_destroy(&mut self, grid_id: GridId);

    /// Publishes owned grid-line updates produced while clearing a view.
    fn emit_grid_lines(&mut self, target: GridOutputTarget, lines: Vec<OwnedRawLineCommand>);
}

/// Safe coordinator for the complete message rendering surface.
pub(crate) struct MessageSurface<'context> {
    grids: &'context GridContext,
    compositor: &'context CompositorContext,
}

impl<'context> MessageSurface<'context> {
    /// Borrows the contexts participating in message rendering.
    pub(crate) const fn new(
        grids: &'context GridContext,
        compositor: &'context CompositorContext,
    ) -> Self {
        Self { grids, compositor }
    }

    /// Applies one complete visibility, storage, layer, and UI transition.
    pub(crate) fn update(
        &self,
        request: MessageSurfaceRequest,
        runtime: CompositorRuntime,
        output: &mut impl MessageSurfaceOutput,
    ) -> CompositorResult<MessageSurfaceUpdate> {
        debug_assert_eq!(request.screen_size, runtime.screen_size());
        let grid_id = self.resolve_grid_id(request.grid_id);
        let mode = if request.should_allocate {
            MessageGridMode::Visible(request.screen_size)
        } else {
            MessageGridMode::Hidden
        };
        let transition = {
            let mut line_buffer = self.grids.lock_line_buffer();
            let mut store = self.grids.lock_store();
            store
                .grid_mut(grid_id)
                .expect("message grid must remain permanently registered")
                .prepare_for_messages(mode, &mut line_buffer)
        };
        if transition == MessageGridTransition::Resized {
            self.grids
                .lock_message_state()
                .scroll
                .record_resize(request.scrolled_lines);
        }

        let mut position = request.current_position;
        let mut redraw_cmdline = false;
        match transition {
            MessageGridTransition::Resized => {
                position = if request.pager_active {
                    0
                } else {
                    (request.screen_size.height - request.command_height - request.scrolled_lines)
                        .max(0)
                };
                self.update_position(
                    grid_id,
                    position,
                    request.scrolled_lines != 0,
                    request,
                    runtime,
                    output,
                )?;

                let placement = CompositorGridPlacement::try_new(
                    position,
                    0,
                    request.screen_size.height,
                    request.screen_size.width,
                )
                .expect("validated message dimensions form a compositor placement");
                self.compositor.place_registered_grid(
                    self.grids,
                    RegisteredGridPlacement::new(
                        grid_id,
                        request.current_window_grid_id,
                        Some(grid_id),
                        CompositorGridPlacementRequest::new(
                            placement,
                            request.appearance.z_index,
                            request.appearance.blending,
                            false,
                            true,
                            false,
                        ),
                        runtime,
                    ),
                    output,
                )?;
                output.emit_grid_resize(grid_id, request.screen_size);
                self.grids.lock_message_state().activate_view(grid_id);
            }
            MessageGridTransition::ReleaseRequired => {
                self.compositor.remove_registered_grid(
                    self.grids,
                    grid_id,
                    Some(grid_id),
                    runtime,
                    output,
                )?;
                self.grids
                    .lock_store()
                    .grid_mut(grid_id)
                    .expect("message grid must remain permanently registered")
                    .release_message();
                output.emit_grid_destroy(grid_id);
                self.grids.lock_message_state().reset_view();
                redraw_cmdline = true;
            }
            MessageGridTransition::Unchanged
                if request.should_allocate
                    && request.scrolled_lines == 0
                    && position != request.screen_size.height - request.command_height =>
            {
                let max_rows = request.screen_size.height - request.command_height;
                let difference = position - max_rows;
                position = max_rows;
                self.update_position(grid_id, position, false, request, runtime, output)?;
                let descriptor = self.grids.lock_message_state().view();
                if difference > 0 {
                    let (target, lines) = self.grids.clear_view(
                        descriptor,
                        request.screen_size.height - difference,
                        request.screen_size.height,
                        0,
                        request.screen_size.width,
                        request.clear_attribute,
                    );
                    output.emit_grid_lines(target, lines);
                }
            }
            MessageGridTransition::Unchanged => {}
        }

        Ok(MessageSurfaceUpdate {
            grid_id,
            position,
            redraw_cmdline,
        })
    }

    /// Allocates and registers the message identity on first use.
    fn resolve_grid_id(&self, grid_id: Option<GridId>) -> GridId {
        grid_id.unwrap_or_else(|| {
            let grid_id = self.grids.allocate_grid_id();
            self.grids
                .lock_store()
                .register_empty(grid_id)
                .expect("newly allocated message identity must be unique");
            grid_id
        })
    }

    /// Updates the adjusted view and publishes an unthrottled position.
    fn update_position(
        &self,
        grid_id: GridId,
        row: i32,
        scrolled: bool,
        request: MessageSurfaceRequest,
        runtime: CompositorRuntime,
        output: &mut impl MessageSurfaceOutput,
    ) -> CompositorResult<()> {
        let MessageGridPositionUpdate {
            publish,
            view_row_offset,
        } = self
            .grids
            .lock_store()
            .grid(grid_id)
            .expect("message grid must remain permanently registered")
            .update_message_position(row);

        if publish {
            let compositor_index = self.compositor.route_message_position(
                self.grids,
                grid_id,
                CompositorMessagePositionRequest::new(
                    row,
                    request.appearance.z_index,
                    Some(request.appearance.separator_character),
                    request.screen_size,
                    scrolled,
                ),
                runtime,
                output,
            )?;
            output.emit_message_position(
                grid_id,
                row,
                scrolled,
                request.appearance.separator_character,
                request.appearance.z_index,
                compositor_index,
            );
        }

        let mut state = self.grids.lock_message_state();
        if publish {
            state.scroll.record_position(row);
        }
        if let Some(row_offset) = view_row_offset {
            state.set_view_row_offset(row_offset);
        }
        Ok(())
    }
}

impl GridContext {
    /// Clears a rectangular region through one persistent view descriptor.
    pub(super) fn clear_view(
        &self,
        descriptor: GridViewDescriptor,
        start_row: i32,
        end_row: i32,
        start_col: i32,
        mut end_col: i32,
        attribute: VimSAttrT,
    ) -> (GridOutputTarget, Vec<OwnedRawLineCommand>) {
        let start_row = start_row
            .checked_add(descriptor.offset.row)
            .expect("grid clear start-row translation overflowed");
        let end_row = end_row
            .checked_add(descriptor.offset.row)
            .expect("grid clear end-row translation overflowed");
        let mut line_buffer = self.lock_line_buffer();
        let mut store = self.lock_store();
        let grid = store
            .grid_mut(descriptor.target)
            .expect("grid view target must remain permanently registered");
        let target = GridOutputTarget::new(grid.id(), grid.size().width);
        let mut line_grid = LineGrid::new(grid);
        let mut lines = Vec::new();

        for row in start_row..end_row {
            line_grid.begin_grid(row, descriptor.offset.column, &mut line_buffer);
            end_col = std::cmp::min(end_col, line_grid.max_col());
            if line_grid.row_reaches_grid_end() || start_col >= end_col {
                line_grid.cancel();
                break;
            }
            line_grid.set_clear_range(start_col, end_col, attribute, 0);
            lines.extend(line_grid.flush(&mut line_buffer));
        }
        (target, lines)
    }
}
