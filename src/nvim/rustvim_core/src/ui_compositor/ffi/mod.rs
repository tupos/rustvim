//! C boundary adapters for the Rust compositor.

mod entrypoints;
mod nvim_output;
mod runtime;

#[cfg(test)]
pub(in crate::ui_compositor) use entrypoints::update_registered_layer_policy;
pub(crate) use nvim_output::NvimComposedOutput;
pub(crate) use runtime::compositor_runtime_snapshot;
pub(crate) use runtime::current_message_grid_id;
pub(in crate::ui_compositor) use runtime::initialize_debug_highlights;
