#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, hash_map::Entry};

use super::{GridId, ScreenGrid};

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
    retired: HashSet<GridId>,
}

impl GridStore {
    /// Creates an empty grid store.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Registers one grid under its existing stable identity.
    pub(crate) fn register(&mut self, grid: ScreenGrid) -> GridStoreResult<GridId> {
        let grid_id = grid.id();
        if grid_id.as_native_handle() <= 0 {
            return Err(GridStoreError::InvalidIdentity(grid_id));
        }
        if self.retired.contains(&grid_id) {
            return Err(GridStoreError::RetiredIdentity(grid_id));
        }

        match self.grids.entry(grid_id) {
            Entry::Vacant(entry) => {
                entry.insert(grid);
                Ok(grid_id)
            }
            Entry::Occupied(_) => Err(GridStoreError::AlreadyRegistered(grid_id)),
        }
    }

    /// Borrows one registered grid.
    pub(crate) fn grid(&self, grid_id: GridId) -> GridStoreResult<&ScreenGrid> {
        self.grids
            .get(&grid_id)
            .ok_or(GridStoreError::UnknownGrid(grid_id))
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
        let newly_retired = self.retired.insert(grid_id);
        debug_assert!(
            newly_retired,
            "a live grid identity cannot already be retired"
        );
        Ok(grid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::LineBuffer;

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
