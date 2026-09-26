//! C boundary adapters for the Rust compositor.

pub(in crate::ui_compositor) mod grid_store;
mod nvim_output;
mod runtime;

pub(in crate::ui_compositor) use nvim_output::NvimOutput;
pub(in crate::ui_compositor) use runtime::{
    compositor_runtime_snapshot, initialize_debug_highlights,
};
