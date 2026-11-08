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

`character` is an encoded screen character: up to four bytes stored directly,
or a reference into the glyph cache. One value can represent a base character
and combining marks.
`attribute` identifies its highlighting.
`virtual_column` supplies position information for mouse/cursor mapping,
including special negative markers for fold-column cells.

A wide character occupies two entries. The first contains the character;
the second has `character == 0`, marking its continuation.

A tab has already been expanded when its final cells are written. Those cells
contain spaces or configured tab-display characters.

`GridCell::new()` combines the supplied fields without allocation or
validation. Copying a cell copies its encoded value and metadata; cached
glyph bytes remain in `GlyphMapping`.

The derived default is all zeros, including the continuation marker in
`character`. A blank row is initialized separately with space characters and
unavailable virtual columns (`-1`).

`LineBuffer` stores a `Vec<GridCell>`.
`ScreenGrid` stores separate arrays but also returns a `GridCell` when reading
a complete cell.
