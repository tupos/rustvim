# Cells, rows, and grids

A screen cell is one position identified by a row and column.
It contains a displayed character, a highlight identifier, and a virtual
column used for mouse and cursor mapping.

A cell is not the same thing as a source byte or Unicode codepoint:

- A wide character occupies two cells.
- A character with combining marks can contain several codepoints.
- A tab can produce several cells.

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

## The temporary row and the stored grid

`LineBuffer` holds the screen row currently being built.
`ScreenGrid` stores the contents of all rows in a rectangular screen area.

The current drawing sequence is:

1. Build a screen row in `LineBuffer`.
2. Call `grid_put_window_line()` through C's `wlv_put_linebuf()`.
3. Compare the completed row with the stored grid and update changed cells.
4. Produce UI updates for the changes.
5. Reset the temporary buffer when starting another screen row.

The compositor combines grids according to their positions and layer order.
The UI output path delivers the resulting updates to UI clients.

## What column width means

Width is horizontal space measured in screen cells.

The "fold column" is the vertical area beside buffer text that displays fold
indicators. That area can occupy several screen columns:

```text
Width 1: +Hello world
Width 3: +  Hello world
```

In the second example, the fold area contains one marker and two spaces.
