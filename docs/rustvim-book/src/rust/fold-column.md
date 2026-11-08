# grid/fold_column.rs

Source: `src/nvim/rustvim_core/src/grid/fold_column.rs`.

This file generates the fold indicators displayed beside a line.
It receives the reserved width, fold information, configured symbols, and
whether this is a virtual/filler row.

`FoldColumnInfo` describes nesting and whether a fold starts here or is closed.
`FoldColumnSymbols` supplies open, closed, separator, and optional inner symbols.

## What render produces

`FoldColumn::render()` constructs an owned array with exactly `width` entries.
It does not itself write to the screen.

For width 3 and a closed fold at nesting level 1, with symbol `+`, the result is:

```text
Cell:        0    1    2
Character:  '+'  ' '  ' '
Marker:      -2   -1   -1
```

With no folds, all three characters are spaces.
The width is the horizontal space reserved for fold indicators, not the width
of the buffer text.

The algorithm handles closed folds, fold starts, continuing levels, and nesting
too deep to fit. Deep nesting can use the configured inner symbol or numeric
depth indicators, followed by `>` beyond nine.

For virtual rows attached to a line where folds start, the displayed symbols
show only the continuing outer levels. Mouse-action markers are computed
separately from these symbols, so a blank cell can still carry a fold action.

## Metadata and output

Each `FoldColumnCell` contains a character and a mouse-action marker:

| Marker | Meaning |
| --- | --- |
| `-1` | No fold action |
| `-2` | Open a closed fold |
| `-3` | Close a fold |

Highlighting is supplied later by the caller.
The result is stored in `Box<[FoldColumnCell]>`.

`cells()` borrows the generated entries.
`copy_to()` copies characters and markers into separate arrays, requiring both
arrays to match the generated width. A mismatch returns `false` without
modifying either array.

C's `fill_foldcolumn()` calls the Rust boundary
`linebuf_render_fold_column()`, which invokes this implementation.
It supports both the ordinary fold column and fold indicators inside
`'statuscolumn'`.

C determines the fold state, width, and highlighting. This file selects the
symbols and their fold-action metadata. The fold-column path assumes each
configured symbol occupies one screen cell.
