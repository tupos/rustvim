//! Structured Rust diagnostics routed through Neovim's existing logger.

pub mod buffer;
mod ffi;
mod layer;
pub mod logger;
pub mod writer;

pub use tracing::Level;
pub use tracing::debug;
pub use tracing::error;
pub use tracing::event;
pub use tracing::info;
pub use tracing::span;
pub use tracing::trace;
pub use tracing::warn;

/// Installs the process-wide subscriber backed by Neovim's logger.
pub fn initialize() -> Result<(), tracing::subscriber::SetGlobalDefaultError> {
    layer::install()
}

/// Installs Rust tracing after Neovim's native logger is initialized.
///
/// Returns `false` if another process-wide subscriber was already installed.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_tracing_initialize() -> bool {
    initialize().is_ok()
}
