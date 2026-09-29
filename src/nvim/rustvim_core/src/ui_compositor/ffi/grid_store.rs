//! Transitional ownership of screen grids stored in C structures.

use std::{
    collections::HashMap,
    ptr::NonNull,
    slice,
    sync::{Mutex, MutexGuard},
};

use crate::grid::{
    CScreenGrid, GridId, GridStore, ScreenGrid, ScreenGridGuard, lock_grid_store, permanent_grid_id,
};

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

/// One C pointer binding and any required legacy ownership guard.
struct GuardedCompositorLayer {
    cgrid: NonNull<CScreenGrid>,
    legacy_grid: Option<ScreenGridGuard>,
}

/// Complete compositor access to permanent and transitional screen grids.
///
/// Permanently owned grids share one store borrow. Grids that have not yet
/// migrated retain one temporary legacy guard each.
#[must_use = "the access guard owns every compositor grid borrow"]
pub(in crate::ui_compositor) struct CompositorGridAccess {
    stored_grids: Option<MutexGuard<'static, GridStore>>,
    bindings: HashMap<GridId, GuardedCompositorLayer>,
}

impl CompositorGridAccess {
    /// Creates access after validating every permanent binding.
    fn try_from_bindings(bindings: HashMap<GridId, GuardedCompositorLayer>) -> Option<Self> {
        let stored_grids = bindings
            .values()
            .any(|binding| binding.legacy_grid.is_none())
            .then(lock_grid_store);
        if let Some(store) = &stored_grids
            && bindings.iter().any(|(grid_id, binding)| {
                binding.legacy_grid.is_none() && store.grid(*grid_id).is_err()
            })
        {
            return None;
        }
        Some(Self {
            stored_grids,
            bindings,
        })
    }

    /// Borrows all grids registered at the compositor FFI boundary.
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

        let (grid_id, legacy_grid) = if let Some(grid_id) = permanent_grid_id(candidate) {
            let stored_grids = grid_store.stored_grids.get_or_insert_with(lock_grid_store);
            stored_grids.grid(grid_id).ok()?;
            (grid_id, None)
        } else {
            // SAFETY: The caller transfers ownership of the unregistered
            // candidate for the lifetime of the returned access guard.
            let grid = unsafe { ScreenGridGuard::take(candidate.as_ptr()) };
            let grid_id = grid.id();
            (grid_id, Some(grid))
        };
        if grid_store.bindings.contains_key(&grid_id) {
            return None;
        }

        grid_store
            .bindings
            .insert(
                grid_id,
                GuardedCompositorLayer {
                    cgrid: candidate,
                    legacy_grid,
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
    /// readable grid pointers. Every pointer must be distinct and identify
    /// either a permanently stored grid or a valid, exclusively accessible
    /// legacy grid satisfying the ownership and layout requirements of
    /// `ScreenGrid`.
    #[cfg(test)]
    pub(in crate::ui_compositor) unsafe fn try_take_in_binding_order(
        layer_grids: *const *mut CScreenGrid,
        layer_count: usize,
    ) -> Option<(Self, Vec<GridId>)> {
        // SAFETY: The caller satisfies the documented pointer and grid
        // ownership requirements.
        let (layer_ids, bindings) = unsafe { Self::take_bindings(layer_grids, layer_count) }?;
        Some((Self::try_from_bindings(bindings)?, layer_ids))
    }

    /// Borrows the supplied distinct permanent and legacy grids.
    unsafe fn try_take(layer_grids: *const *mut CScreenGrid, layer_count: usize) -> Option<Self> {
        // SAFETY: The caller satisfies the documented pointer and grid
        // ownership requirements.
        let (_, bindings) = unsafe { Self::take_bindings(layer_grids, layer_count) }?;
        Self::try_from_bindings(bindings)
    }

    /// Validates C bindings and takes ownership of any legacy grids.
    ///
    /// # Safety
    ///
    /// If `layer_count` is nonzero, `layer_grids` must point to that many
    /// readable grid pointers. Every pointer must be distinct and identify
    /// either a permanently stored grid or a valid, exclusively accessible
    /// legacy grid satisfying the ownership and layout requirements of
    /// `ScreenGrid`.
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
                let (grid_id, legacy_grid) = if let Some(grid_id) = permanent_grid_id(cgrid) {
                    (grid_id, None)
                } else {
                    // SAFETY: Null and duplicate pointers were rejected, and
                    // the caller transfers legacy ownership for this operation.
                    let grid = unsafe { ScreenGridGuard::take(cgrid.as_ptr()) };
                    (grid.id(), Some(grid))
                };
                (grid_id, GuardedCompositorLayer { cgrid, legacy_grid })
            })
            .collect::<Vec<_>>();
        let layer_ids = guarded_layers
            .iter()
            .map(|(grid_id, _)| *grid_id)
            .collect::<Vec<_>>();
        if layer_ids
            .iter()
            .enumerate()
            .any(|(index, grid_id)| layer_ids[..index].contains(grid_id))
        {
            return None;
        }
        let bindings = guarded_layers.into_iter().collect();

        Some((layer_ids, bindings))
    }

    /// Returns the native identity belonging to one C grid pointer.
    pub(in crate::ui_compositor) fn grid_id_for_cgrid(
        &self,
        cgrid: *mut CScreenGrid,
    ) -> Option<GridId> {
        let cgrid = NonNull::new(cgrid)?;
        self.bindings
            .iter()
            .find_map(|(grid_id, layer)| (layer.cgrid == cgrid).then_some(*grid_id))
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

impl GridAccess for CompositorGridAccess {
    fn grid(&self, grid_id: GridId) -> Option<&ScreenGrid> {
        self.stored_grids
            .as_ref()
            .and_then(|store| store.grid(grid_id).ok())
            .or_else(|| self.bindings.get(&grid_id)?.legacy_grid.as_deref())
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

/// Moves the default grid into permanent storage and initializes its binding.
///
/// # Safety
///
/// `default_grid` must point to a valid, exclusively accessible C grid whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
pub(in crate::ui_compositor) unsafe fn initialize_grid_bindings(
    default_grid: *mut CScreenGrid,
) -> CompositorResult<GridId> {
    let default_cgrid = NonNull::new(default_grid).ok_or(CompositorError::InvalidLayerState)?;

    let default_grid_id = unsafe {
        // SAFETY: The caller transfers exclusive ownership for this scope.
        ScreenGridGuard::take(default_grid)
    }
    .store_permanently()
    .map_err(|_| CompositorError::InvalidLayerState)?;
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
