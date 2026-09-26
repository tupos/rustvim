//! Transitional ownership of screen grids stored in C structures.

use std::{
    collections::HashMap,
    ptr::NonNull,
    slice,
    sync::{Mutex, MutexGuard},
};

use crate::grid::{CScreenGrid, GridId, ScreenGrid, ScreenGridGuard};

use super::super::{CompositorError, CompositorResult, GridAccess};

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
pub(in crate::ui_compositor) struct CCompositorGridBindings {
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

    /// Returns every registered C grid pointer.
    fn cgrids(&self) -> Vec<*mut CScreenGrid> {
        self.grids
            .values()
            .map(|binding| binding.cgrid.as_ptr())
            .collect()
    }
}

/// One temporary C pointer binding paired with its Rust grid owner.
struct GuardedCompositorLayer {
    cgrid: NonNull<CScreenGrid>,
    grid: ScreenGridGuard,
}

/// Temporary Rust ownership of compositor layers received from C.
///
/// Layer identity and ordering remain in the safe compositor. This boundary
/// store contains only temporary C bindings. Each `ScreenGridGuard` restores
/// its grid allocations when this collection is dropped.
#[must_use = "dropping the layer guards restores ownership to the C grids"]
pub(in crate::ui_compositor) struct LegacyGridStore {
    bindings: HashMap<GridId, GuardedCompositorLayer>,
}

impl LegacyGridStore {
    /// Takes temporary ownership of all grids registered at the FFI boundary.
    pub(in crate::ui_compositor) fn try_take_registered() -> Option<Self> {
        let cgrids = {
            let bindings = lock_compositor_grid_bindings();
            bindings.as_ref()?.cgrids()
        };

        // SAFETY: The boundary state contains distinct pointers established
        // by validated FFI operations. Compositor calls are serialized on
        // Neovim's main thread, and registered C grids remain alive until
        // removal.
        unsafe { Self::try_take(cgrids.as_ptr(), cgrids.len()) }
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
    ) -> Option<(Self, GridId, bool)> {
        let mut grid_store = Self::try_take_registered()?;
        if let Some(grid_id) = grid_store.grid_id_for_cgrid(candidate.as_ptr()) {
            return Some((grid_store, grid_id, true));
        }

        // SAFETY: The caller transfers ownership of the unregistered
        // candidate for the lifetime of the returned guard collection.
        let grid = unsafe { ScreenGridGuard::take(candidate.as_ptr()) };
        let grid_id = grid.id();
        if grid_store.bindings.contains_key(&grid_id) {
            return None;
        }

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

        Some((grid_store, grid_id, false))
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
    ) -> Option<(Self, Vec<GridId>)> {
        // SAFETY: The caller satisfies the documented pointer and grid
        // ownership requirements.
        let (layer_ids, bindings) = unsafe { Self::take_bindings(layer_grids, layer_count) }?;
        Some((Self { bindings }, layer_ids))
    }

    /// Takes temporary ownership of the supplied distinct C grids.
    unsafe fn try_take(layer_grids: *const *mut CScreenGrid, layer_count: usize) -> Option<Self> {
        // SAFETY: The caller satisfies the documented pointer and grid
        // ownership requirements.
        let (_, bindings) = unsafe { Self::take_bindings(layer_grids, layer_count) }?;
        Some(Self { bindings })
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

    /// Returns the native identity belonging to one C grid pointer.
    pub(in crate::ui_compositor) fn grid_id_for_cgrid(
        &self,
        cgrid: *mut CScreenGrid,
    ) -> Option<GridId> {
        let cgrid = NonNull::new(cgrid)?;
        self.bindings
            .values()
            .find_map(|layer| (layer.cgrid == cgrid).then_some(layer.grid.id()))
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

    /// Removes one temporary C binding after its layer is unregistered.
    pub(in crate::ui_compositor) fn remove_binding(
        &mut self,
        grid_id: GridId,
    ) -> CompositorResult<()> {
        self.bindings
            .remove(&grid_id)
            .map(drop)
            .ok_or(CompositorError::InvalidLayerState)
    }

    /// Captures all current temporary C bindings.
    pub(in crate::ui_compositor) fn ffi_bindings(&self) -> Option<CCompositorGridBindings> {
        let c_grids = self
            .bindings
            .iter()
            .map(|(grid_id, layer)| (*grid_id, CCompositorGridBinding { cgrid: layer.cgrid }))
            .collect();
        CCompositorGridBindings::try_new(c_grids)
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

static COMPOSITOR_GRID_BINDINGS: Mutex<Option<CCompositorGridBindings>> = Mutex::new(None);

/// Acquires exclusive access to the transitional compositor C-grid bindings.
pub(in crate::ui_compositor) fn lock_compositor_grid_bindings()
-> MutexGuard<'static, Option<CCompositorGridBindings>> {
    COMPOSITOR_GRID_BINDINGS
        .try_lock()
        .expect("compositor grid bindings are already borrowed")
}

/// Replaces the complete transitional binding collection.
pub(in crate::ui_compositor) fn replace_compositor_grid_bindings(
    replacement: CCompositorGridBindings,
) -> CompositorResult<()> {
    let mut current_bindings = lock_compositor_grid_bindings();
    if current_bindings.is_none() {
        return Err(CompositorError::InvalidLayerState);
    }
    *current_bindings = Some(replacement);
    Ok(())
}

/// Initializes compositor layer ownership and its default-grid binding.
///
/// # Safety
///
/// `default_grid` must point to a valid, exclusively accessible C grid whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
pub(in crate::ui_compositor) unsafe fn initialize_grid_bindings(
    default_grid: *mut CScreenGrid,
) -> CompositorResult<GridId> {
    let default_cgrid = NonNull::new(default_grid).ok_or(CompositorError::InvalidLayerState)?;

    let default_grid_id = {
        // SAFETY: The caller transfers exclusive ownership for this scope.
        let default_grid = unsafe { ScreenGridGuard::take(default_grid) };
        default_grid.id()
    };
    let c_grids = CCompositorGridBindings::try_new(HashMap::from([(
        default_grid_id,
        CCompositorGridBinding {
            cgrid: default_cgrid,
        },
    )]))
    .ok_or(CompositorError::InvalidLayerState)?;
    let mut current_bindings = lock_compositor_grid_bindings();
    if current_bindings.is_some() {
        return Err(CompositorError::InvalidLayerState);
    }
    *current_bindings = Some(c_grids);
    Ok(default_grid_id)
}

/// Clears temporary C-grid bindings during teardown.
pub(in crate::ui_compositor) fn clear_grid_bindings() -> CompositorResult<()> {
    lock_compositor_grid_bindings()
        .take()
        .map(drop)
        .ok_or(CompositorError::InvalidLayerState)
}
