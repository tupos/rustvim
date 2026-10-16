# grid/popup_surface.rs

Source: `src/nvim/rustvim_core/src/grid/popup_surface.rs`.

This file manages the popup menu's grid storage, screen placement, and UI
notifications. This includes the built-in completion menu.

## Ownership

The main type borrows two subsystems:

```rust
pub(crate) struct PopupMenuSurface<'context> {
    grids: &'context GridContext,
    compositor: &'context CompositorContext,
}
```

`GridContext` owns the cells. The compositor places that rectangular grid over
the editor's other grids.

## Updating and hiding the menu

The main methods handle these operations:

| Method | Responsibility |
| --- | --- |
| `update()` | Registers a grid on first use, places its compositor layer, and allocates or resizes cell storage. |
| `hide()` | Removes the layer, optionally sends window-close and grid-destroy events, then releases cell storage. |
| `publish_position()` | Calculates and publishes the menu's position and stacking information for UI clients. |
| `recompose()` | Asks the compositor to redraw the screen area occupied by the menu. |

Suppose C requests a menu at row 10, column 20, with five rows and twelve
columns. `update()` places that grid and ensures it has storage for those
dimensions. If the menu moved or its contents were marked invalid, existing
cells are treated as needing redraw.

C's `popupmenu.c` then draws the entries, selected-item highlighting,
scrollbar, and border through the grid drawing functions.

When the menu closes, its compositor layer is removed before cell storage is
released. Its grid identifier remains registered for reuse.

## Anchors and position publication

`publish_position()` handles which corner anchors the menu:

- A menu below its anchor uses its top-left corner: `NorthWest`.
- A menu above its anchor uses its bottom-left corner: `SouthWest`.

It also converts the screen position into coordinates relative to the anchor
grid. This lets a UI place the menu correctly when the containing window has
an offset.

Position publication is separate because the compositor's layer order can
change even when the menu itself stays in place. The caller can also force
publication.

## Requests, output, and the C boundary

The remaining types package information for these operations: placement
requests, publication conditions, anchor geometry, hide requests, and the
resulting position event.

`PopupMenuSurfaceOutput` supplies the methods that deliver popup-specific
window-close and position events. It extends the shared `GridSurfaceOutput`
interface for grid resize, destruction, and composed screen updates.

The entire module forbids unsafe code. C representations, exported entry
points, and calls into the C UI functions live in `grid/ffi/popup.rs`.
