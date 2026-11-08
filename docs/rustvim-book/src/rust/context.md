# context.rs

Source: `src/nvim/rustvim_core/src/context.rs`.

`RustVimContext` owns the long-lived Rust rendering state. This file also
provides the C entry points that register the context, initialize rendering,
and release its resources.

```rust
pub(crate) struct RustVimContext {
    grid: GridContext,
    compositor: CompositorContext,
    ui: UiContext,
}
```

| Field | State owned by that subsystem |
| --- | --- |
| `grid` | Stored grids, the temporary row buffer, screen-character storage, message-grid state, and grid identifiers |
| `compositor` | Layer state, composition buffers, and debug highlights |
| `ui` | Outgoing-update buffers, pending cursor updates, and pending default-color updates |

There is one registered context for the process, stored in
`OnceLock<RustVimContext>`. `registered_context()` borrows it. In the editor,
calling this function before registration panics.

## Creation, initialization, and cleanup

Registration creates the subsystem objects; initialization connects their
state so rendering can use them.

1. C's `main()` calls `rustvim_context_register()`. It constructs the subsystem
   objects and installs the context. A second registration returns `false`;
   `main()` aborts in that case.
2. C's `ui_init()` calls `rustvim_rendering_initialize()`. It registers an empty
   default grid first, then initializes compositor layers using that grid.
3. C's `free_all_mem()` in `memory.c` calls
   `rustvim_rendering_free_all_mem()`. It releases grid, UI, and compositor
   resources, in that order. This explicit cleanup path is compiled under
   `EXITFREE`.

Cleanup releases the resources without removing the registered context.
Initialization and cleanup errors are collected in `RenderingSystemError`;
the C-facing wrappers panic if those operations fail.

## Access to the subsystems

`grid()`, `compositor()`, and `ui()` return references to the corresponding
fields. The context retains ownership; each subsystem manages access to its
mutable state through its internal locks and atomics.

`message_surface()`, `popup_menu_surface()`, and `window_surface()` construct
temporary objects borrowing both the grid and compositor. Construction stores
only those references; it does not copy grids or allocate rendering storage.
The objects coordinate operations involving both subsystems, such as updating
a popup menu's grid and its compositor placement.

## Unit tests

In unit tests, `registered_context()` can construct the context lazily because
the test executable does not run C's `main()`. This does not itself initialize
rendering.

The lifecycle test creates a separate local context. It initializes rendering,
checks the default grid and compositor layer, releases resources, and checks
that they are gone.
