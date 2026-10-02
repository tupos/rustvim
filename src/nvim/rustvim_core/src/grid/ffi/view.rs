//! C boundary adapters for persistent grid views.

use crate::ui::GridOutputTarget;

use super::super::{
    CMessageGridPositionEmitter, DEFAULT_GRID_ID, GridId, GridPosition, GridSize,
    GridViewDescriptor, StoredGridGuard, VimHandleT, WindowLineScrollRequest, lock_grid_store,
    lock_message_grid_state,
};

/// Reports whether a window grid owns a registered view.
#[unsafe(no_mangle)]
pub extern "C" fn grid_view_has_target(window_grid_handle: VimHandleT) -> bool {
    lock_grid_store()
        .window_view_descriptor(GridId::new(window_grid_handle))
        .is_ok()
}

/// Reports whether a window view targets the shared default grid.
#[unsafe(no_mangle)]
pub extern "C" fn grid_view_targets_default(window_grid_handle: VimHandleT) -> bool {
    let Ok(descriptor) = lock_grid_store().window_view_descriptor(GridId::new(window_grid_handle))
    else {
        return false;
    };
    descriptor.target == DEFAULT_GRID_ID
}

/// Reports whether a grid view requires a complete redraw.
///
/// A window without a registered view requires redraw because it cannot
/// currently render a cursor.
#[unsafe(no_mangle)]
pub extern "C" fn grid_view_requires_redraw(window_grid_handle: VimHandleT) -> bool {
    let store = lock_grid_store();
    let Ok(view) = store.window_view(GridId::new(window_grid_handle)) else {
        return true;
    };

    view.requires_redraw()
}

/// Reports whether a window view's target has `width` columns.
#[unsafe(no_mangle)]
pub extern "C" fn grid_view_width_matches_target(
    window_grid_handle: VimHandleT,
    width: libc::c_int,
) -> bool {
    let store = lock_grid_store();
    let Ok(view) = store.window_view(GridId::new(window_grid_handle)) else {
        return false;
    };

    view.width_matches_target(width)
}

/// Moves the UI cursor through a persistent view descriptor.
fn move_grid_view_cursor(descriptor: GridViewDescriptor, position: GridPosition) {
    let output = {
        let store = lock_grid_store();
        let Ok(view) = store.view(descriptor) else {
            return;
        };
        view.plan_cursor_move(position)
    };
    if let Some((target, command)) = output {
        target.emit(command);
    }
}

/// Moves the UI cursor through a registered window view.
///
/// Grid ownership is restored before the cursor command is emitted.
#[unsafe(no_mangle)]
pub extern "C" fn grid_view_cursor_goto(
    window_grid_handle: VimHandleT,
    row: libc::c_int,
    column: libc::c_int,
) {
    let Ok(descriptor) = lock_grid_store().window_view_descriptor(GridId::new(window_grid_handle))
    else {
        return;
    };
    move_grid_view_cursor(descriptor, GridPosition::new(row, column));
}

/// Moves the UI cursor through the adjusted message view.
#[unsafe(no_mangle)]
pub extern "C" fn grid_message_view_cursor_goto(row: libc::c_int, column: libc::c_int) {
    let descriptor = lock_message_grid_state().view();
    move_grid_view_cursor(descriptor, GridPosition::new(row, column));
}

/// Refreshes the external representation of an allocated message grid.
///
/// The resize command and position callback run after the grid-store borrow
/// has been released.
///
/// # Safety
///
/// `emit_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_refresh_message_ui(
    grid_handle: VimHandleT,
    position: libc::c_int,
    scrolled: bool,
    emit_position: CMessageGridPositionEmitter,
) {
    let resize = {
        let Ok(grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
            return;
        };
        grid.resize_command_if_allocated()
    };
    let Some((target, command)) = resize else {
        return;
    };

    target.emit(command);
    // SAFETY: The caller guarantees that the callback is valid. The store
    // borrow was released before this callback can reenter editor code.
    unsafe {
        emit_position(position, scrolled);
    }
}

/// Scrolls lines within a registered window view.
#[unsafe(no_mangle)]
pub extern "C" fn grid_scroll_window_lines(
    window_grid_handle: VimHandleT,
    row: libc::c_int,
    line_count: libc::c_int,
    view_height: libc::c_int,
    view_width: libc::c_int,
) {
    let Some(view_size) = GridSize::try_new(view_height, view_width) else {
        return;
    };

    let (target, command) = {
        let mut store = lock_grid_store();
        let Ok(descriptor) = store.window_view_descriptor(GridId::new(window_grid_handle)) else {
            return;
        };
        let request = WindowLineScrollRequest {
            row,
            line_count,
            view_origin: descriptor.offset,
            view_size,
        };
        let Ok(grid) = store.grid_mut(descriptor.target) else {
            return;
        };
        let target = GridOutputTarget::new(grid.id(), grid.size().width);
        let command = grid.scroll_window_lines(request);
        (target, command)
    };
    if let Some(command) = command {
        target.emit(command);
    }
}
