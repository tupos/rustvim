//! C boundary adapters for screen-grid lifecycle operations.

use crate::globals;

use super::super::{
    CWindowGridUpdate, DEFAULT_GRID_ID, GlyphMapping, GridId, GridLifecycleChange, GridPosition,
    GridSize, LineBufferGuard, MessageGridState, StoredGridGuard, VimHandleT, WindowGridMode,
    lock_glyph_mapping, lock_grid_store, lock_message_grid_state, register_empty_grid,
    release_permanent_grid,
};

#[unsafe(no_mangle)]
pub extern "C" fn alloc_initial_line_buffer() {
    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.realloc(unsafe { globals::Columns as usize });
}

/// Reports whether a stored grid currently owns cell buffers.
///
/// Missing, invalid, and retired identities are not allocated.
#[unsafe(no_mangle)]
pub extern "C" fn grid_is_allocated(grid_handle: VimHandleT) -> bool {
    if grid_handle <= 0 {
        return false;
    }

    StoredGridGuard::acquire(GridId::new(grid_handle)).is_ok_and(|grid| grid.is_allocated())
}

/// Reports whether the current screen update must invalidate composed output.
///
/// This combines the requested full-clear state with the stored grid's redraw
/// state. A missing identity conservatively requires compositor invalidation.
#[unsafe(no_mangle)]
pub extern "C" fn grid_requires_compositor_invalidation(
    grid_handle: VimHandleT,
    clear_requested: bool,
) -> bool {
    let Ok(grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
        return true;
    };

    grid.requires_compositor_invalidation(clear_requested)
}

/// Revalidates invalid grid contents in preparation for a complete redraw.
///
/// A grid that does not require a redraw remains unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn grid_revalidate_for_redraw(grid_handle: VimHandleT) {
    if let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) {
        grid.revalidate_for_redraw();
    }
}

/// Records that a grid requires a complete redraw.
#[unsafe(no_mangle)]
pub extern "C" fn grid_require_full_redraw(grid_handle: VimHandleT) {
    if let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) {
        grid.require_full_redraw();
    }
}

/// Assigns and permanently stores the default screen grid.
#[unsafe(no_mangle)]
pub extern "C" fn grid_initialize_default() {
    register_empty_grid(DEFAULT_GRID_ID);

    *lock_message_grid_state() = MessageGridState::new();
}

/// Resizes the default screen grid when its configured dimensions changed.
///
/// Returns whether a resize was performed.
///
#[unsafe(no_mangle)]
pub extern "C" fn grid_configure_default(rows: libc::c_int, columns: libc::c_int) -> bool {
    let Some(size) = GridSize::try_new(rows, columns) else {
        return false;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    let Ok(mut grid) = StoredGridGuard::acquire(DEFAULT_GRID_ID) else {
        return false;
    };
    grid.configure_as_default(size, &mut line_buffer) == GridLifecycleChange::Resized
}

/// Applies the complete allocation, validity, and view transition for one
/// window grid.
///
/// Returns the configured grid identity and whether dedicated storage was
/// allocated or resized. The window subsystem uses that result for border
/// redraw and UI resize notification.
///
#[unsafe(no_mangle)]
pub extern "C" fn grid_configure_for_window(
    grid_handle: VimHandleT,
    rows: libc::c_int,
    columns: libc::c_int,
    use_dedicated_grid: bool,
    row_offset: libc::c_int,
    col_offset: libc::c_int,
) -> CWindowGridUpdate {
    let mode = if use_dedicated_grid {
        let Some(size) = GridSize::try_new(rows, columns) else {
            return CWindowGridUpdate {
                grid_handle: 0,
                resized: false,
                row_offset_changed: false,
            };
        };
        WindowGridMode::Dedicated(size)
    } else {
        WindowGridMode::Shared
    };

    let (update, previous_row_offset) = {
        let mut line_buffer = LineBufferGuard::acquire();
        let mut store = lock_grid_store();
        let grid_id = GridId::new(grid_handle);
        let previous_row_offset = store
            .window_view_descriptor(grid_id)
            .map(|descriptor| descriptor.offset.row)
            .unwrap_or(0);
        let update = {
            let Ok(rust_grid) = store.grid_mut(grid_id) else {
                return CWindowGridUpdate {
                    grid_handle: 0,
                    resized: false,
                    row_offset_changed: false,
                };
            };
            rust_grid.configure_window_view(
                mode,
                GridPosition::new(row_offset, col_offset),
                &mut line_buffer,
            )
        };
        if store.set_window_view(grid_id, update.view).is_err() {
            return CWindowGridUpdate {
                grid_handle: 0,
                resized: false,
                row_offset_changed: false,
            };
        }
        (update, previous_row_offset)
    };

    CWindowGridUpdate::from_update(update, previous_row_offset)
}

/// Releases one permanently stored grid by its stable identity.
#[unsafe(no_mangle)]
pub extern "C" fn grid_free(grid_handle: VimHandleT) {
    release_permanent_grid(GridId::new(grid_handle));
}

/// Releases all process-global grid resources during editor shutdown.
#[unsafe(no_mangle)]
pub extern "C" fn grid_free_all_mem() {
    lock_grid_store().clear();

    *lock_message_grid_state() = MessageGridState::new();

    // The caller guarantees that no line-buffer user remains.
    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.release();
    drop(line_buffer);

    // Moving the mapping out of the mutex releases all of its allocations.
    let glyph_mapping = {
        let mut mapping = lock_glyph_mapping();
        std::mem::replace(&mut *mapping, GlyphMapping::new())
    };
    drop(glyph_mapping);
}

/// Clears every allocated logical row.
///
#[unsafe(no_mangle)]
pub extern "C" fn grid_clear_default(valid: bool) -> bool {
    let Ok(mut grid) = StoredGridGuard::acquire(DEFAULT_GRID_ID) else {
        return false;
    };
    grid.clear_all(valid)
}

/// Clears a half-open range of complete logical rows.
///
/// Rows beyond the allocated grid are ignored.
///
#[unsafe(no_mangle)]
pub extern "C" fn grid_clear_default_rows(
    start_row: libc::c_int,
    end_row: libc::c_int,
    valid: bool,
) {
    let (Ok(start_row), Ok(end_row)) = (usize::try_from(start_row), usize::try_from(end_row))
    else {
        return;
    };

    if let Ok(mut grid) = StoredGridGuard::acquire(DEFAULT_GRID_ID) {
        grid.clear_rows(start_row..end_row, valid);
    }
}
