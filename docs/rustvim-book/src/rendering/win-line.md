# The current C win_line

Source: `src/nvim/drawline.c`; caller: `win_update()` in
`src/nvim/drawscreen.c`.

`win_update()` chooses which buffer lines need redrawing. `win_line()` turns
one buffer-line entry into screen rows, including wrapped text and associated
diff filler or virtual lines. A closed fold represents several buffer lines
in one text row.

It builds one row at a time in `LineBuffer` and submits completed rows to the
window's target `ScreenGrid`. The same temporary buffer is reused for the
next row.

## Inputs and drawing modes

The caller supplies the window, the one-based buffer line number `lnum`,
the window-relative row range `[startrow, endrow)`, fold information, and
spelling state retained between calls.

| Mode | Input |
| --- | --- |
| Draw text and associated rows | `col_rows == 0`, with buffer text visible |
| Update information columns while preserving existing text | `col_rows > 0`, giving the line's existing screen height |
| Draw associated rows without buffer text | `concealed`, or `lnum` is one past the last buffer line |

For a concealed line, the remaining virtual rows belong to the preceding
line. The call beyond EOF draws filler below the last buffer line.

A normal completed draw returns the row immediately after its output. If a
line continues below the available area, the return value can be
`endrow + 1`, signalling that it did not fit.

## Building the rows

Setup prepares selection boundaries, diff and virtual rows, highlighting,
spelling context, decoration providers, search matches, and the first visible
source position. Horizontal scrolling and smooth scrolling can make that
position fall inside a tab or another multi-cell expansion.

The main loop interleaves these operations:

1. Start a row with the built-in fold, sign, and number columns, or evaluate
   and draw `'statuscolumn'`.
2. Add continuation prefixes (`'breakindent'` and `'showbreak'`) or filler.
3. Read buffer text or continue pending display text.
4. Resolve highlighting, expand tabs and non-printable characters, apply
   `'listchars'`, and decide whether concealment replaces or hides text.
5. Write cells and their virtual-column metadata, or account for skipped
   cells. Correct cursor geometry when the cursor position is reached.
6. At a row boundary or end of text, draw the applicable virtual text and
   submit the completed row through `wlv_put_linebuf()`.

Wrapping starts another row while retaining the source position and pending
output. Reaching the end of buffer text finishes the last row, including
highlighting and virtual-column metadata for space after the text.

## Pending display text

Reading one source character can produce several output cells. A tab, for
example, is one source byte but may require several spaces. Inline virtual
text also adds output without consuming buffer bytes.

| Field | Meaning |
| --- | --- |
| `p_extra` | NUL-terminated text still being emitted |
| `sc_extra` | A glyph to repeat instead of reading `p_extra` |
| `sc_final` | An optional different glyph for the final repetition |
| `n_extra` | Remaining bytes for text, or remaining emissions for repeated glyphs |

These fields let output continue across loop iterations and wrapped rows
without reading another source character. The associated attributes remain
active until that output is exhausted.

## Positions and concealment

`winlinevars_T` (`wlv`) holds state shared with the drawing helpers. Its
position counters measure different things:

| Value | Meaning |
| --- | --- |
| `ptr - line` | Byte offset in the buffer line |
| `wlv.row` | Current screen row relative to the window |
| `wlv.off` | Next cell to write in `LineBuffer` |
| `wlv.col` | Column used for layout and wrapping |
| `wlv.vcol` | Virtual-column progress across wrapped rows |
| `wlv.vcol_off_co` | Hidden cells tracked by the legacy concealment path |
| `wlv.extconceal_off` | Source-versus-displayed cell offset for persistent extmark concealment |

A multibyte glyph advances the source pointer by several bytes but occupies
one or two cells. A tab advances it by one byte and can occupy several cells.
Concealment can consume source text without writing any cells.

The legacy concealment path, used by syntax and match concealment, can
advance `col` without advancing `off` to preserve wrapping positions.
`boguscols` tracks this fictitious layout advance, while `vcol_off_co` is
used to correct displayed positions.

Persistent extmark concealment behaves differently when text wraps: hidden
cells are removed from the wrapped layout. `extconceal_off` tracks the
difference so tabs can still be sized using source columns. It can be
negative when a replacement is wider than the text it replaces.

## The C and Rust boundary

C controls source traversal, layout, highlighting decisions, and editor
callbacks. It calls these Rust operations to construct and submit cells:

| Operation | Rust entry point |
| --- | --- |
| Write a prepared glyph | `linebuf_put_rendered_glyph()` |
| Render a complete text run | `linebuf_render_text()` |
| Generate and write fold indicators | `linebuf_render_fold_column()` |
| Render virtual-text chunks | `linebuf_render_virtual_text()` |
| Submit a row to its stored grid | `grid_put_window_line()` |

Inline virtual text goes through the C character loop because it affects
layout alongside buffer text. Overlay, end-of-line, right-aligned virtual
text, and virtual-line contents use the Rust virtual-text renderer.

## Borrowed buffer text

`line` points into Neovim's memline storage, and `ptr` points within that
line. Editor operations such as spelling queries, decoration callbacks, and
`'statuscolumn'` or `'foldtext'` evaluation can invalidate those pointers.
The code saves the byte offset, reacquires `line`, and restores `ptr` after
such calls. The offset survives the call; the borrowed pointer may not.
