//! C boundary adapters for persistent grid views.

use super::super::CGridCell;
use super::super::CMessageGridPositionConfig;
use super::super::CMessageGridPositionEmitter;
use super::super::DEFAULT_GRID_ID;
use super::super::GridId;
use super::super::GridPosition;
use super::super::GridSize;
use super::super::GridViewDescriptor;
use super::super::StoredGridGuard;
use super::super::VimHandleT;
use super::super::WindowLineScrollRequest;
use super::super::lock_grid_store;
use super::super::lock_message_grid_state;
use super::message::publish_message_grid_position;
use crate::ui::GridOutputTarget;

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
    config: CMessageGridPositionConfig,
    emit_position: CMessageGridPositionEmitter,
) {
    let grid_id = GridId::new(grid_handle);
    let resize = {
        let Ok(grid) = StoredGridGuard::acquire(grid_id) else {
            return;
        };
        grid.resize_command_if_allocated()
    };
    let Some((target, command)) = resize else {
        return;
    };

    target.emit(command);
    // SAFETY: The caller guarantees that the callback is valid.
    unsafe {
        publish_message_grid_position(grid_handle, position, scrolled, config, emit_position);
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

/// Translates window-local offsets and returns the selected grid handle.
///
/// # Safety
///
/// `row_offset` and `col_offset` must point to valid, properly aligned
/// writable integers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_view_translate(
    window_grid_handle: VimHandleT,
    row_offset: *mut libc::c_int,
    col_offset: *mut libc::c_int,
) -> VimHandleT {
    assert!(
        !row_offset.is_null() && !col_offset.is_null(),
        "window-view position is nullptr"
    );
    let store = lock_grid_store();
    let Ok(view) = store.window_view(GridId::new(window_grid_handle)) else {
        return 0;
    };

    // SAFETY: The caller guarantees that both pointers reference initialized
    // integers.
    let position = unsafe { GridPosition::new(*row_offset, *col_offset) };
    let position = view
        .translated_position(position)
        .expect("window-view coordinate translation overflowed");
    let target = view.target.id();

    // SAFETY: The caller guarantees exclusive writable access to both
    // position outputs.
    unsafe {
        *row_offset = position.row;
        *col_offset = position.column;
    }
    target.as_native_handle()
}

/// Translates grid-local offsets through the adjusted message view.
///
/// Returns the selected grid handle.
///
/// # Safety
///
/// `row_offset` and `col_offset` must point to valid, properly aligned
/// writable integers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_message_view_translate(
    row_offset: *mut libc::c_int,
    col_offset: *mut libc::c_int,
) -> VimHandleT {
    assert!(
        !row_offset.is_null() && !col_offset.is_null(),
        "message-view position is nullptr"
    );
    let descriptor = lock_message_grid_state().view();
    let (grid_id, position) = {
        let store = lock_grid_store();
        let view = store
            .view(descriptor)
            .expect("message view must resolve to permanent storage");
        // SAFETY: The caller guarantees that both pointers reference
        // initialized integers.
        let position = unsafe { GridPosition::new(*row_offset, *col_offset) };
        let position = view
            .translated_position(position)
            .expect("message-view coordinate translation overflowed");
        (view.target.id(), position)
    };

    // SAFETY: The caller guarantees exclusive writable access to both
    // position outputs.
    unsafe {
        *row_offset = position.row;
        *col_offset = position.column;
    }
    grid_id.as_native_handle()
}

/// Translates a target-grid position into coordinates relative to a grid view.
///
/// Returns `false` when the window has no registered view, either output
/// pointer is null, or coordinate subtraction overflows.
///
/// # Safety
///
/// `row` and `column` must be null or point to valid, properly aligned
/// writable integers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_view_translate_to_local(
    window_grid_handle: VimHandleT,
    row: *mut libc::c_int,
    column: *mut libc::c_int,
) -> bool {
    if row.is_null() || column.is_null() {
        return false;
    }

    let store = lock_grid_store();
    let Ok(view) = store.window_view(GridId::new(window_grid_handle)) else {
        return false;
    };

    // SAFETY: Both output pointers were checked for null, and the caller
    // guarantees that they reference valid integers.
    let position = unsafe { GridPosition::new(*row, *column) };
    let Some(position) = view.local_position(position) else {
        return false;
    };

    // SAFETY: Both pointers were checked for null, and the caller guarantees
    // exclusive writable access.
    unsafe {
        *row = position.row;
        *column = position.column;
    }
    true
}

/// Marks one row addressed through a registered window view as invalid.
#[unsafe(no_mangle)]
pub extern "C" fn grid_view_invalidate_row(window_grid_handle: VimHandleT, row: libc::c_int) {
    let mut store = lock_grid_store();
    let Ok(descriptor) = store.window_view_descriptor(GridId::new(window_grid_handle)) else {
        return;
    };
    let Some(row) = row
        .checked_add(descriptor.offset.row)
        .and_then(|row| usize::try_from(row).ok())
    else {
        return;
    };

    if let Ok(grid) = store.grid_mut(descriptor.target) {
        grid.invalidate_row(row);
    }
}

/// Reads one cell through a grid view after validating its target identity.
///
/// Returns `false` when the view has no target, its target does not match
/// `expected_grid`, coordinate translation overflows, or the translated
/// position does not identify an allocated cell.
///
/// # Safety
///
/// `output` must point to valid, properly aligned writable `CGridCell` storage
/// that does not alias the target grid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_view_get_cell(
    window_grid_handle: VimHandleT,
    expected_grid: VimHandleT,
    row: libc::c_int,
    column: libc::c_int,
    output: *mut CGridCell,
) -> bool {
    if output.is_null() {
        return false;
    }

    let store = lock_grid_store();
    let Ok(view) = store.window_view(GridId::new(window_grid_handle)) else {
        return false;
    };
    let Some(cell) = view.cell_for_grid(GridId::new(expected_grid), GridPosition::new(row, column))
    else {
        return false;
    };

    // SAFETY: The output pointer was checked for null above and the caller
    // guarantees valid writable storage.
    unsafe {
        output.write(CGridCell::from(cell));
    }
    true
}
