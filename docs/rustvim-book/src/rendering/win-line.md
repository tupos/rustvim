# The current C win_line

Source: `src/nvim/drawline.c`.

This chapter records the initial overview. The detailed walkthrough of the
function is still in progress.

`win_update()` in `drawscreen.c` chooses which buffer lines need updating.
It calls `win_line()` to draw one buffer-line entry into the available
screen rows.

## Three uses of the function

| Use | Relevant input |
| --- | --- |
| Draw a buffer line and its associated rows | `col_rows == 0` |
| Update only the left-side columns of an already displayed line | `col_rows > 0` |
| Draw associated filler/virtual rows without buffer text | Concealed line or the line beyond EOF |

The caller provides the window, buffer line number, available screen-row
range, fold information, and spell state retained between calls.

## Work performed in C

Before entering the main loop, the function prepares selection boundaries,
diff and virtual rows, line highlighting, spelling context, the first visible
source position, decoration providers, and search highlighting.

The main loop combines several jobs:

- Start a screen row and draw its fold, sign, number, or status column.
- Draw breakindent, showbreak, and filler content.
- Read buffer text or continue previously generated display text.
- Obtain search, syntax, spelling, and decoration information.
- Expand tabs and non-printable characters, apply list characters and concealment.
- Write or skip the resulting screen cells.
- Finish a row, wrap onto another row, or finish the buffer line.

`n_extra`, `p_extra`, `sc_extra`, and `sc_final` describe display text still
waiting to be emitted. A tab or inline virtual text can therefore take several
iterations while the buffer position remains unchanged.

The counters also measure different things:

| Value | Meaning |
| --- | --- |
| `ptr - line` | Byte position in buffer text |
| `wlv.row` | Current screen row within the window |
| `wlv.off` | Position being written in the temporary row buffer |
| `wlv.col` | Column used for layout and wrapping |
| `wlv.vcol` | Virtual display column across wrapped rows |

Concealment can advance layout without writing a cell, so `col` and `off`
can differ. `boguscols` records that fictitious column advance.

## Existing Rust calls

C already uses Rust to write prepared glyphs, draw complete strings,
generate fold indicators, draw virtual-text items, and update stored grids.

Inline virtual text goes through the C character loop. Overlay, end-of-line,
right-aligned virtual text, and virtual-line contents use the Rust
virtual-text renderer.

Some editor operations can invalidate a borrowed memline pointer. The C code
reacquires `line` and restores `ptr` from its byte offset after such operations.
