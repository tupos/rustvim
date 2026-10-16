# grid/cell.rs

Source: `src/nvim/rustvim_core/src/grid/cell.rs`.

`GridCell` describes one screen cell:

```rust
pub(crate) struct GridCell {
    pub(super) character: VimSCharT,
    pub(super) attribute: VimSAttrT,
    pub(super) virtual_column: VimColNrT,
}
```

`character` is the encoded screen character.
`attribute` identifies its highlighting.
`virtual_column` supplies position information for mouse/cursor mapping,
including special negative markers for fold-column cells.

A wide character occupies two entries. The first contains the character;
the second has `character == 0`, marking its continuation.

A tab has already been expanded when its final cells are written. Those cells
contain spaces or configured tab-display characters.

`GridCell::new()` combines the supplied fields without allocation or
validation. The derived traits support copying, comparison, debug printing,
and an all-zero default value.

`LineBuffer` stores a `Vec<GridCell>`.
`ScreenGrid` stores separate arrays but also returns a `GridCell` when reading
a complete cell.
