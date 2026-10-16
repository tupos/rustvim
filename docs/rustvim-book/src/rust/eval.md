# eval/mod.rs

Source: `src/nvim/rustvim_core/src/eval/mod.rs`.

This file describes where a script action originated, such as the script and
line that last set an option.

It defines:

- `SCtx`: Rust fields for the script ID, sourcing sequence, line number, and
  API channel.
- `CSCtx`: the C-compatible representation corresponding to C's `sctx_T`.

At the investigated revision, `SCtx` is referenced by `VimOption`.
`CSCtx` has no usages elsewhere in the Rust sources.

There are no functions here. Vimscript evaluation still happens in C.
