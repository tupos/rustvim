# lib.rs

Source: `src/nvim/rustvim_core/src/lib.rs`.

This is the root of the `rustvim_core` library. It declares which modules
belong to that library.

For example, `mod context;` includes `context.rs`, while `pub mod grid;`
includes `grid.rs` and makes that module accessible to other Rust crates.
A directory module such as `eval` is loaded from `eval/mod.rs`.

Rust module visibility is separate from exporting functions for C.
The individual C-callable functions declare their C ABI and exported names.

Two attributes apply to the entire crate:

- `allow(dead_code)` suppresses warnings about unused code.
- `allow(static_mut_refs)` suppresses the lint against references to mutable
  globals. It does not make unsafe access safe.

This file contains no startup procedure.
C's `main()` calls `rustvim_context_register()` from `context.rs`.
