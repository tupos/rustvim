//! C boundary adapters for message-grid operations.

use crate::{
    context::registered_context,
    defines::Integer,
    ui::{
        GridOutputTarget,
        output::{OwnedRawLineCommand, RawLineCells, RawLineCommand, UiCommand, UiOutput},
    },
    ui_compositor::{
        CompositorMessagePositionRequest, NvimComposedOutput, compositor_runtime_snapshot,
    },
};

use super::super::{
    CMessageGridPositionConfig, CMessageGridPositionEmitter, CMessageGridScrollFlushRequest,
    CMessageGridTransition, CMessageSurfaceRequest, CMessageSurfaceUpdate, ComposedGridSpace,
    GridId, GridSize, LineBufferGuard, MessageGridMode, MessageGridScrollFlushPlan,
    MessageGridScrollFlushRequest, MessageGridScrollUp, MessageGridScrollUpAction,
    MessageGridTransition, MessageSurfaceOutput, StoredGridGuard, VimHandleT, VimSCharT,
    lock_grid_store, lock_message_grid_state,
};

unsafe extern "C" {
    fn ui_uncomposed_call_grid_resize(grid: Integer, width: Integer, height: Integer);
    fn ui_call_grid_destroy(grid: Integer);
}

/// Final C UI adapter for one complete message-surface transition.
struct NvimMessageSurfaceOutput {
    composed: NvimComposedOutput,
    emit_position: CMessageGridPositionEmitter,
}

impl UiOutput<ComposedGridSpace> for NvimMessageSurfaceOutput {
    fn emit(&mut self, command: UiCommand<ComposedGridSpace>) {
        self.composed.emit(command);
    }

    fn emit_raw_line<Cells: RawLineCells>(&mut self, command: RawLineCommand<Cells>) {
        self.composed.emit_raw_line(command);
    }
}

impl MessageSurfaceOutput for NvimMessageSurfaceOutput {
    fn emit_message_position(
        &mut self,
        grid_id: GridId,
        row: i32,
        scrolled: bool,
        separator_character: VimSCharT,
        z_index: i32,
        compositor_index: usize,
    ) {
        let compositor_index = i32::try_from(compositor_index)
            .expect("compositor layer index must fit the C UI event representation");
        // SAFETY: The FFI caller guarantees the callback remains valid for the
        // complete synchronous lifecycle operation.
        unsafe {
            (self.emit_position)(
                grid_id.as_native_handle(),
                row,
                scrolled,
                separator_character,
                z_index,
                compositor_index,
            );
        }
    }

    fn emit_grid_resize(&mut self, grid_id: GridId, size: GridSize) {
        // SAFETY: The identity and dimensions were validated by safe Rust, and
        // the generated callback is synchronous.
        unsafe {
            ui_uncomposed_call_grid_resize(
                Integer::from(grid_id.as_native_handle()),
                Integer::from(size.width),
                Integer::from(size.height),
            );
        }
    }

    fn emit_grid_destroy(&mut self, grid_id: GridId) {
        // SAFETY: The stable identity is delivered synchronously at the final
        // C UI boundary.
        unsafe {
            ui_call_grid_destroy(Integer::from(grid_id.as_native_handle()));
        }
    }

    fn emit_grid_lines(&mut self, target: GridOutputTarget, lines: Vec<OwnedRawLineCommand>) {
        target.emit_lines(lines);
    }
}

/// Applies one complete message-surface lifecycle transition.
///
/// # Safety
///
/// `emit_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_update_message_surface(
    request: CMessageSurfaceRequest,
    emit_position: CMessageGridPositionEmitter,
) -> CMessageSurfaceUpdate {
    let request = request
        .try_into_native()
        .expect("message-surface request must contain valid editor state");
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let mut output = NvimMessageSurfaceOutput {
        composed: NvimComposedOutput,
        emit_position,
    };

    registered_context()
        .message_surface()
        .update(request, runtime, &mut output)
        .expect("message-surface update must preserve rendering invariants")
        .into()
}

/// Processes and publishes one message-grid position.
///
/// Composed output is emitted before the original uncomposed event callback.
///
/// # Safety
///
/// `emit_position` must remain callable for the duration of this operation.
pub(super) unsafe fn publish_message_grid_position(
    grid_handle: VimHandleT,
    row: libc::c_int,
    scrolled: bool,
    config: CMessageGridPositionConfig,
    emit_position: CMessageGridPositionEmitter,
) {
    let context = registered_context();
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let request = CompositorMessagePositionRequest::new(
        row,
        config.z_index,
        Some(config.separator_character),
        runtime.screen_size(),
        scrolled,
    );
    let mut output = NvimComposedOutput;
    let layer_index = context
        .compositor()
        .route_message_position(
            context.grid(),
            GridId::new(grid_handle),
            request,
            runtime,
            &mut output,
        )
        .expect("message-grid position update must preserve compositor invariants");
    let compositor_index = i32::try_from(layer_index)
        .expect("compositor layer index must fit the C UI event representation");

    // SAFETY: The caller guarantees that the callback is valid. All Rust
    // subsystem borrows were released before this callback can reenter editor
    // code.
    unsafe {
        emit_position(
            grid_handle,
            row,
            scrolled,
            config.separator_character,
            config.z_index,
            compositor_index,
        );
    }
}

/// Updates Rust-owned message position state and publishes when not throttled.
///
/// # Safety
///
/// `emit_position` must remain callable for the duration of this operation.
unsafe fn update_message_grid_position(
    grid_handle: VimHandleT,
    row: libc::c_int,
    scrolled: bool,
    config: CMessageGridPositionConfig,
    emit_position: CMessageGridPositionEmitter,
) {
    let update = {
        let Ok(grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
            return;
        };
        grid.update_message_position(row)
    };

    if update.publish {
        // SAFETY: The caller guarantees that the callback is valid.
        unsafe {
            publish_message_grid_position(grid_handle, row, scrolled, config, emit_position);
        }
    }

    let mut state = lock_message_grid_state();
    if update.publish {
        state.scroll.record_position(row);
    }
    if let Some(row_offset) = update.view_row_offset {
        state.set_view_row_offset(row_offset);
    }
}

/// Prepares the message grid for its requested visibility and size.
///
/// A `ReleaseRequired` result leaves storage intact so C can remove the
/// compositor layer before calling `grid_release_message`.
#[unsafe(no_mangle)]
pub extern "C" fn grid_prepare_for_messages(
    grid_handle: VimHandleT,
    rows: libc::c_int,
    columns: libc::c_int,
    should_allocate: bool,
    scrolled_lines: libc::c_int,
) -> CMessageGridTransition {
    let mode = if should_allocate {
        let Some(size) = GridSize::try_new(rows, columns) else {
            return CMessageGridTransition::Unchanged;
        };
        MessageGridMode::Visible(size)
    } else {
        MessageGridMode::Hidden
    };

    let transition = {
        let mut line_buffer = LineBufferGuard::acquire();
        let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
            return CMessageGridTransition::Unchanged;
        };
        grid.prepare_for_messages(mode, &mut line_buffer)
    };

    if transition == MessageGridTransition::Resized {
        lock_message_grid_state()
            .scroll
            .record_resize(scrolled_lines);
    }
    transition.into()
}

/// Releases message-grid storage after compositor removal.
#[unsafe(no_mangle)]
pub extern "C" fn grid_release_message(grid_handle: VimHandleT) {
    if let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) {
        grid.release_message();
    }
}

/// Selects the allocated message grid for the adjusted message view.
#[unsafe(no_mangle)]
pub extern "C" fn grid_activate_message_view(grid_handle: VimHandleT) {
    let grid_id = GridId::new(grid_handle);
    {
        let store = lock_grid_store();
        store
            .grid(grid_id)
            .expect("message grid identity must resolve to permanent storage");
    }
    lock_message_grid_state().activate_view(grid_id);
}

/// Restores an adjusted message view to the shared default grid.
#[unsafe(no_mangle)]
pub extern "C" fn grid_reset_message_view() {
    lock_message_grid_state().reset_view();
}

/// Updates the message-grid position and its adjusted view.
///
/// The position callback and view mutation occur only after the grid-store
/// borrow has been released.
///
/// Returns whether the position was published immediately.
///
/// # Safety
///
/// `emit_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_set_message_position(
    grid_handle: VimHandleT,
    row: libc::c_int,
    scrolled: bool,
    config: CMessageGridPositionConfig,
    emit_position: CMessageGridPositionEmitter,
) {
    // SAFETY: The caller guarantees that the callback is valid.
    unsafe {
        update_message_grid_position(grid_handle, row, scrolled, config, emit_position);
    }
}

/// Scrolls the message grid upward by one row.
///
/// Position callbacks and scroll events are emitted only after the grid-store
/// borrow has been released.
///
/// # Safety
///
/// `set_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_scroll_message_up(
    grid_handle: VimHandleT,
    current_position: libc::c_int,
    enable_throttling: bool,
    zero_cmdheight: bool,
    discount_first_scroll: bool,
    config: CMessageGridPositionConfig,
    set_position: CMessageGridPositionEmitter,
) -> libc::c_int {
    let grid_id = GridId::new(grid_handle);
    let scroll_up = {
        let Ok(mut grid) = StoredGridGuard::acquire(grid_id) else {
            return current_position;
        };
        grid.scroll_message_up(current_position, enable_throttling, zero_cmdheight)
    };
    let MessageGridScrollUp {
        started_throttling,
        action,
    } = scroll_up;

    if started_throttling && discount_first_scroll {
        lock_message_grid_state().scroll.discount_first_scroll();
    }
    match action {
        MessageGridScrollUpAction::Reposition {
            position,
            scrolled,
            clear_first_row,
        } => {
            // SAFETY: The caller guarantees that the callback is valid.
            unsafe {
                update_message_grid_position(grid_handle, position, scrolled, config, set_position);
            }
            if clear_first_row && let Ok(mut grid) = StoredGridGuard::acquire(grid_id) {
                grid.clear_first_message_row();
            }
            position
        }
        MessageGridScrollUpAction::ShiftRows { target, scroll } => {
            if let Some(scroll) = scroll {
                target.emit(scroll);
            }
            current_position
        }
    }
}

/// Clears message-grid rows in preparation for a screen redraw.
///
#[unsafe(no_mangle)]
pub extern "C" fn grid_prepare_message_redraw(
    grid_handle: VimHandleT,
    rows_to_clear: libc::c_int,
    valid_rows: libc::c_int,
) {
    let (Ok(rows_to_clear), Ok(valid_rows)) =
        (usize::try_from(rows_to_clear), usize::try_from(valid_rows))
    else {
        return;
    };

    {
        let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
            return;
        };
        grid.prepare_message_redraw(rows_to_clear, valid_rows);
    }
    lock_message_grid_state().scroll.reset_progress();
}

/// Ends message scrolling, publishes its resting position, and clears hidden
/// message rows.
///
/// The position callback runs after throttling is disabled and the store
/// borrow is released, but before hidden rows are cleared. This preserves the
/// original event order.
///
/// # Safety
///
/// `set_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_reset_message_scroll(
    grid_handle: VimHandleT,
    position: libc::c_int,
    rows_to_clear: libc::c_int,
    config: CMessageGridPositionConfig,
    set_position: CMessageGridPositionEmitter,
) -> libc::c_int {
    let Ok(rows_to_clear) = usize::try_from(rows_to_clear) else {
        return position;
    };
    let grid_id = GridId::new(grid_handle);

    {
        let Ok(mut grid) = StoredGridGuard::acquire(grid_id) else {
            return position;
        };
        grid.disable_message_throttling();
    }
    // SAFETY: The caller guarantees that the callback is valid.
    unsafe {
        update_message_grid_position(grid_handle, position, false, config, set_position);
    }
    {
        if let Ok(mut grid) = StoredGridGuard::acquire(grid_id) {
            grid.clear_message_rows(rows_to_clear, 0);
        }
    }
    lock_message_grid_state().scroll.reset_progress();
    position
}

/// Flushes pending throttled message-grid scrolling and row updates.
///
/// The message-position callback and UI commands are emitted only after the
/// grid-store borrow has been released.
///
/// # Safety
///
/// `emit_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_flush_message_scroll(
    grid_handle: VimHandleT,
    request: CMessageGridScrollFlushRequest,
    config: CMessageGridPositionConfig,
    emit_position: CMessageGridPositionEmitter,
) {
    let Some(screen_size) = GridSize::try_new(request.screen_rows, request.screen_columns) else {
        return;
    };
    let request = MessageGridScrollFlushRequest {
        position: request.position,
        scrolled_lines: request.scrolled_lines,
        screen_size,
        clear_attribute: request.clear_attribute,
    };
    let state = lock_message_grid_state().scroll;
    let plan = {
        let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
            return;
        };
        grid.flush_message_scroll(request, state)
    };

    let MessageGridScrollFlushPlan {
        target,
        position_update,
        scroll,
        lines,
    } = plan;
    if let Some(row) = position_update {
        // SAFETY: The caller guarantees that the callback is valid.
        unsafe {
            publish_message_grid_position(grid_handle, row, true, config, emit_position);
        }
    }
    if let Some(scroll) = scroll {
        target.emit(scroll);
    }
    target.emit_lines(lines);
    lock_message_grid_state()
        .scroll
        .finish_flush(request.position, request.scrolled_lines);
}
