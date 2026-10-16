# grid/line_submission.rs

Source: `src/nvim/rustvim_core/src/grid/line_submission.rs`.

This file prepares a finished screen row for copying into the stored grid.

`LineSubmission::prepare()` receives the row buffer, the end of the prepared
text, the available row width, and options for clearing, right-to-left display,
and a smooth-scroll marker.

It first checks that the text ends within the requested width and that the
width fits the buffer. Invalid boundaries return `None`.

## Writing and clearing

Suppose the grid currently displays `123456`, and the new prepared text is
`abcd`. The row width is six cells.

| Request | Grid contents after the result is applied |
| --- | --- |
| `clear_end = false` | `abcd56` |
| `clear_end = true` | `abcd` followed by two spaces |

Preparation records the clearing boundaries. The grid operation later performs
the clearing.

For right-to-left display, preparation mirrors the text toward the right edge
and translates the write/clear boundaries into the existing grid API's
right-to-left convention.

## Smooth-scroll marker

`SmoothScrollMarker` specifies the marker's highlighting and whether leading
line-number digits must be preserved.

When requested, preparation writes `<<<` into the buffer after mirroring.
For example, the test transforms `12xyzw` into `12<<<w` while preserving the
leading digits.

The caller decides whether a marker is needed; this file applies that request.

## Result and caller

`LineSubmission` stores `start_col`, `end_col`, `clear_width`, and `right_left`.
Its accessors return those values. Their interpretation follows the direction
convention used by the grid API.

The C-facing `grid_put_window_line()` calls preparation, then passes the
result and buffer to `ScreenGrid::put_line_buffer()`.

Tests cover write/clear boundaries, right-to-left mirroring, marker placement,
and rejecting invalid boundaries without changing the buffer.
