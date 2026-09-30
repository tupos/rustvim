//! C boundary adapters for the Rust compositor.

mod entrypoints;
mod nvim_output;
mod runtime;

#[cfg(test)]
pub(in crate::ui_compositor) use entrypoints::{
    place_grid_in_registered_layers, remove_registered_layer, update_registered_layer_policy,
};
pub(in crate::ui_compositor) use nvim_output::NvimOutput;
pub(in crate::ui_compositor) use runtime::{
    compositor_runtime_snapshot, initialize_debug_highlights,
};
