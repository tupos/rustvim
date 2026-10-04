//! Access to compositor settings and highlights owned by Neovim.

use std::sync::MutexGuard;

use super::super::CompositorDebugHighlights;
use super::super::CompositorRecomposeArea;
use super::super::CompositorRuntime;
use super::super::emit_compositor_debug_line;
use super::super::lock_compositor;
use crate::context::registered_context;
use crate::defines::OptInt;
use crate::globals;
use crate::grid::ComposedGridSpace;
use crate::grid::GridId;
use crate::grid::VimHandleT;
use crate::grid::VimSAttrT;
use crate::grid::lock_grid_store;
use crate::ui::output::UiCommand;
use crate::ui::output::UiOutput;

unsafe extern "C" {
    static mut p_wd: OptInt;
    static mut rdb_flags: libc::c_uint;
    static mut msg_grid_handle: VimHandleT;

    fn syn_check_group(name: *const libc::c_char, length: libc::size_t) -> libc::c_int;
    fn syn_id2attr(highlight_id: libc::c_int) -> libc::c_int;
    fn ui_get_message_separator_attr() -> VimSAttrT;
}

/// Acquires exclusive access to compositor debug-highlight identities.
fn lock_compositor_debug_highlights() -> MutexGuard<'static, CompositorDebugHighlights> {
    registered_context().compositor().lock_debug_highlights()
}

/// Resolves one redraw-debug highlight group through Neovim's syntax API.
fn compositor_debug_highlight(name: &[u8]) -> i32 {
    // SAFETY: The byte slice supplies a readable pointer for exactly `len`
    // bytes, and `syn_check_group` does not require a trailing NUL.
    unsafe { syn_check_group(name.as_ptr().cast(), name.len()) }
}

/// Initializes compositor debug-highlight identities from Neovim.
pub(in crate::ui_compositor) fn initialize_debug_highlights() {
    let highlights = CompositorDebugHighlights {
        normal: compositor_debug_highlight(b"RedrawDebugNormal"),
        clear: compositor_debug_highlight(b"RedrawDebugClear"),
        composed: compositor_debug_highlight(b"RedrawDebugComposed"),
        recomposed: compositor_debug_highlight(b"RedrawDebugRecompose"),
    };
    *lock_compositor_debug_highlights() = highlights;
}

/// Returns the current message-grid identity when one has been assigned.
pub(crate) fn current_message_grid_id() -> Option<GridId> {
    // SAFETY: Rendering events read Neovim globals synchronously on the main
    // thread.
    let grid_handle = unsafe { msg_grid_handle };
    (grid_handle > 0).then_some(GridId::new(grid_handle))
}

/// Captures C-owned settings as validated native Rust values.
pub(crate) fn compositor_runtime_snapshot() -> Option<CompositorRuntime> {
    // SAFETY: Compositor events run on Neovim's main thread. The accessor
    // returns the current dynamic MsgSeparator attribute by value.
    let (screen_rows, screen_columns, message_separator_attribute, flags, write_delay) = unsafe {
        (
            globals::Rows,
            globals::Columns,
            ui_get_message_separator_attr(),
            rdb_flags,
            p_wd,
        )
    };

    CompositorRuntime::try_new(
        screen_rows,
        screen_columns,
        message_separator_attribute,
        flags,
        write_delay,
        *lock_compositor_debug_highlights(),
    )
}

impl CompositorRuntime {
    /// Visualizes one compositor operation when redraw debugging is enabled.
    pub(in crate::ui_compositor) fn visualize(
        self,
        requested_area: &CompositorRecomposeArea,
        highlight_id: i32,
        delay: bool,
        output: &mut impl UiOutput<ComposedGridSpace>,
    ) {
        if !self.debug_enabled || requested_area.columns.start >= requested_area.columns.end {
            return;
        }
        let area = {
            let grid_store = lock_grid_store();
            lock_compositor().clip_recompose_area(&grid_store, requested_area.clone())
        };
        let Ok(area) = area else {
            return;
        };

        // SAFETY: The highlight ID was returned by `syn_check_group`, and this
        // call occurs on Neovim's main thread without holding grid ownership.
        let attribute = unsafe { syn_id2attr(highlight_id) };
        if delay {
            self.delay(area.rows.end - area.rows.start, output);
        }
        for row in area.rows.clone() {
            emit_compositor_debug_line(row, area.columns.clone(), attribute, output)
                .expect("debug line must fit compositor storage");
        }
        if delay {
            self.delay(area.rows.end - area.rows.start, output);
        }
    }

    /// Flushes composed output and applies the configured per-operation delay.
    pub(in crate::ui_compositor) fn delay(
        self,
        lines: i32,
        output: &mut impl UiOutput<ComposedGridSpace>,
    ) {
        let factor = lines.clamp(1, 5) as u64;
        output.emit(UiCommand::Flush);
        output.emit(UiCommand::Delay(factor.wrapping_mul(self.write_delay_ms)));
    }
}
