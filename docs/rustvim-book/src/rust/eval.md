# eval/mod.rs

Source: `src/nvim/rustvim_core/src/eval/mod.rs`.

This module defines script-context metadata: which script or API client
originated an action. For an option, this records where it was last set.

## Script context

`ScriptID` is an alias for `i32`. `SCtx` groups it with the other origin fields:

| Field | Meaning |
| --- | --- |
| `script_id` | Script identifier |
| `source_seq_num` | Sourcing sequence number, distinguishing separate script executions |
| `line_num` | Script line number; when stored with an option, the line where it was set |
| `chan` | API channel identifier, used when the script identifier is `SID_API_CLIENT` |

## C representation

`CSCtx` has `#[repr(C)]` and mirrors `sctx_T` from
`src/nvim/eval/typval_defs.h`. Its `sc_sid`, `sc_seq`, `sc_lnum`, and `sc_chan`
fields carry the same information using the C field names and compatible
types. `SCtx` has Rust's default layout.

## Current use

`SCtx` appears as the `script_ctx` field of `VimOption` in `option.rs`.
`VimOption` itself is currently only a type declaration, without consumers.
`CSCtx` also has no consumers elsewhere in the Rust sources.

This module contains no functions, C exports, or conversions between the two
representations. Vimscript evaluation and the active option handling remain
in C.
