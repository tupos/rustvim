#![forbid(unsafe_code)]

use std::marker::PhantomData;
use std::ops::Range;

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

/// A half-open row and column range belonging to coordinate space `Space`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Region<Space> {
    pub(crate) rows: Range<i32>,
    pub(crate) columns: Range<i32>,
    space: PhantomData<Space>,
}

/// An ordered region relative to an individual screen grid.
pub(crate) type GridRegion = Region<LocalGridSpace>;

/// An ordered region in the composed output grid.
pub(crate) type ComposedRegion = Region<ComposedGridSpace>;

/// Nonnegative dimensions of a screen grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GridSize {
    pub(crate) height: i32,
    pub(crate) width: i32,
}

/// A signed row and column displacement used by grid scrolling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GridDelta {
    pub(crate) rows: i32,
    pub(crate) columns: i32,
}

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

impl<Space> Region<Space> {
    /// Constructs a region with ordered row and column boundaries.
    pub(crate) fn try_new(rows: Range<i32>, columns: Range<i32>) -> Option<Self> {
        (rows.start <= rows.end && columns.start <= columns.end).then_some(Self {
            rows,
            columns,
            space: PhantomData,
        })
    }
}

impl GridSize {
    /// Constructs nonnegative grid dimensions.
    pub(crate) fn try_new(height: i32, width: i32) -> Option<Self> {
        (height >= 0 && width >= 0).then_some(Self { height, width })
    }
}

impl GridDelta {
    /// Creates a signed grid displacement.
    pub(crate) const fn new(rows: i32, columns: i32) -> Self {
        Self { rows, columns }
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

impl Region<LocalGridSpace> {
    /// Translates a grid-local region relative to a composed-grid origin.
    pub(crate) fn checked_translate(self, origin: ComposedPosition) -> Option<ComposedRegion> {
        ComposedRegion::try_new(
            self.rows.start.checked_add(origin.row)?..self.rows.end.checked_add(origin.row)?,
            self.columns.start.checked_add(origin.column)?
                ..self.columns.end.checked_add(origin.column)?,
        )
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

    #[test]
    fn validates_and_translates_regions() {
        assert!(GridRegion::try_new(Range { start: 2, end: 1 }, 0..1).is_none());
        assert!(GridRegion::try_new(0..1, Range { start: 2, end: 1 }).is_none());

        let region = GridRegion::try_new(1..3, 2..5).unwrap();
        assert_eq!(
            region.checked_translate(ComposedPosition::new(4, 6)),
            ComposedRegion::try_new(5..7, 8..11)
        );
    }

    #[test]
    fn validates_grid_sizes() {
        assert_eq!(
            GridSize::try_new(2, 3),
            Some(GridSize {
                height: 2,
                width: 3,
            })
        );
        assert_eq!(GridSize::try_new(-1, 3), None);
        assert_eq!(GridSize::try_new(2, -1), None);
    }
}
