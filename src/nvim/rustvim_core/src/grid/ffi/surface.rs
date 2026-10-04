//! Final UI adapter shared by complete grid-surface operations.

use super::super::ComposedGridSpace;
use super::super::GridId;
use super::super::GridSize;
use super::super::GridSurfaceOutput;
use crate::defines::Integer;
use crate::ui::output::RawLineCells;
use crate::ui::output::RawLineCommand;
use crate::ui::output::UiCommand;
use crate::ui::output::UiOutput;
use crate::ui_compositor::NvimComposedOutput;

unsafe extern "C" {
    fn ui_uncomposed_call_grid_resize(grid: Integer, width: Integer, height: Integer);
    fn ui_call_grid_destroy(grid: Integer);
}

/// Delivers composed output and common lifecycle events to Neovim.
pub(super) struct NvimGridSurfaceOutput {
    composed: NvimComposedOutput,
}

impl NvimGridSurfaceOutput {
    /// Creates an output adapter for one synchronous surface operation.
    pub(super) const fn new() -> Self {
        Self {
            composed: NvimComposedOutput,
        }
    }
}

impl UiOutput<ComposedGridSpace> for NvimGridSurfaceOutput {
    fn emit(&mut self, command: UiCommand<ComposedGridSpace>) {
        self.composed.emit(command);
    }

    fn emit_raw_line<Cells: RawLineCells>(&mut self, command: RawLineCommand<Cells>) {
        self.composed.emit_raw_line(command);
    }
}

impl GridSurfaceOutput for NvimGridSurfaceOutput {
    fn emit_grid_resize(&mut self, grid_id: GridId, size: GridSize) {
        // SAFETY: Safe Rust validated the identity and dimensions, and the
        // generated callback is synchronous.
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
}
