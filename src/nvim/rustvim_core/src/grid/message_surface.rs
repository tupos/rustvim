//! Safe coordination of the message grid, compositor layer, and UI events.

use crate::{
    ui::{
        GridOutputTarget,
        output::{OwnedRawLineCommand, UiCommand},
    },
    ui_compositor::{
        CompositorContext, CompositorGridPlacement, CompositorGridPlacementRequest,
        CompositorMessagePositionRequest, CompositorResult, CompositorRuntime,
        RegisteredGridPlacement,
    },
};

use super::{
    CMessageSurfaceRequest, CMessageSurfaceScrollRequest, CMessageSurfaceUpdate, GridContext,
    GridId, GridSize, GridSurfaceOutput, GridViewDescriptor, LineGrid, LocalGridSpace,
    MessageGridMode, MessageGridPositionUpdate, MessageGridScrollFlushPlan,
    MessageGridScrollFlushRequest, MessageGridScrollUp, MessageGridScrollUpAction,
    MessageGridTransition, VimSAttrT, VimSCharT,
};

/// Appearance and stacking policy of the message surface.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MessageSurfaceAppearance {
    separator_character: VimSCharT,
    z_index: i32,
    blending: bool,
}

/// Rendering policy needed to publish one message-grid position.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MessagePositionPolicy {
    separator_character: VimSCharT,
    z_index: i32,
}

impl MessagePositionPolicy {
    /// Creates position policy from validated editor state.
    pub(super) const fn new(separator_character: VimSCharT, z_index: i32) -> Self {
        Self {
            separator_character,
            z_index,
        }
    }
}

/// One validated message-grid position transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MessagePositionRequest {
    grid_id: GridId,
    row: i32,
    scrolled: bool,
    screen_size: GridSize,
    policy: MessagePositionPolicy,
}

impl MessagePositionRequest {
    /// Creates a position request for a registered message grid.
    pub(super) const fn new(
        grid_id: GridId,
        row: i32,
        scrolled: bool,
        screen_size: GridSize,
        policy: MessagePositionPolicy,
    ) -> Self {
        Self {
            grid_id,
            row,
            scrolled,
            screen_size,
            policy,
        }
    }
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

impl MessageSurfaceRequest {
    /// Returns the UI policy shared by position changes during this update.
    const fn position_policy(self) -> MessagePositionPolicy {
        MessagePositionPolicy::new(self.appearance.separator_character, self.appearance.z_index)
    }
}

/// Validated input for one upward message-surface scroll.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct MessageSurfaceScrollRequest {
    grid_id: GridId,
    current_position: i32,
    screen_size: GridSize,
    enable_throttling: bool,
    zero_cmdheight: bool,
    discount_first_scroll: bool,
    position: MessagePositionPolicy,
    clear_attribute: VimSAttrT,
}

impl CMessageSurfaceScrollRequest {
    /// Converts C-owned scroll policy into validated native Rust state.
    pub(super) fn try_into_native(self) -> Option<MessageSurfaceScrollRequest> {
        if self.grid_handle <= 0 || self.current_position < 0 {
            return None;
        }
        let screen_size = GridSize::try_new(self.screen_rows, self.screen_columns)?;
        Some(MessageSurfaceScrollRequest {
            grid_id: GridId::new(self.grid_handle),
            current_position: self.current_position,
            screen_size,
            enable_throttling: self.enable_throttling,
            zero_cmdheight: self.zero_cmdheight,
            discount_first_scroll: self.discount_first_scroll,
            position: MessagePositionPolicy::new(
                self.position.separator_character,
                self.position.z_index,
            ),
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
pub(crate) trait MessageSurfaceOutput: GridSurfaceOutput {
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

    /// Publishes one message-grid command after storage borrows are released.
    fn emit_grid_command(&mut self, target: GridOutputTarget, command: UiCommand<LocalGridSpace>);

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
                self.set_position(
                    MessagePositionRequest::new(
                        grid_id,
                        position,
                        request.scrolled_lines != 0,
                        request.screen_size,
                        request.position_policy(),
                    ),
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
                self.set_position(
                    MessagePositionRequest::new(
                        grid_id,
                        position,
                        false,
                        request.screen_size,
                        request.position_policy(),
                    ),
                    runtime,
                    output,
                )?;
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
    pub(super) fn set_position(
        &self,
        request: MessagePositionRequest,
        runtime: CompositorRuntime,
        output: &mut impl MessageSurfaceOutput,
    ) -> CompositorResult<()> {
        let MessagePositionRequest { grid_id, row, .. } = request;
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
            self.publish_position(request, runtime, output)?;
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

    /// Publishes one position without changing message scroll bookkeeping.
    pub(super) fn publish_position(
        &self,
        request: MessagePositionRequest,
        runtime: CompositorRuntime,
        output: &mut impl MessageSurfaceOutput,
    ) -> CompositorResult<()> {
        let compositor_index = self.compositor.route_message_position(
            self.grids,
            request.grid_id,
            CompositorMessagePositionRequest::new(
                request.row,
                request.policy.z_index,
                Some(request.policy.separator_character),
                request.screen_size,
                request.scrolled,
            ),
            runtime,
            output,
        )?;
        output.emit_message_position(
            request.grid_id,
            request.row,
            request.scrolled,
            request.policy.separator_character,
            request.policy.z_index,
            compositor_index,
        );
        Ok(())
    }

    /// Scrolls the complete message surface upward by one row.
    pub(super) fn scroll_up(
        &self,
        request: MessageSurfaceScrollRequest,
        runtime: CompositorRuntime,
        output: &mut impl MessageSurfaceOutput,
    ) -> CompositorResult<i32> {
        let scroll_up = {
            let mut store = self.grids.lock_store();
            store
                .grid_mut(request.grid_id)
                .expect("message grid must remain permanently registered")
                .scroll_message_up(
                    request.current_position,
                    request.enable_throttling,
                    request.zero_cmdheight,
                )
        };
        let MessageGridScrollUp {
            started_throttling,
            action,
        } = scroll_up;

        if started_throttling && request.discount_first_scroll {
            self.grids
                .lock_message_state()
                .scroll
                .discount_first_scroll();
        }
        let position = match action {
            MessageGridScrollUpAction::Reposition {
                position,
                scrolled,
                clear_first_row,
            } => {
                self.set_position(
                    MessagePositionRequest::new(
                        request.grid_id,
                        position,
                        scrolled,
                        request.screen_size,
                        request.position,
                    ),
                    runtime,
                    output,
                )?;
                if clear_first_row {
                    self.grids
                        .lock_store()
                        .grid_mut(request.grid_id)
                        .expect("message grid must remain permanently registered")
                        .clear_first_message_row();
                }
                position
            }
            MessageGridScrollUpAction::ShiftRows { target, scroll } => {
                if let Some(scroll) = scroll {
                    output.emit_grid_command(target, scroll);
                }
                request.current_position
            }
        };

        let descriptor = self.grids.lock_message_state().view();
        let (target, lines) = self.grids.clear_view(
            descriptor,
            request.screen_size.height - 1,
            request.screen_size.height,
            0,
            request.screen_size.width,
            request.clear_attribute,
        );
        output.emit_grid_lines(target, lines);
        Ok(position)
    }

    /// Flushes pending throttled message-grid scrolling and row updates.
    pub(super) fn flush_scroll(
        &self,
        grid_id: GridId,
        request: MessageGridScrollFlushRequest,
        position: MessagePositionPolicy,
        runtime: CompositorRuntime,
        output: &mut impl MessageSurfaceOutput,
    ) -> CompositorResult<()> {
        let state = self.grids.lock_message_state().scroll;
        let plan = self
            .grids
            .lock_store()
            .grid_mut(grid_id)
            .expect("message grid must remain permanently registered")
            .flush_message_scroll(request, state);

        let MessageGridScrollFlushPlan {
            target,
            position_update,
            scroll,
            lines,
        } = plan;
        if let Some(row) = position_update {
            self.publish_position(
                MessagePositionRequest::new(grid_id, row, true, request.screen_size, position),
                runtime,
                output,
            )?;
        }
        if let Some(scroll) = scroll {
            output.emit_grid_command(target, scroll);
        }
        output.emit_grid_lines(target, lines);
        self.grids
            .lock_message_state()
            .scroll
            .finish_flush(request.position, request.scrolled_lines);
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
