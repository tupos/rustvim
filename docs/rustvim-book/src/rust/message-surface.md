# grid/message_surface.rs

Source: `src/nvim/rustvim_core/src/grid/message_surface.rs`.

This file manages the grid used to display messages, such as `:echo` output
and errors, when messages are drawn as screen cells.

## Ownership

The main type contains two references:

```rust
pub(crate) struct MessageSurface<'context> {
    grids: &'context GridContext,
    compositor: &'context CompositorContext,
}
```

It coordinates grid storage and screen placement. The actual cells belong to
`GridContext`; the compositor determines how the message grid appears over
other grids. `MessageSurface` borrows those subsystems for an operation.

## Placement and scrolling

Consider a screen with 24 rows and `cmdheight=1`. Initially, the message grid
starts at screen row 23, so only its first row is visible at the bottom.
The allocated grid itself has the screen's full dimensions.

When another message line needs space, the starting position can move to row
22, exposing two rows. Once the position reaches row zero, further scrolling
shifts the contents inside the grid.

The main methods handle these operations:

| Method | Responsibility |
| --- | --- |
| `update()` | Creates or resizes storage, positions the compositor layer, or removes it when no longer needed. |
| `set_position()` | Updates the coordinate mapping for message drawing and publishes the position unless output is delayed. |
| `publish_position()` | Tells the compositor and UI clients where the message grid belongs. |
| `scroll_up()` | Moves the message area upward or scrolls its contents, then clears the bottom row for new text. |
| `flush_scroll()` | Sends scrolling and cell updates accumulated while output was delayed. |

Delaying output matters when a command produces many message lines.
Storage still changes, but several small scroll operations can be combined
before notifying the UI.

When removing the message grid, `update()` removes its compositor layer before
releasing its cell storage. This lets the compositor restore the screen area
previously covered by messages. The grid identifier remains registered.

## Inputs, results, and UI output

Appearance and position settings specify the separator character, stacking
order, and blending.

Request types carry screen dimensions, scrolling state, command height, and
grid identifiers. Conversion methods validate the incoming C representations
before constructing the corresponding Rust requests.

`MessageSurfaceUpdate` returns the resulting grid identifier, position, and
whether the command line needs redrawing.

`MessageSurfaceOutput` provides methods for sending UI events. Events are sent
after the relevant storage locks have been released. Owned cell updates remain
valid while the UI callbacks run.

## Clearing and callers

At the end of the file, `GridContext::clear_view()` clears a rectangle through
a grid view, using `LineGrid` and the shared line buffer. It returns the UI
updates for the caller to send.

The entry path starts in C's `message.c`, passes through
`grid/ffi/message.rs`, and reaches this coordinator.

Message text formatting and character writes happen elsewhere. This file
manages the message area's storage, placement, scrolling, and UI notifications.
