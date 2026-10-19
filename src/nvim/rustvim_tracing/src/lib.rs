//! Structured tracing and native diagnostics sharing the safe Rust logger.

pub mod buffer;
pub mod ffi;
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

/// Installs the process-wide tracing subscriber, transferring logger ownership.
pub fn initialize(
    logger: logger::Logger,
) -> Result<(), tracing::subscriber::SetGlobalDefaultError> {
    layer::install(logger)
}
