//! Process-lifetime ownership of RustVim runtime state.

use std::sync::OnceLock;

use crate::grid::GridContext;
use crate::grid::MessageSurface;
use crate::grid::PopupMenuSurface;
use crate::grid::WindowSurface;
use crate::ui::UiContext;
use crate::ui_compositor::CompositorContext;

/// Root of all Rust-owned runtime state.
///
/// Subsystem contexts are added here as their independent globals are
/// migrated.
pub(crate) struct RustVimContext {
    grid: GridContext,
    compositor: CompositorContext,
    ui: UiContext,
}

impl RustVimContext {
    /// Constructs the context before subsystem initialization.
    fn new() -> Self {
        Self {
            grid: GridContext::new(),
            compositor: CompositorContext::new(),
            ui: UiContext::new(),
        }
    }

    /// Returns the grid subsystem context.
    pub(crate) const fn grid(&self) -> &GridContext {
        &self.grid
    }

    /// Returns the compositor subsystem context.
    pub(crate) const fn compositor(&self) -> &CompositorContext {
        &self.compositor
    }

    /// Returns the UI subsystem context.
    pub(crate) const fn ui(&self) -> &UiContext {
        &self.ui
    }

    /// Borrows the complete message rendering surface.
    pub(crate) const fn message_surface(&self) -> MessageSurface<'_> {
        MessageSurface::new(&self.grid, &self.compositor)
    }

    /// Borrows the complete popup-menu rendering surface.
    pub(crate) const fn popup_menu_surface(&self) -> PopupMenuSurface<'_> {
        PopupMenuSurface::new(&self.grid, &self.compositor)
    }

    /// Borrows the complete window rendering surface.
    pub(crate) const fn window_surface(&self) -> WindowSurface<'_> {
        WindowSurface::new(&self.grid, &self.compositor)
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
