//! C boundary adapters for the Rust compositor.

mod nvim_output;

pub(in crate::ui_compositor) use nvim_output::NvimOutput;
