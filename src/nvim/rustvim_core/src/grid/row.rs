use std::ops::Range;

use super::{ScreenGrid, VimSAttrT, VimSCharT};

/// Borrowed character and attribute storage for one logical screen-grid row.
///
/// The row follows `ScreenGrid::line_offset`, so it remains correct when
/// scrolling rotates the physical row allocations.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GridRow<'grid> {
    characters: &'grid [VimSCharT],
    attributes: &'grid [VimSAttrT],
}

impl<'grid> GridRow<'grid> {
    /// Borrows one complete logical row from `grid`.
    pub(crate) fn try_from_grid(grid: &'grid ScreenGrid, row: usize) -> Option<Self> {
        let width = usize::try_from(grid.size().width).ok()?;
        let start = *grid.line_offset.get(row)?;
        let end = start.checked_add(width)?;

        Some(Self {
            characters: grid.chars.get(start..end)?,
            attributes: grid.attrs.get(start..end)?,
        })
    }

    /// Borrows a bounded column range and remembers the following character.
    pub(crate) fn segment(self, columns: Range<usize>) -> Option<GridRowSegment<'grid>> {
        let start = columns.start;
        let end = columns.end;
        if start > end {
            return None;
        }

        Some(GridRowSegment {
            characters: self.characters.get(start..end)?,
            attributes: self.attributes.get(start..end)?,
            following_character: self.characters.get(end).copied(),
        })
    }

    /// Reports whether one column contains a valid rendered cell.
    pub(crate) fn is_valid_at(self, column: usize) -> bool {
        self.attributes
            .get(column)
            .is_some_and(|attribute| *attribute >= 0)
    }
}

/// Borrowed grid cells participating in one compositor span.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GridRowSegment<'grid> {
    characters: &'grid [VimSCharT],
    attributes: &'grid [VimSAttrT],
    following_character: Option<VimSCharT>,
}

impl<'grid> GridRowSegment<'grid> {
    /// Returns the number of cells in this segment.
    pub(crate) fn len(self) -> usize {
        self.characters.len()
    }

    /// Borrows the rendered characters in this segment.
    pub(crate) fn characters(self) -> &'grid [VimSCharT] {
        self.characters
    }

    /// Borrows the highlight attributes in this segment.
    pub(crate) fn attributes(self) -> &'grid [VimSAttrT] {
        self.attributes
    }

    /// Reports whether the next grid cell is the continuation of a wide cell.
    pub(crate) fn is_followed_by_continuation(self) -> bool {
        self.following_character == Some(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{LineBuffer, schar_from_ascii};

    fn grid_with_row(characters: &[VimSCharT], attributes: &[VimSAttrT]) -> ScreenGrid {
        assert_eq!(characters.len(), attributes.len());
        let mut line_buffer = LineBuffer::new(characters.len());
        let mut grid = ScreenGrid::new(
            1,
            i32::try_from(characters.len()).unwrap(),
            true,
            &mut line_buffer,
        );
        grid.chars.copy_from_slice(characters);
        grid.attrs.copy_from_slice(attributes);
        grid
    }

    #[test]
    fn follows_rotated_line_offsets() {
        let mut line_buffer = LineBuffer::new(3);
        let mut grid = ScreenGrid::new(2, 3, true, &mut line_buffer);
        grid.chars = "abcdef".chars().map(schar_from_ascii).collect();
        grid.attrs = vec![1, 2, 3, 4, 5, 6];
        grid.line_offset.swap(0, 1);

        let row = GridRow::try_from_grid(&grid, 0).unwrap();
        let segment = row.segment(0..3).unwrap();

        assert_eq!(
            segment.characters(),
            [
                schar_from_ascii('d'),
                schar_from_ascii('e'),
                schar_from_ascii('f'),
            ]
        );
        assert_eq!(segment.attributes(), [4, 5, 6]);
        assert!(row.is_valid_at(0));
        assert!(row.is_valid_at(1));

        let first_row_offset = grid.line_offset[0];
        grid.attrs[first_row_offset + 1] = -1;
        let row = GridRow::try_from_grid(&grid, 0).unwrap();
        assert!(row.is_valid_at(0));
        assert!(!row.is_valid_at(1));
        assert!(!row.is_valid_at(3));
    }

    #[test]
    fn reports_a_following_wide_cell_continuation() {
        let grid = grid_with_row(
            &[schar_from_ascii('W'), 0, schar_from_ascii('x')],
            &[1, 1, 1],
        );
        let row = GridRow::try_from_grid(&grid, 0).unwrap();

        assert!(row.segment(0..1).unwrap().is_followed_by_continuation());
        assert!(!row.segment(0..3).unwrap().is_followed_by_continuation());
    }

    #[test]
    fn rejects_rows_and_segments_outside_the_grid() {
        let grid = grid_with_row(&[schar_from_ascii('a'), schar_from_ascii('b')], &[1, 2]);

        assert!(GridRow::try_from_grid(&grid, 1).is_none());
        let row = GridRow::try_from_grid(&grid, 0).unwrap();
        assert!(row.segment(1..3).is_none());
        let reversed_end = 0;
        assert!(row.segment(1..reversed_end).is_none());
    }
}
