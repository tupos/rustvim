//! C boundary adapters for message-grid operations.

use super::super::{
    CMessageGridPositionEmitter, CMessageGridScrollFlushRequest, CMessageGridTransition, GridId,
    GridSize, LineBufferGuard, MessageGridMode, MessageGridScrollFlushPlan,
    MessageGridScrollFlushRequest, MessageGridScrollUp, MessageGridScrollUpAction,
    MessageGridTransition, StoredGridGuard, VimHandleT, lock_grid_store, lock_message_grid_state,
};

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
    emit_position: CMessageGridPositionEmitter,
) {
    let update = {
        let Ok(grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
            return;
        };
        grid.update_message_position(row)
    };

    if update.publish {
        // SAFETY: The caller guarantees that the callback is valid. The grid
        // store borrow was released before this callback can reenter editor
        // code.
        unsafe {
            emit_position(row, scrolled);
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
    set_position: CMessageGridPositionEmitter,
) {
    let grid_id = GridId::new(grid_handle);
    let scroll_up = {
        let Ok(mut grid) = StoredGridGuard::acquire(grid_id) else {
            return;
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
            // SAFETY: The caller guarantees that the callback is valid. The
            // store borrow was released before this callback can reenter
            // editor code.
            unsafe {
                set_position(position, scrolled);
            }
            if clear_first_row && let Ok(mut grid) = StoredGridGuard::acquire(grid_id) {
                grid.clear_first_message_row();
            }
        }
        MessageGridScrollUpAction::ShiftRows { target, scroll } => {
            if let Some(scroll) = scroll {
                target.emit(scroll);
            }
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
    set_position: CMessageGridPositionEmitter,
) {
    let Ok(rows_to_clear) = usize::try_from(rows_to_clear) else {
        return;
    };
    let grid_id = GridId::new(grid_handle);

    {
        let Ok(mut grid) = StoredGridGuard::acquire(grid_id) else {
            return;
        };
        grid.disable_message_throttling();
    }
    unsafe {
        set_position(position, false);
    }
    {
        if let Ok(mut grid) = StoredGridGuard::acquire(grid_id) {
            grid.clear_message_rows(rows_to_clear, 0);
        }
    }
    lock_message_grid_state().scroll.reset_progress();
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
        // SAFETY: The caller guarantees that the callback is valid. The grid
        // guard was dropped before this callback can reenter editor code.
        unsafe {
            emit_position(row, true);
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
