# grid.rs

Source: `src/nvim/rustvim_core/src/grid.rs`.

This file implements screen-grid storage and operations for drawing into it.
It also declares the grid submodules.

## Stored screen contents

`ScreenGrid` represents a rectangular screen area.

| Field | Contents |
| --- | --- |
| `chars` | Encoded displayed characters |
| `attrs` | Highlight identifiers |
| `vcols` | Virtual columns for mouse/cursor mapping |
| `line_offset` | Starting storage index of each screen row |
| `size` | Grid height and width |

Row 2, column 3 is stored at `line_offset[2] + 3` in each of the three cell
arrays. Whole-row scrolling can rearrange row offsets instead of copying all
the cell contents.

`ScreenGrid` supports allocation, resizing, clearing, scrolling, invalidation,
and reading cells.

`put_line_buffer()` compares a completed temporary row with the stored grid.
It updates changed cells, clears the requested remainder, and returns owned
UI commands describing the changes. Virtual-column metadata is refreshed even
when the displayed characters and highlighting are unchanged.

When output is delayed, the grid records dirty columns for later publication.
Otherwise, the caller emits the returned commands after releasing the grid
and line-buffer borrows.

## Resource ownership and views

`GridContext` owns the grid collection, message-grid bookkeeping, glyph cache,
grid-identifier allocator, and shared temporary `LineBuffer`.
`GridStore`, implemented in `grid/store.rs`, owns the registered grids and
looks them up by stable `GridId` values. C callers retain identifiers;
individual Rust operations borrow the corresponding stored grids.

A window can have its own grid or draw into the shared default grid.
`GridViewDescriptor` records its target grid and offset.
`GridView` translates coordinates through that offset.

For example, local position `(2, 3)` with offset `(5, 10)` refers to target-grid
position `(7, 13)`.

Other structures track window-grid configuration, message scrolling, and
changes accumulated while output is delayed.

## Building rows

`TextGlyph` describes one prepared output item: an encoded character or a run
of spaces, its screen width, and the number of source bytes consumed.
Decoding uses Neovim's byte and cluster rules through `mbyte`.

The width and byte count can differ. A tab consumes one byte and produces
several spaces. A wide glyph clipped at the row edge produces one space
without consuming the glyph's bytes.

`LineState` tracks an active drawing operation, including its row, column
offset, changed range, and clear range.
`LineGrid` combines that state with a borrowed `ScreenGrid`.

`CLineGrid` supports C callers whose begin, write, and flush operations happen
in separate calls. It retains a grid identifier between calls and obtains the
grid when needed. Its state is thread-local: each thread completes a
begin-to-flush operation before starting another.

The current `win_line()` uses `linebuf_*()` operations and
`grid_put_window_line()` directly, rather than the `CLineGrid` begin/flush
sequence.

## Character encoding

`VimSCharT` is a `u32` containing up to four encoded bytes directly, or a
reference to longer text in `GlyphMapping`. Its numeric value is an encoding
of screen text, rather than a Unicode codepoint. This permits a displayed
glyph to include combining marks.

`GlyphMapping` stores and reuses those longer sequences.
`GlyphBuff` and `GlyphBuilder` hold and construct their bytes. Clearing the
cache invalidates its encoded references, so the screen must be redrawn.

`ArabicShaper` adjusts Arabic characters according to neighboring characters,
using the existing C `arabic_shape()` primitive.

## Other responsibilities

`Border` and related types draw floating-window edges, corners, titles, and
footers.

`Highlight` wraps a highlight identifier. Combining highlights, resolving
groups, and calculating transparency still call C highlight functions.

The file also contains C-compatible representations such as `CGridCell`,
`CGridLineSubmissionConfig`, and `CVirtText`. Most corresponding exported
entry points live under `grid/ffi/`.

Row construction and submission are described in
[line_buffer.rs](line-buffer.md) and [line_submission.rs](line-submission.md).
Message and popup-menu operations that coordinate storage with compositor
placement live in [message_surface.rs](message-surface.md) and
[popup_surface.rs](popup-surface.md).

The test module covers storage, scrolling, row drawing, character encoding,
borders, and associated behavior.
