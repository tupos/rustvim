//! Process-lifetime ownership of RustVim runtime state.

use std::sync::OnceLock;

use crate::grid::GridContext;
use crate::grid::GridStoreError;
use crate::grid::MessageSurface;
use crate::grid::PopupMenuSurface;
use crate::grid::WindowSurface;
use crate::ui::UiContext;
use crate::ui_compositor::CompositorContext;
use crate::ui_compositor::CompositorError;

/// Failures while coordinating the complete Rust rendering system.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
enum RenderingSystemError {
    /// The permanent grid store could not initialize.
    #[error(transparent)]
    GridStore(#[from] GridStoreError),

    /// The compositor could not initialize or release its layer state.
    #[error(transparent)]
    Compositor(#[from] CompositorError),
}

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

    /// Initializes mutually dependent rendering subsystems in dependency order.
    fn initialize_rendering(&self) -> Result<(), RenderingSystemError> {
        self.grid.initialize()?;
        self.compositor.initialize(&self.grid)?;
        Ok(())
    }

    /// Releases context-owned rendering resources without unregistering it.
    fn release_rendering(&self) -> Result<(), RenderingSystemError> {
        self.grid.release();
        self.ui.release();
        self.compositor.release()?;
        Ok(())
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

/// Initializes the registered Rust rendering system.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_rendering_initialize() {
    registered_context()
        .initialize_rendering()
        .expect("Rust rendering system must initialize exactly once");
}

/// Releases all registered Rust rendering-system resources.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_rendering_free_all_mem() {
    registered_context()
        .release_rendering()
        .expect("Rust rendering system must be initialized before teardown");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::ComposedPosition;
    use crate::grid::DEFAULT_GRID_ID;

    #[test]
    fn rendering_lifecycle_coordinates_grid_and_compositor_state() {
        let context = RustVimContext::new();

        context.initialize_rendering().unwrap();
        assert!(context.grid().lock_store().grid(DEFAULT_GRID_ID).is_ok());
        assert_eq!(
            context.compositor().registered_grid_origin(DEFAULT_GRID_ID),
            Ok(Some(ComposedPosition::new(0, 0)))
        );

        context.release_rendering().unwrap();
        assert!(matches!(
            context.grid().lock_store().grid(DEFAULT_GRID_ID),
            Err(GridStoreError::UnknownGrid(grid_id)) if grid_id == DEFAULT_GRID_ID
        ));
        assert_eq!(
            context.compositor().registered_grid_origin(DEFAULT_GRID_ID),
            Err(CompositorError::InvalidLayerState)
        );
    }
}
