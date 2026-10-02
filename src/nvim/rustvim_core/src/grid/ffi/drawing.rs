//! C boundary adapters for grid clearing and border rendering.

use super::super::{
    Border, BorderCanvas, CGridBorderConfig, GridId, GridOutputTarget, GridPosition,
    GridViewDescriptor, Highlight, LineBufferGuard, LineGrid, StoredGridGuard, VimHandleT,
    VimSAttrT, lock_glyph_mapping, lock_message_grid_state, with_line_grid,
};

/// Clears a rectangular region through a persistent view descriptor.
fn clear_grid_view(
    descriptor: GridViewDescriptor,
    start_row: i32,
    end_row: i32,
    start_col: i32,
    mut end_col: i32,
    attr: VimSAttrT,
) {
    let start_row = start_row
        .checked_add(descriptor.offset.row)
        .expect("grid clear start-row translation overflowed");
    let end_row = end_row
        .checked_add(descriptor.offset.row)
        .expect("grid clear end-row translation overflowed");

    let output = {
        let mut line_buffer = LineBufferGuard::acquire();
        with_line_grid(|line_grid| {
            let mut output = Vec::new();
            for row in start_row..end_row {
                line_grid.begin(
                    descriptor.target,
                    row,
                    descriptor.offset.column,
                    &mut line_buffer,
                );
                end_col = std::cmp::min(end_col, line_grid.max_col());
                if line_grid.row_reaches_grid_end() || start_col >= end_col {
                    line_grid.cancel();
                    break;
                }
                line_grid.set_clear_range(start_col, end_col, attr, 0);
                if let Some(update) = line_grid.flush(&mut line_buffer) {
                    output.push(update);
                }
            }
            output
        })
    };
    for (target, commands) in output {
        target.emit_lines(commands);
    }
}

/// Clears a rectangular region of an identified grid.
#[unsafe(no_mangle)]
pub extern "C" fn grid_clear_region(
    grid_handle: VimHandleT,
    start_row: libc::c_int,
    end_row: libc::c_int,
    start_col: libc::c_int,
    end_col: libc::c_int,
    attr: libc::c_int,
) {
    let descriptor = GridViewDescriptor::new(GridId::new(grid_handle), GridPosition::new(0, 0));
    clear_grid_view(descriptor, start_row, end_row, start_col, end_col, attr);
}

/// Clears a rectangular region through the adjusted message view.
#[unsafe(no_mangle)]
pub extern "C" fn grid_message_view_clear(
    start_row: libc::c_int,
    end_row: libc::c_int,
    start_col: libc::c_int,
    end_col: libc::c_int,
    attr: libc::c_int,
) {
    let descriptor = lock_message_grid_state().view();
    clear_grid_view(descriptor, start_row, end_row, start_col, end_col, attr);
}

/// Draws a floating-window border from a stable subset of `WinConfig`.
///
/// Missing grid identities are ignored.
///
/// # Safety
///
/// `config` must point to a valid `CGridBorderConfig` whose arrays and enabled
/// virtual-text vectors satisfy the requirements of `Border::from_c`. A
/// non-null `adjustments` must point to four initialized integers. The global
/// line buffer must be initialized, and this function must run on Neovim's
/// main thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_draw_border(
    grid_handle: VimHandleT,
    config: *const CGridBorderConfig,
    adjustments: *const libc::c_int,
    winbl: libc::c_int,
    win_hl_attr_normal: libc::c_int,
) {
    if config.is_null() {
        return;
    }

    let border = {
        let mut glyph_map = lock_glyph_mapping();
        // SAFETY: The pointer and all nested border data satisfy the contract
        // documented on this function.
        unsafe {
            Border::from_c(
                &*config,
                adjustments,
                winbl,
                Highlight(win_hl_attr_normal),
                &mut glyph_map,
            )
        }
    };
    let Some(border) = border else {
        return;
    };

    let (target, commands) = {
        let Ok(mut rust_grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
            return;
        };
        let target = GridOutputTarget::new(rust_grid.id(), rust_grid.size().width);
        let mut line_buffer = LineBufferGuard::acquire();
        let mut canvas = BorderCanvas {
            line_grid: LineGrid::new(&mut rust_grid),
            line_buffer: &mut line_buffer,
            output: Vec::new(),
        };
        border.draw(&mut canvas);
        (target, canvas.output)
    };
    target.emit_lines(commands);
}
