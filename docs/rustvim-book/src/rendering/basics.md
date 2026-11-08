# Cells, rows, and grids

A screen cell is one position in a grid, identified by a row and column.
It contains displayed text, a highlight identifier, and virtual-column
metadata used for mouse and cursor mapping.

A glyph is a displayed character, possibly with combining marks. Glyphs,
Unicode codepoints, source bytes, and screen cells are different:

- A wide glyph such as `界` occupies two cells. The first stores the glyph;
  the second is marked as its continuation.
- A base character and its combining marks can occupy one cell.
- A tab expands to several cells to reach the next tab stop.
- A non-printable byte can produce several cells, such as `^G`.

A virtual column measures a position along the displayed buffer line before
wrapping. A screen column measures a position within one screen row.
Some cells, such as fold markers, use special metadata instead of a
buffer-text position.

For example, with `tabstop=4`:

```text
Buffer text:  A<Tab>B
Screen cells: [A][ ][ ][ ][B]
```

## Buffer lines and screen rows

A buffer line is one line of the edited document. A screen row is one
horizontal row of displayed cells.

One buffer line can occupy several screen rows when wrapping is enabled.
A closed fold can represent several buffer lines in one displayed row.
Virtual lines introduce screen rows without corresponding buffer text.

Each row can contain more than buffer text: fold, sign, and number columns,
continuation prefixes, virtual text, and unused space also occupy cells.

## The temporary row and the stored grid

`LineBuffer` is temporary storage for the screen row currently being built.
It is reused as rendering moves from one row to the next; it does not hold
all wrapped rows of a buffer line.

`ScreenGrid` retains the rendered contents of a rectangular screen area
between updates. A window can draw into its own grid or into a region of
the shared default grid.

During window-line rendering:

1. Reset the visible part of `LineBuffer`.
2. Fill it with the row's columns, text, and decorations.
3. Submit the row through C's `wlv_put_linebuf()`, which calls Rust's
   `grid_put_window_line()`.
4. Prepare the submitted range, including right-to-left mirroring and any
   smooth-scroll marker.
5. Update the target `ScreenGrid` and produce UI updates for visual changes.

Submission also updates virtual-column metadata, even when the displayed
text and highlighting have not changed.

For UIs that receive a single composed grid, the compositor combines grids
according to their positions and layer order. UIs using `ext_multigrid`
receive separate grids and arrange them themselves.

## What column width means

Width is horizontal space measured in screen cells.

The "fold column" is the vertical area beside buffer text that displays fold
indicators. That area can occupy several screen columns:

```text
Width 1: +Hello world
Width 3: +  Hello world
```

In the second example, the fold area contains one marker and two spaces.
