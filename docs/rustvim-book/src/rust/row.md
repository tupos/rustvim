# grid/row.rs

Source: `src/nvim/rustvim_core/src/grid/row.rs`.

This file provides read-only access to a row, or part of a row, already stored
in `ScreenGrid`. The compositor and UI output code use it to read characters
and highlighting.

## GridRow

`GridRow` borrows a complete row:

```rust
pub(crate) struct GridRow<'grid> {
    characters: &'grid [VimSCharT],
    attributes: &'grid [VimSAttrT],
}
```

These slices refer directly to the grid's arrays. Creating the view does not
allocate or copy the row.

`try_from_grid()` finds the row through `line_offset`. This matters because
scrolling can rearrange row offsets instead of moving the cells.

For example, physical storage may contain `abc` followed by `def`, while the
offsets say that logical row zero is now `def`. A row view created after that
change reads `def`.

The other methods are:

- `segment(2..5)` borrows columns 2, 3, and 4.
- `is_valid_at(column)` checks whether the cell has a nonnegative highlight
  attribute. Negative attributes mark invalid stored content. An out-of-range
  column returns `false`.

Out-of-range rows and segments return `None`. Reversed segment bounds also
return `None`.

## GridRowSegment

`GridRowSegment` borrows a portion of a row. It exposes the segment's length,
characters, and attributes. It also remembers the character immediately after
the segment, if one exists.

That last detail handles wide characters. Suppose the stored cells are:

```text
[A][界][continuation][B]
```

A segment ending just after `界` would contain only the first half of that
wide character. `is_followed_by_continuation()` detects this because the next
cell contains the continuation marker, zero.

The compositor uses that information to repair a character cut by an overlap
or clipping boundary.

These views carry characters and highlighting. Virtual-column metadata stays
in the grid.

## Tests

The tests cover rotated row offsets, cell validity, wide-character
continuation detection, and invalid bounds.
