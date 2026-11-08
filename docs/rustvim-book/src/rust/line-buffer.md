# grid/line_buffer.rs

Source: `src/nvim/rustvim_core/src/grid/line_buffer.rs`.

This file implements the temporary screen row built before it is copied into
a stored grid. `GridContext` owns one shared `LineBuffer`, reused for
successive rows by window, message, and other grid drawing operations.

```rust
pub struct LineBuffer {
    pub(super) cells: Vec<GridCell>,
    pub(super) mirror_scratch: Vec<GridCell>,
}
```

`cells` holds the row's contents.
`mirror_scratch` provides working storage when reversing a row for
right-to-left display.
Construction/resizing allocates the arrays; ordinary cell writes reuse them.

## Writing characters and strings

`put_rendered_glyph()` writes a prepared character into one or two cells.
A wide character receives an empty continuation cell. Both cells receive the
same highlighting and consecutive virtual columns.

`put_text_glyph()` writes a prepared `TextGlyph` and its virtual-column
metadata. It also handles the continuation cells affected by overwriting
wide characters.

`render_text()` reads a byte slice using Neovim's text-cluster rules, expands
tabs, assigns highlighting and virtual columns, and stops at the specified
column limit or when an input cannot be rendered. It uses `TextGlyph` from
`grid.rs`; input need not be a Rust UTF-8 string.

If a glyph is too wide for the remaining space, `TextGlyph` supplies a space
without consuming that glyph's input bytes. Output still advances toward the
column limit.

`LineTextRenderOptions` supplies the start/end boundaries, highlight,
tab stops, and virtual-column policy.
Virtual columns can be generated consecutively, copied from supplied values,
or marked unavailable.

`LineTextRenderResult` reports the resulting output and virtual columns,
plus progress through color-column metadata.

## Virtual text and partial updates

`put_virtual_text_glyph()` supports replace, combine, and blend highlighting.
It preserves underlying virtual-column mapping.
Blend-through spaces can update highlighting while retaining the characters
underneath.

Fill/update helpers change characters, highlighting, virtual columns, or all
three together.
`put_fold_column()` copies generated fold-column cells and applies the supplied
highlight.

## Final adjustments

Other helpers:

- Place left/right truncation markers.
- Place the `<<<` smooth-scroll marker.
- Mirror the row while retaining wide-character pairs.
- Release or reset working storage.

Cell storage is reused during rendering. Encoding a previously unseen glyph
that exceeds the inline byte capacity can add an entry to `GlyphMapping`.

This file writes within a screen row. Choosing buffer lines, deciding when to
wrap, and obtaining syntax/search/spell highlighting still happen elsewhere.
