//! Transitional ownership of screen grids stored in C structures.

use std::{
    collections::HashMap,
    ptr::NonNull,
    slice,
    sync::{Mutex, MutexGuard},
};

use crate::grid::{CScreenGrid, GridId, ScreenGrid, ScreenGridGuard};

use super::super::{
    CompositorError, CompositorGridPlacementTarget, CompositorLayerRegistry, CompositorResult,
    GridAccess,
};

/// One temporary pointer connecting a native grid identity to C storage.
///
/// This boundary-only representation will disappear when grids become
/// permanently owned by Rust.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CCompositorGridBinding {
    cgrid: NonNull<CScreenGrid>,
}

// SAFETY: Neovim invokes compositor operations synchronously on its main
// thread. Moving this wrapper does not access the referenced C grid, and the
// pointer is dereferenced only while an FFI operation has exclusive ownership
// of that grid.
unsafe impl Send for CCompositorGridBinding {}

/// Transitional C-grid pointers keyed by safe native grid identities.
#[derive(Clone, Debug)]
struct CCompositorGridBindings {
    grids: HashMap<GridId, CCompositorGridBinding>,
}

impl CCompositorGridBindings {
    /// Creates a nonempty registry containing unique C grid pointers.
    fn try_new(grids: HashMap<GridId, CCompositorGridBinding>) -> Option<Self> {
        if grids.is_empty() {
            return None;
        }

        let mut pointers = Vec::with_capacity(grids.len());
        for binding in grids.values() {
            if pointers.contains(&binding.cgrid) {
                return None;
            }
            pointers.push(binding.cgrid);
        }

        Some(Self { grids })
    }

    /// Reports whether every ordered grid has exactly one C binding.
    fn matches_registry(&self, registry: &CompositorLayerRegistry) -> bool {
        self.grids.len() == registry.len()
            && registry
                .iter()
                .all(|grid_id| self.grids.contains_key(grid_id))
    }

    /// Returns C grid pointers in native compositor order.
    fn ordered_cgrids(&self, registry: &CompositorLayerRegistry) -> Option<Vec<*mut CScreenGrid>> {
        registry
            .iter()
            .map(|grid_id| {
                self.grids
                    .get(grid_id)
                    .map(|binding| binding.cgrid.as_ptr())
            })
            .collect()
    }
}

/// Process-global compositor state at the transitional C boundary.
///
/// Safe compositor logic receives only a clone of `registry`; C pointers stay
/// confined to ownership adapters.
pub(in crate::ui_compositor) struct CompositorLayerFfiState {
    pub(in crate::ui_compositor) registry: CompositorLayerRegistry,
    c_grids: CCompositorGridBindings,
}

impl CompositorLayerFfiState {
    /// Combines matching safe layer order and C-grid bindings.
    fn try_new(
        registry: CompositorLayerRegistry,
        c_grids: CCompositorGridBindings,
    ) -> Option<Self> {
        c_grids
            .matches_registry(&registry)
            .then_some(Self { registry, c_grids })
    }
}

/// One temporary C pointer binding paired with its Rust grid owner.
struct GuardedCompositorLayer {
    cgrid: NonNull<CScreenGrid>,
    grid: ScreenGridGuard,
}

/// Temporary Rust ownership of compositor layers received from C.
///
/// The native registry owns layer identity and ordering. C pointers exist only
/// in the transitional binding map. Each `ScreenGridGuard` restores its grid
/// allocations when this collection is dropped.
#[must_use = "dropping the layer guards restores ownership to the C grids"]
pub(in crate::ui_compositor) struct LegacyGridStore {
    pub(in crate::ui_compositor) registry: CompositorLayerRegistry,
    bindings: HashMap<GridId, GuardedCompositorLayer>,
}

impl LegacyGridStore {
    /// Takes temporary ownership of all grids registered at the FFI boundary.
    pub(in crate::ui_compositor) fn try_take_registered() -> Option<Self> {
        let (registry, cgrids) = {
            let state = lock_compositor_layer_state();
            let state = state.as_ref()?;
            let registry = state.registry.clone();
            let cgrids = state.c_grids.ordered_cgrids(&registry)?;
            (registry, cgrids)
        };

        // SAFETY: The boundary state contains distinct pointers established
        // by validated FFI operations. Compositor calls are serialized on
        // Neovim's main thread, and registered C grids remain alive until
        // removal.
        unsafe { Self::try_take_for_registry(cgrids.as_ptr(), cgrids.len(), registry) }
    }

    /// Binds C grids to one supplied native registry.
    ///
    /// # Safety
    ///
    /// If `layer_count` is nonzero, `layer_grids` must point to that many
    /// readable grid pointers. Every non-null grid must be valid, distinct,
    /// exclusively accessible, and satisfy the ownership and layout
    /// requirements of `ScreenGrid`.
    pub(in crate::ui_compositor) unsafe fn try_take_for_registry(
        layer_grids: *const *mut CScreenGrid,
        layer_count: usize,
        registry: CompositorLayerRegistry,
    ) -> Option<Self> {
        // SAFETY: The caller satisfies the documented pointer and grid
        // ownership requirements.
        let (_, bindings) = unsafe { Self::take_bindings(layer_grids, layer_count) }?;
        Self::try_from_registry(registry, bindings)
    }

    /// Takes registered grids together with one placement candidate.
    ///
    /// # Safety
    ///
    /// If `candidate` is not registered, it must identify a valid,
    /// exclusively accessible C grid whose buffers satisfy the ownership and
    /// layout requirements of `ScreenGrid`.
    pub(in crate::ui_compositor) unsafe fn try_take_registered_with_candidate(
        candidate: NonNull<CScreenGrid>,
    ) -> Option<(Self, CompositorGridPlacementTarget)> {
        let mut grid_store = Self::try_take_registered()?;
        if let Some(layer_index) = grid_store.index_of(candidate.as_ptr()) {
            let grid_id = grid_store.registry.layer_id(layer_index)?;
            return Some((
                grid_store,
                CompositorGridPlacementTarget {
                    grid_id,
                    layer_index,
                    was_registered: true,
                },
            ));
        }

        // SAFETY: The caller transfers ownership of the unregistered
        // candidate for the lifetime of the returned guard collection.
        let grid = unsafe { ScreenGridGuard::take(candidate.as_ptr()) };
        let grid_id = grid.id();
        if grid_store.bindings.contains_key(&grid_id) {
            return None;
        }

        let layer_index = grid_store.registry.len();
        grid_store.registry.try_append(grid_id).ok()?;
        grid_store
            .bindings
            .insert(
                grid_id,
                GuardedCompositorLayer {
                    cgrid: candidate,
                    grid,
                },
            )
            .is_none()
            .then_some(())?;

        Some((
            grid_store,
            CompositorGridPlacementTarget {
                grid_id,
                layer_index,
                was_registered: false,
            },
        ))
    }

    /// Creates guards when the binding order itself is under test.
    ///
    /// # Safety
    ///
    /// If `layer_count` is nonzero, `layer_grids` must point to that many
    /// readable grid pointers. Every non-null grid must be valid, distinct,
    /// exclusively accessible, and satisfy the ownership and layout
    /// requirements of `ScreenGrid`.
    #[cfg(test)]
    pub(in crate::ui_compositor) unsafe fn try_take_in_binding_order(
        layer_grids: *const *mut CScreenGrid,
        layer_count: usize,
    ) -> Option<Self> {
        // SAFETY: The caller satisfies the documented pointer and grid
        // ownership requirements.
        let (layer_ids, bindings) = unsafe { Self::take_bindings(layer_grids, layer_count) }?;
        let registry = CompositorLayerRegistry::try_new(layer_ids)?;
        Self::try_from_registry(registry, bindings)
    }

    /// Creates ownership guards from a safe registry and validated bindings.
    fn try_from_registry(
        registry: CompositorLayerRegistry,
        bindings: HashMap<GridId, GuardedCompositorLayer>,
    ) -> Option<Self> {
        if registry.len() != bindings.len()
            || registry
                .iter()
                .any(|grid_id| !bindings.contains_key(grid_id))
        {
            return None;
        }

        Some(Self { registry, bindings })
    }

    /// Validates C bindings and takes temporary ownership of their grids.
    ///
    /// # Safety
    ///
    /// If `layer_count` is nonzero, `layer_grids` must point to that many
    /// readable grid pointers. Every non-null grid must be valid, distinct,
    /// exclusively accessible, and satisfy the ownership and layout
    /// requirements of `ScreenGrid`.
    unsafe fn take_bindings(
        layer_grids: *const *mut CScreenGrid,
        layer_count: usize,
    ) -> Option<(Vec<GridId>, HashMap<GridId, GuardedCompositorLayer>)> {
        if layer_grids.is_null() || layer_count == 0 {
            return None;
        }

        // SAFETY: The caller guarantees that the pointer array is readable for
        // `layer_count` elements for the duration of this synchronous call.
        let cgrids = unsafe { slice::from_raw_parts(layer_grids, layer_count) }
            .iter()
            .copied()
            .map(NonNull::new)
            .collect::<Option<Vec<_>>>()?;
        if cgrids
            .iter()
            .enumerate()
            .any(|(index, grid)| cgrids[..index].contains(grid))
        {
            return None;
        }

        let guarded_layers = cgrids
            .into_iter()
            .map(|cgrid| {
                // SAFETY: Null and duplicate pointers were rejected, and the
                // caller transfers ownership for this operation.
                let grid = unsafe { ScreenGridGuard::take(cgrid.as_ptr()) };
                GuardedCompositorLayer { cgrid, grid }
            })
            .collect::<Vec<_>>();
        let layer_ids = guarded_layers
            .iter()
            .map(|layer| layer.grid.id())
            .collect::<Vec<_>>();
        if layer_ids
            .iter()
            .enumerate()
            .any(|(index, grid_id)| layer_ids[..index].contains(grid_id))
        {
            return None;
        }
        let bindings = guarded_layers
            .into_iter()
            .map(|layer| (layer.grid.id(), layer))
            .collect();

        Some((layer_ids, bindings))
    }

    /// Returns the stack index belonging to one C grid identity.
    pub(in crate::ui_compositor) fn index_of(&self, cgrid: *mut CScreenGrid) -> Option<usize> {
        let cgrid = NonNull::new(cgrid)?;
        let grid_id = self
            .bindings
            .values()
            .find_map(|layer| (layer.cgrid == cgrid).then_some(layer.grid.id()))?;
        self.registry.index_of(grid_id)
    }

    /// Returns the transitional C pointer bound to one registered identity.
    pub(in crate::ui_compositor) fn cgrid_by_id(
        &self,
        grid_id: GridId,
    ) -> Option<*mut CScreenGrid> {
        self.bindings
            .get(&grid_id)
            .map(|layer| layer.cgrid.as_ptr())
    }

    /// Captures all current identities and their temporary C bindings.
    pub(in crate::ui_compositor) fn ffi_state(&self) -> Option<CompositorLayerFfiState> {
        let registry = self.registry.clone();
        let c_grids = registry
            .iter()
            .map(|grid_id| {
                let layer = self.bindings.get(grid_id)?;
                Some((*grid_id, CCompositorGridBinding { cgrid: layer.cgrid }))
            })
            .collect::<Option<HashMap<_, _>>>()?;
        let c_grids = CCompositorGridBindings::try_new(c_grids)?;

        CompositorLayerFfiState::try_new(registry, c_grids)
    }
}

impl GridAccess for LegacyGridStore {
    fn grid(&self, grid_id: GridId) -> Option<&ScreenGrid> {
        self.bindings.get(&grid_id).map(|layer| &*layer.grid)
    }

    fn grid_mut(&mut self, grid_id: GridId) -> Option<&mut ScreenGrid> {
        self.bindings
            .get_mut(&grid_id)
            .map(|layer| &mut *layer.grid)
    }
}

static COMPOSITOR_LAYER_STATE: Mutex<Option<CompositorLayerFfiState>> = Mutex::new(None);

/// Acquires exclusive access to the transitional compositor boundary state.
pub(in crate::ui_compositor) fn lock_compositor_layer_state()
-> MutexGuard<'static, Option<CompositorLayerFfiState>> {
    COMPOSITOR_LAYER_STATE
        .try_lock()
        .expect("compositor layer state is already borrowed")
}

/// Returns an owned snapshot without retaining the registry lock.
pub(in crate::ui_compositor) fn compositor_layer_registry() -> Option<CompositorLayerRegistry> {
    lock_compositor_layer_state()
        .as_ref()
        .map(|state| state.registry.clone())
}

/// Replaces the current registry if it still matches the supplied snapshot.
pub(in crate::ui_compositor) fn replace_compositor_layer_registry(
    expected: &CompositorLayerRegistry,
    replacement: CompositorLayerRegistry,
) -> CompositorResult<()> {
    let mut current_state = lock_compositor_layer_state();
    let state = current_state
        .as_mut()
        .ok_or(CompositorError::InvalidLayerState)?;
    if &state.registry != expected || !state.c_grids.matches_registry(&replacement) {
        return Err(CompositorError::InvalidLayerState);
    }

    state.registry = replacement;
    Ok(())
}

/// Replaces the complete boundary state if its registry snapshot still
/// matches.
pub(in crate::ui_compositor) fn replace_compositor_layer_state(
    expected: &CompositorLayerRegistry,
    replacement: CompositorLayerFfiState,
) -> CompositorResult<()> {
    let mut current_state = lock_compositor_layer_state();
    let current_registry = &current_state
        .as_ref()
        .ok_or(CompositorError::InvalidLayerState)?
        .registry;
    if current_registry != expected {
        return Err(CompositorError::InvalidLayerState);
    }

    *current_state = Some(replacement);
    Ok(())
}

/// Initializes compositor layer ownership and its default-grid binding.
///
/// # Safety
///
/// `default_grid` must point to a valid, exclusively accessible C grid whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
pub(in crate::ui_compositor) unsafe fn initialize_layer_registry(
    default_grid: *mut CScreenGrid,
) -> CompositorResult<()> {
    let default_cgrid = NonNull::new(default_grid).ok_or(CompositorError::InvalidLayerState)?;

    let default_grid_id = {
        // SAFETY: The caller transfers exclusive ownership for this scope.
        let default_grid = unsafe { ScreenGridGuard::take(default_grid) };
        default_grid.id()
    };
    let registry = CompositorLayerRegistry::try_new(vec![default_grid_id])
        .ok_or(CompositorError::InvalidLayerState)?;
    let c_grids = CCompositorGridBindings::try_new(HashMap::from([(
        default_grid_id,
        CCompositorGridBinding {
            cgrid: default_cgrid,
        },
    )]))
    .ok_or(CompositorError::InvalidLayerState)?;
    let state = CompositorLayerFfiState::try_new(registry, c_grids)
        .ok_or(CompositorError::InvalidLayerState)?;

    let mut current_state = lock_compositor_layer_state();
    if current_state.is_some() {
        return Err(CompositorError::InvalidLayerState);
    }
    *current_state = Some(state);
    Ok(())
}

/// Clears compositor layer order and temporary C bindings during teardown.
pub(in crate::ui_compositor) fn clear_layer_registry() -> CompositorResult<()> {
    lock_compositor_layer_state()
        .take()
        .map(drop)
        .ok_or(CompositorError::InvalidLayerState)
}
