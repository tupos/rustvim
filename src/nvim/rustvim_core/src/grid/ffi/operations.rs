//! C boundary adapters for general screen-grid operations.

use super::super::CGridCell;
use super::super::CGridCellInspectionResult;
use super::super::DEFAULT_GRID_HANDLE;
use super::super::GridId;
use super::super::GridInspectionTarget;
use super::super::InvalidGridId;
use super::super::StoredGridGuard;
use super::super::VimHandleT;
use super::super::allocate_grid_id;
use super::super::register_empty_grid;
use crate::ui::GridOutputTarget;

/// Invalidates every cell in a permanently stored grid.
///
/// Missing grid identities are ignored.
#[unsafe(no_mangle)]
pub extern "C" fn grid_invalidate(grid_handle: VimHandleT) {
    if let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) {
        grid.invalidate();
    }
}

/// Selects the requested API grid and reads one complete logical cell.
///
/// An invalid window-grid identity is distinct from a valid grid whose
/// coordinates do not identify a cell.
/// Grid contents are resolved directly from permanent Rust storage.
///
/// # Safety
///
/// `output` must point to valid, properly aligned writable `CGridCell` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_inspect_cell(
    popup_grid_handle: VimHandleT,
    window_grid_handle: VimHandleT,
    requested_grid: i64,
    row: i64,
    column: i64,
    output: *mut CGridCell,
) -> CGridCellInspectionResult {
    if output.is_null() {
        return CGridCellInspectionResult::InvalidGrid;
    }

    let popup_grid_id = GridId::new(popup_grid_handle);
    let window_grid_id =
        (window_grid_handle > DEFAULT_GRID_HANDLE).then_some(GridId::new(window_grid_handle));
    let target = match GridInspectionTarget::from_requested_handle(
        requested_grid,
        popup_grid_id,
        window_grid_id,
    ) {
        Ok(target) => target,
        Err(InvalidGridId) => return CGridCellInspectionResult::InvalidGrid,
    };

    let selected_grid_id = match target {
        GridInspectionTarget::Default => GridId::new(DEFAULT_GRID_HANDLE),
        GridInspectionTarget::PopupMenu if popup_grid_handle == 0 => {
            return CGridCellInspectionResult::MissingCell;
        }
        GridInspectionTarget::PopupMenu => popup_grid_id,
        GridInspectionTarget::Window(grid_id) => grid_id,
    };
    let Ok(selected_grid) = StoredGridGuard::acquire(selected_grid_id) else {
        return CGridCellInspectionResult::InvalidGrid;
    };
    if matches!(target, GridInspectionTarget::Window(_)) && !selected_grid.is_allocated() {
        return CGridCellInspectionResult::InvalidGrid;
    };

    let (Ok(row), Ok(column)) = (usize::try_from(row), usize::try_from(column)) else {
        return CGridCellInspectionResult::MissingCell;
    };
    let Some(cell) = selected_grid.cell(row, column) else {
        return CGridCellInspectionResult::MissingCell;
    };

    // SAFETY: The output pointer was checked for null above and the caller
    // guarantees valid writable storage.
    unsafe {
        output.write(CGridCell::from(cell));
    }
    CGridCellInspectionResult::Found
}

/// Assigns a new stable identity and creates an empty grid in permanent Rust
/// storage.
#[unsafe(no_mangle)]
pub extern "C" fn grid_assign_handle() -> VimHandleT {
    let grid_id = allocate_grid_id();
    register_empty_grid(grid_id);
    grid_id.as_native_handle()
}

/// Inserts lines in a permanently stored grid and emits the resulting scroll
/// command after releasing the grid-store borrow.
#[unsafe(no_mangle)]
pub extern "C" fn grid_ins_lines(
    grid_handle: VimHandleT,
    row: libc::c_int,
    line_count: libc::c_int,
    end: libc::c_int,
    col: libc::c_int,
    width: libc::c_int,
) {
    let (target, command) = {
        let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
            return;
        };
        let target = GridOutputTarget::new(grid.id(), grid.size().width);
        let command = grid.ins_lines(row, line_count, end, col, width);
        (target, command)
    };
    if let Some(command) = command {
        target.emit(command);
    }
}
