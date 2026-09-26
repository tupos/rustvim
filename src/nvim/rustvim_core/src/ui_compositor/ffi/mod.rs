//! C boundary adapters for the Rust compositor.

mod entrypoints;
pub(in crate::ui_compositor) mod grid_store;
mod nvim_output;
mod runtime;

#[cfg(test)]
pub(in crate::ui_compositor) use entrypoints::ui_comp_find_grid_at_coord;
pub(in crate::ui_compositor) use nvim_output::NvimOutput;
pub(in crate::ui_compositor) use runtime::{
    compositor_runtime_snapshot, initialize_debug_highlights,
};
