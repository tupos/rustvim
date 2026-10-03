//! Output shared by complete grid-surface lifecycle operations.

#![forbid(unsafe_code)]

use crate::ui::output::UiOutput;

use super::{ComposedGridSpace, GridId, GridSize};

/// Receives common UI effects produced by grid-surface lifecycles.
pub(crate) trait GridSurfaceOutput: UiOutput<ComposedGridSpace> {
    /// Publishes allocated grid dimensions.
    fn emit_grid_resize(&mut self, grid_id: GridId, size: GridSize);

    /// Publishes final destruction of grid storage.
    fn emit_grid_destroy(&mut self, grid_id: GridId);
}
