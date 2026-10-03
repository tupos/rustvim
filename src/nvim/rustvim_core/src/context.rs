//! Process-lifetime ownership of RustVim runtime state.

use std::sync::OnceLock;

/// Root of all Rust-owned runtime state.
///
/// Subsystem contexts are added here as their independent globals are
/// migrated.
pub(crate) struct RustVimContext;

impl RustVimContext {
    /// Constructs an empty context before subsystem initialization.
    const fn new() -> Self {
        Self
    }
}

static RUSTVIM_CONTEXT: OnceLock<RustVimContext> = OnceLock::new();

/// Returns the registered process context.
pub(crate) fn registered_context() -> &'static RustVimContext {
    RUSTVIM_CONTEXT
        .get()
        .expect("RustVim context must be registered before use")
}

#[unsafe(no_mangle)]
pub extern "C" fn rustvim_context_register() -> bool {
    RUSTVIM_CONTEXT.set(RustVimContext::new()).is_ok()
}
