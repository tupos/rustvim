//! Process-lifetime ownership of RustVim runtime state.

use std::sync::OnceLock;

use crate::grid::GridContext;

/// Root of all Rust-owned runtime state.
///
/// Subsystem contexts are added here as their independent globals are
/// migrated.
pub(crate) struct RustVimContext {
    grid: GridContext,
}

impl RustVimContext {
    /// Constructs the context before subsystem initialization.
    fn new() -> Self {
        Self {
            grid: GridContext::new(),
        }
    }

    /// Returns the grid subsystem context.
    pub(crate) const fn grid(&self) -> &GridContext {
        &self.grid
    }
}

static RUSTVIM_CONTEXT: OnceLock<RustVimContext> = OnceLock::new();

/// Returns the registered process context.
#[cfg(not(test))]
pub(crate) fn registered_context() -> &'static RustVimContext {
    RUSTVIM_CONTEXT
        .get()
        .expect("RustVim context must be registered before use")
}

/// Returns the test process context.
///
/// Rust unit-test binaries do not execute the C `main()` registration path.
#[cfg(test)]
pub(crate) fn registered_context() -> &'static RustVimContext {
    RUSTVIM_CONTEXT.get_or_init(RustVimContext::new)
}

#[unsafe(no_mangle)]
pub extern "C" fn rustvim_context_register() -> bool {
    RUSTVIM_CONTEXT.set(RustVimContext::new()).is_ok()
}
