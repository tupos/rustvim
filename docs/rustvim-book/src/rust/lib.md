# lib.rs

Source: `src/nvim/rustvim_core/src/lib.rs`.

This is the root of the `rustvim_core` library. It declares which modules
belong to that library.

## Modules and C exports

`mod context;` includes `context.rs` as a private module.
`pub mod grid;` includes `grid.rs` and lets other Rust crates access its
public items. A directory module such as `eval` is loaded from `eval/mod.rs`.

These declarations control visibility to Rust callers. C entry points
declare `extern "C"` and use `#[unsafe(no_mangle)]` to preserve their exported
names. A function inside a private Rust module can therefore be called from
C, as `rustvim_context_register()` is.

## Crate-wide lints

Two attributes apply to the entire crate:

- `allow(dead_code)` suppresses warnings about unused code.
- `allow(static_mut_refs)` permits references to mutable statics without
  reporting that lint. It does not make those references safe.

## Unit-test linkage

The crate root also contains:

```rust
#[cfg(test)]
extern crate rustvim_tracing as _;
```

Unit-test executables link `libnvim` directly and need its logging and OS
entry points. This declaration ensures that `rustvim_tracing`, and its
`rust_os` dependency, participate in linking. The `_` avoids introducing a
name for Rust code to use. `#[cfg(test)]` limits the declaration to test
builds.

## Startup

`lib.rs` declares modules and compiler settings; it runs no initialization.
C's `main()` calls `rustvim_context_register()` to create the runtime context.
Its ownership and lifecycle are described in [context.rs](context.md).
