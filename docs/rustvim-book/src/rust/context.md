# context.rs

Source: `src/nvim/rustvim_core/src/context.rs`.

This file keeps the long-lived Rust rendering state together and coordinates
its startup and cleanup.

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
`OnceLock<RustVimContext>`.
`registered_context()` returns a reference to it.

## Creation, initialization, and cleanup

1. C's `main()` calls `rustvim_context_register()`. It constructs the subsystem
   objects and installs the context. A second registration returns `false`;
   `main()` treats that as an error.
2. C's `ui_init()` calls `rustvim_rendering_initialize()`. It registers the
   default grid first, then initializes compositor layers using that grid.
3. C's explicit memory-cleanup path calls `rustvim_rendering_free_all_mem()`.
   It releases grid, UI, and compositor resources, in that order.

Cleanup releases the resources without removing the registered context.
Initialization and cleanup errors are collected in `RenderingSystemError`;
the C-facing wrappers panic if those operations fail.

## Access to the subsystems

`grid()`, `compositor()`, and `ui()` return references to the corresponding
fields.

`message_surface()`, `popup_menu_surface()`, and `window_surface()` construct
temporary objects borrowing both the grid and compositor. Those objects
coordinate operations involving both subsystems.

In unit tests, `registered_context()` can construct the context lazily because
the test executable does not run C's `main()`. This does not itself initialize
rendering.

The lifecycle test explicitly initializes a context, checks the default grid
and compositor layer, releases resources, and checks that they are gone.
