#![forbid(unsafe_code)]

use std::marker::PhantomData;

/// Marker for coordinates relative to an individual screen grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LocalGridSpace;

/// Marker for coordinates in the composed output grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ComposedGridSpace;

/// A row and column belonging to coordinate space `Space`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Position<Space> {
    pub(crate) row: i32,
    pub(crate) column: i32,
    space: PhantomData<Space>,
}

/// A position relative to an individual screen grid.
pub(crate) type GridPosition = Position<LocalGridSpace>;

/// A position in the composed output grid.
pub(crate) type ComposedPosition = Position<ComposedGridSpace>;

impl<Space> Position<Space> {
    /// Creates a position without changing its signed coordinates.
    pub(crate) const fn new(row: i32, column: i32) -> Self {
        Self {
            row,
            column,
            space: PhantomData,
        }
    }
}

impl Position<ComposedGridSpace> {
    /// Translates a grid-local position relative to this composed-grid origin.
    pub(crate) fn checked_add_local(self, local: GridPosition) -> Option<Self> {
        Some(Self::new(
            self.row.checked_add(local.row)?,
            self.column.checked_add(local.column)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_grid_position_to_composed_position() {
        let origin = ComposedPosition::new(3, 5);

        assert_eq!(
            origin.checked_add_local(GridPosition::new(2, 4)),
            Some(ComposedPosition::new(5, 9))
        );
    }

    #[test]
    fn rejects_overflowing_position_translation() {
        let origin = ComposedPosition::new(i32::MAX, 0);

        assert_eq!(origin.checked_add_local(GridPosition::new(1, 0)), None);
    }
}
