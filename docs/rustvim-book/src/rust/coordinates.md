# grid/coordinates.rs

Source: `src/nvim/rustvim_core/src/grid/coordinates.rs`.

This file defines positions, rectangles, sizes, and scrolling distances.
It forbids unsafe code.

## Two coordinate systems

`GridPosition` is relative to one grid.
`ComposedPosition` is relative to the final combined screen.

For a grid whose screen origin is `(5, 10)`, local position `(2, 3)` becomes
screen position `(7, 13)`:

```rust
let origin = ComposedPosition::new(5, 10);
let local = GridPosition::new(2, 3);
let screen = origin.checked_add_local(local);
```

`screen` is `Some(ComposedPosition::new(7, 13))`.

Both types use `Position<Space>` internally. `PhantomData` distinguishes the
coordinate system at compile time without adding stored runtime data.
The marker identifies the coordinate system, not a particular grid.

## Rectangles, sizes, and movement

| Type | Meaning |
| --- | --- |
| `GridRegion` | Rectangle within a grid |
| `ComposedRegion` | Rectangle in the combined screen |
| `GridSize` | Height and width |
| `GridDelta` | Signed row and column displacement |

Regions use half-open ranges: `2..5` includes 2, 3, and 4.

`GridSize::try_new()` rejects negative dimensions; zero dimensions are valid.
Region construction rejects reversed ranges and accepts empty ranges.
Translation methods return `None` on integer overflow.

Position construction preserves signed coordinates, and region construction
can accept negative boundaries. These types do not check whether coordinates
lie inside a particular grid; callers validate or clip against its dimensions.
For example, a negative composed position can describe a grid placed partly
outside the visible screen.

Tests cover translations, reversed ranges, negative sizes, and overflow.
