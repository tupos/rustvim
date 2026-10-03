#![forbid(unsafe_code)]

use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    ops::{Deref, DerefMut},
    sync::MutexGuard,
};

use crate::context::registered_context;

use super::{GridId, GridView, GridViewDescriptor, ScreenGrid};

/// Failures produced while managing permanently owned screen grids.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum GridStoreError {
    /// Grid zero is reserved as the absence of a grid identity.
    #[error("grid identity {0} is invalid")]
    InvalidIdentity(GridId),

    /// A live grid already owns the requested identity.
    #[error("grid identity {0} is already registered")]
    AlreadyRegistered(GridId),

    /// The requested grid is not registered.
    #[error("grid identity {0} is not registered")]
    UnknownGrid(GridId),

    /// The requested window grid has no registered view.
    #[error("window grid identity {0} has no registered view")]
    MissingWindowView(GridId),

    /// A removed identity cannot be reused by another grid.
    #[error("grid identity {0} has been retired")]
    RetiredIdentity(GridId),
}

/// Result of an operation on the permanent screen-grid registry.
pub(crate) type GridStoreResult<T> = Result<T, GridStoreError>;

/// Permanent Rust ownership of screen grids indexed by stable identities.
///
/// Removed identities are retained as tombstones so a stale C identity can
/// never resolve to a newly registered grid.
#[derive(Default)]
pub(crate) struct GridStore {
    grids: HashMap<GridId, ScreenGrid>,
    views: HashMap<GridId, GridViewDescriptor>,
    retired: HashSet<GridId>,
}

impl GridStore {
    /// Creates an empty grid store.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Creates and registers an empty grid under `grid_id`.
    pub(crate) fn register_empty(&mut self, grid_id: GridId) -> GridStoreResult<GridId> {
        self.register(ScreenGrid::new_empty(grid_id))
    }

    /// Registers one grid under its existing stable identity.
    pub(crate) fn register(&mut self, grid: ScreenGrid) -> GridStoreResult<GridId> {
        let grid_id = grid.id();
        self.validate_registration(grid_id)?;

        match self.grids.entry(grid_id) {
            Entry::Vacant(entry) => {
                entry.insert(grid);
                Ok(grid_id)
            }
            Entry::Occupied(_) => Err(GridStoreError::AlreadyRegistered(grid_id)),
        }
    }

    /// Validates an identity before its grid is moved into this store.
    pub(super) fn validate_registration(&self, grid_id: GridId) -> GridStoreResult<()> {
        if grid_id.as_native_handle() <= 0 {
            return Err(GridStoreError::InvalidIdentity(grid_id));
        }
        if self.retired.contains(&grid_id) {
            return Err(GridStoreError::RetiredIdentity(grid_id));
        }
        if self.grids.contains_key(&grid_id) {
            return Err(GridStoreError::AlreadyRegistered(grid_id));
        }
        Ok(())
    }

    /// Borrows one registered grid.
    pub(crate) fn grid(&self, grid_id: GridId) -> GridStoreResult<&ScreenGrid> {
        self.grids
            .get(&grid_id)
            .ok_or(GridStoreError::UnknownGrid(grid_id))
    }

    /// Resolves a descriptor into a view borrowing its registered target.
    pub(super) fn view(&self, descriptor: GridViewDescriptor) -> GridStoreResult<GridView<'_>> {
        Ok(GridView::new(
            self.grid(descriptor.target)?,
            descriptor.offset,
        ))
    }

    /// Registers or replaces the view owned by one window grid.
    pub(super) fn set_window_view(
        &mut self,
        owner: GridId,
        descriptor: GridViewDescriptor,
    ) -> GridStoreResult<()> {
        self.grid(owner)?;
        self.grid(descriptor.target)?;
        self.views.insert(owner, descriptor);
        Ok(())
    }

    /// Returns the persistent view descriptor owned by one window grid.
    pub(super) fn window_view_descriptor(
        &self,
        owner: GridId,
    ) -> GridStoreResult<GridViewDescriptor> {
        self.views
            .get(&owner)
            .copied()
            .ok_or(GridStoreError::MissingWindowView(owner))
    }

    /// Resolves one window-owned descriptor into a borrowed view.
    pub(super) fn window_view(&self, owner: GridId) -> GridStoreResult<GridView<'_>> {
        self.view(self.window_view_descriptor(owner)?)
    }

    /// Mutably borrows one registered grid.
    pub(crate) fn grid_mut(&mut self, grid_id: GridId) -> GridStoreResult<&mut ScreenGrid> {
        self.grids
            .get_mut(&grid_id)
            .ok_or(GridStoreError::UnknownGrid(grid_id))
    }

    /// Removes one grid and permanently retires its identity.
    pub(crate) fn remove(&mut self, grid_id: GridId) -> GridStoreResult<ScreenGrid> {
        let grid = self
            .grids
            .remove(&grid_id)
            .ok_or(GridStoreError::UnknownGrid(grid_id))?;
        self.views.remove(&grid_id);
        self.views
            .retain(|_, descriptor| descriptor.target != grid_id);
        let newly_retired = self.retired.insert(grid_id);
        debug_assert!(
            newly_retired,
            "a live grid identity cannot already be retired"
        );
        Ok(grid)
    }

    /// Releases every grid, view, and retired identity during shutdown.
    pub(crate) fn clear(&mut self) {
        self.grids.clear();
        self.views.clear();
        self.retired.clear();
    }
}

/// Acquires exclusive access to permanent screen-grid storage.
pub(crate) fn lock_grid_store() -> MutexGuard<'static, GridStore> {
    registered_context().grid().lock_store()
}

/// One exclusive borrow of a permanently stored screen grid.
pub(super) struct StoredGridGuard {
    store: MutexGuard<'static, GridStore>,
    grid_id: GridId,
}

impl StoredGridGuard {
    /// Borrows a registered grid until this guard is dropped.
    pub(super) fn acquire(grid_id: GridId) -> GridStoreResult<Self> {
        let store = lock_grid_store();
        store.grid(grid_id)?;
        Ok(Self { store, grid_id })
    }
}

impl Deref for StoredGridGuard {
    type Target = ScreenGrid;

    fn deref(&self) -> &Self::Target {
        self.store
            .grid(self.grid_id)
            .expect("stored grid disappeared while borrowed")
    }
}

impl DerefMut for StoredGridGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.store
            .grid_mut(self.grid_id)
            .expect("stored grid disappeared while mutably borrowed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{GridPosition, LineBuffer};

    fn grid_with_id(grid_id: i32) -> ScreenGrid {
        let mut line_buffer = LineBuffer::new(2);
        let mut grid = ScreenGrid::new(1, 2, true, &mut line_buffer);
        grid.handle = grid_id;
        grid
    }

    #[test]
    fn registers_and_borrows_owned_grids() {
        let mut store = GridStore::new();
        let grid_id = GridId::new(7);

        assert_eq!(store.register(grid_with_id(7)), Ok(grid_id));
        store.grid_mut(grid_id).unwrap().chars[0] = 42;

        assert_eq!(store.grid(grid_id).unwrap().chars, [42, 0x20]);
    }

    #[test]
    fn resolves_descriptors_into_borrowed_views() {
        let mut store = GridStore::new();
        let grid_id = GridId::new(7);
        store.register(grid_with_id(7)).unwrap();

        let view = store
            .view(GridViewDescriptor::new(grid_id, GridPosition::new(2, 3)))
            .unwrap();

        assert_eq!(view.target.id(), grid_id);
        assert_eq!(view.offset, GridPosition::new(2, 3));
    }

    #[test]
    fn stores_window_views_by_owner_identity() {
        let mut store = GridStore::new();
        let default_grid_id = GridId::new(1);
        let first_owner = GridId::new(7);
        let second_owner = GridId::new(8);
        store.register(grid_with_id(1)).unwrap();
        store.register(grid_with_id(7)).unwrap();
        store.register(grid_with_id(8)).unwrap();
        let first = GridViewDescriptor::new(default_grid_id, GridPosition::new(2, 3));
        let second = GridViewDescriptor::new(default_grid_id, GridPosition::new(4, 5));

        store.set_window_view(first_owner, first).unwrap();
        store.set_window_view(second_owner, second).unwrap();

        assert_eq!(store.window_view_descriptor(first_owner), Ok(first));
        assert_eq!(store.window_view_descriptor(second_owner), Ok(second));
        assert_eq!(
            store.window_view(first_owner).unwrap().offset,
            GridPosition::new(2, 3)
        );
    }

    #[test]
    fn removing_a_grid_removes_owned_and_targeting_views() {
        let mut store = GridStore::new();
        let target = GridId::new(7);
        let owner = GridId::new(8);
        store.register(grid_with_id(7)).unwrap();
        store.register(grid_with_id(8)).unwrap();
        store
            .set_window_view(
                target,
                GridViewDescriptor::new(target, GridPosition::new(0, 0)),
            )
            .unwrap();
        store
            .set_window_view(
                owner,
                GridViewDescriptor::new(target, GridPosition::new(1, 2)),
            )
            .unwrap();

        store.remove(target).unwrap();

        assert_eq!(
            store.window_view_descriptor(target),
            Err(GridStoreError::MissingWindowView(target))
        );
        assert_eq!(
            store.window_view_descriptor(owner),
            Err(GridStoreError::MissingWindowView(owner))
        );
    }

    #[test]
    fn creates_empty_grids_with_stable_identities() {
        let mut store = GridStore::new();
        let grid_id = GridId::new(7);

        assert_eq!(store.register_empty(grid_id), Ok(grid_id));

        let grid = store.grid(grid_id).unwrap();
        assert_eq!(grid.id(), grid_id);
        assert!(!grid.is_allocated());
        let size = grid.size();
        assert_eq!((size.height, size.width), (0, 0));
    }

    #[test]
    fn rejects_invalid_and_duplicate_identities() {
        let mut store = GridStore::new();
        let grid_id = GridId::new(7);

        assert_eq!(
            store.register(grid_with_id(0)),
            Err(GridStoreError::InvalidIdentity(GridId::new(0)))
        );
        assert_eq!(store.register(grid_with_id(7)), Ok(grid_id));
        assert_eq!(
            store.register(grid_with_id(7)),
            Err(GridStoreError::AlreadyRegistered(grid_id))
        );
    }

    #[test]
    fn removed_identities_cannot_be_resolved_or_reused() {
        let mut store = GridStore::new();
        let grid_id = GridId::new(7);
        store.register(grid_with_id(7)).unwrap();

        let removed = store.remove(grid_id).unwrap();

        assert!(matches!(
            store.grid(grid_id),
            Err(GridStoreError::UnknownGrid(id)) if id == grid_id
        ));
        assert_eq!(
            store.register(removed),
            Err(GridStoreError::RetiredIdentity(grid_id))
        );
    }
}
