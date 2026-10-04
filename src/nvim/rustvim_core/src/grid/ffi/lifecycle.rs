//! C boundary adapters for screen-grid lifecycle operations.

use crate::{context::registered_context, globals};

use super::super::{
    DEFAULT_GRID_ID, GlyphMapping, GridId, GridLifecycleChange, GridPosition, GridSize,
    LineBufferGuard, MessageGridState, StoredGridGuard, VimHandleT, WindowGridMode,
    WindowSurfaceRequest, WindowSurfaceUpdate, lock_glyph_mapping, lock_grid_store,
    lock_message_grid_state, register_empty_grid, release_permanent_grid,
};

use super::surface::NvimGridSurfaceOutput;

/// C representation of one complete window-grid configuration request.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CWindowSurfaceRequest {
    pub(crate) grid_handle: VimHandleT,
    pub(crate) rows: libc::c_int,
    pub(crate) columns: libc::c_int,
    pub(crate) use_dedicated_grid: bool,
    pub(crate) row_offset: libc::c_int,
    pub(crate) column_offset: libc::c_int,
    pub(crate) force_dimensions_publication: bool,
    pub(crate) bordered_float: bool,
}

impl CWindowSurfaceRequest {
    /// Converts C-owned window state into a validated native request.
    fn try_into_native(self) -> Option<WindowSurfaceRequest> {
        if self.grid_handle <= 0 {
            return None;
        }
        let mode = if self.use_dedicated_grid {
            WindowGridMode::Dedicated(GridSize::try_new(self.rows, self.columns)?)
        } else {
            WindowGridMode::Shared
        };

        Some(WindowSurfaceRequest::new(
            GridId::new(self.grid_handle),
            mode,
            GridPosition::new(self.row_offset, self.column_offset),
            self.force_dimensions_publication,
            self.bordered_float,
        ))
    }
}

/// C representation of editor-owned actions after window configuration.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CWindowSurfaceUpdate {
    pub(crate) grid_handle: VimHandleT,
    pub(crate) row_offset_changed: bool,
    pub(crate) redraw_border: bool,
    pub(crate) republish_cursor: bool,
}

impl CWindowSurfaceUpdate {
    /// Returns the failure result expected by the existing C caller.
    const fn invalid() -> Self {
        Self {
            grid_handle: 0,
            row_offset_changed: false,
            redraw_border: false,
            republish_cursor: false,
        }
    }
}

impl From<WindowSurfaceUpdate> for CWindowSurfaceUpdate {
    fn from(update: WindowSurfaceUpdate) -> Self {
        Self {
            grid_handle: update.grid_id.as_native_handle(),
            row_offset_changed: update.row_offset_changed,
            redraw_border: update.redraw_border,
            republish_cursor: update.republish_cursor,
        }
    }
}

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

/// Configures one complete window surface and applies its resize output.
#[unsafe(no_mangle)]
pub extern "C" fn grid_configure_window_surface(
    request: CWindowSurfaceRequest,
) -> CWindowSurfaceUpdate {
    let Some(request) = request.try_into_native() else {
        return CWindowSurfaceUpdate::invalid();
    };
    let mut output = NvimGridSurfaceOutput::new();

    registered_context()
        .window_surface()
        .configure(request, &mut output)
        .map(CWindowSurfaceUpdate::from)
        .unwrap_or_else(|_| CWindowSurfaceUpdate::invalid())
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
