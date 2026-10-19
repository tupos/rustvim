# AGENTS.md

## AI Disclosure

If AI was used for a commit, add a generic `AI-assisted` trailer to the commit message.
Do NOT add any info mentioning the particular model or the AI service.
Do NOT list an AI service as the author name/email.

## Review and Editing

- Before modifying any tracked file, show the complete proposed patch.
- Never truncate a patch or use editorial `...` to omit code.
- Explain the purpose and effect of every diff chunk.
- Wait for explicit user approval before applying a patch.
- Apply exactly the approved patch. If it needs to change, show the revised
  complete patch and request approval again.
- Use `apply_patch` to modify files.
- Apply patch directly after approval, do not apply them at the end. After all
  diff chunks were reviewed.

## Rust

- Rust formatting may be applied in place without showing the formatting-only
  patch first.
- Format the complete Rust workspace with `make formatrust`. Do not invoke
  `cargo fmt` directly because the stable project toolchain ignores the
  nightly import-grouping options in `.rustfmt.toml`.
- Cargo may update the generated `Cargo.lock` file in place without showing
  its patch first. Show the resulting lockfile diff after Cargo finishes.
  This exception does not apply to Cargo manifests.

### C-to-Rust Migration

- Preserve observable Vim/Neovim behavior and C ABI compatibility throughout
  the migration.
- Before migrating code, compare the current C implementation, declarations,
  callers, data layouts, and relevant tests with the Rust implementation.
- Use a two-stage migration process:
  1. First establish a complete, self-contained Rust implementation of the
     existing behavior. Follow the integration approach used by `grid.rs`, but
     do not mechanically reproduce C patterns when a component can already be
     isolated cleanly.
  2. After behavior parity and integration are verified, refactor remaining
     transitional C-shaped internals into idiomatic safe Rust and eliminate
     unsafe code from the implementation logic.
- For isolated components such as `LineBuffer`, prefer owned Rust types,
  references and slices, RAII, and invariants enforced by constructors and
  methods from the outset.
- Keep unavoidable unsafe code confined to minimal `extern "C"` boundary
  wrappers. These wrappers should validate raw inputs where possible, convert
  C representations into safe Rust types, call safe Rust functions, and
  convert results back for C.
- Keep C type aliases such as `Integer` confined to FFI declarations,
  `#[repr(C)]` representations, and explicit conversions at FFI boundaries.
  Safe Rust implementation types and logic must use native Rust types instead
  of C aliases.
- Every unsafe function or block must have a specific safety rationale. Public
  unsafe functions must document their caller requirements in a `# Safety`
  section.
- Add focused compatibility and behavior tests before removing the
  corresponding C implementation.

## Python

- Add explicit type annotations to new or modified Python code, including
  parameters and return types.
- Follow the existing repository formatting and typing conventions.

## Verification

- When a native build is required, use GCC 15 (`gcc-15` and `g++-15`)
  or `gcc16` if available.
- When running parallel builds, use `-j$(nproc)` instead of a fixed job
  count.
- Run tests focused on the changed behavior.
- Run a focused functional test from the repository root with:

  ```sh
  functional_test_log=$(mktemp /tmp/rustvim-functionaltest.XXXXXX)
  NVIM_RPLUGIN_MANIFEST="$PWD/build/Xtest_rplugin_manifest" \
    make functionaltest TEST_FILE=test/functional/ui/multibyte_spec.lua \
    >"$functional_test_log" 2>&1
  functional_test_status=$?
  if [ "$functional_test_status" -eq 0 ]; then
    rg 'tests from .* ran|PASSED|SKIPPED TESTS' "$functional_test_log"
  else
    rg -n 'FAILED|ERROR|Tests exited' "$functional_test_log"
    tail -n 200 "$functional_test_log"
  fi
  rm -f "$functional_test_log"
  exit "$functional_test_status"
  ```

  Replace `TEST_FILE` with the relevant test file. The explicit
  `NVIM_RPLUGIN_MANIFEST` avoids a cleanup error in `cmake/RunTests.cmake`
  when the variable is empty. Functional tests must run outside the sandbox
  because the Neovim test processes require local IPC sockets. Always capture
  verbose functional-test output in a temporary log and extract concise
  summaries on success. On failure, show the relevant errors and enough
  trailing output to diagnose them while preserving the original exit status.
- When several focused functional suites are required, use this helper and
  call it once per test file. Pass an empty second argument to run the complete
  file or a filter string to select matching tests:

  ```sh
  run_focused_test() {
    functional_test_file=$1
    functional_test_filter=${2-}
    functional_test_log=$(mktemp /tmp/rustvim-functionaltest.XXXXXX)

    TEST_FILTER="$functional_test_filter" \
      NVIM_RPLUGIN_MANIFEST="$PWD/build/Xtest_rplugin_manifest" \
      make functionaltest TEST_FILE="$functional_test_file" \
      >"$functional_test_log" 2>&1
    functional_test_status=$?

    if [ "$functional_test_status" -eq 0 ]; then
      rg 'tests from .* ran|PASSED|SKIPPED TESTS' "$functional_test_log"
    else
      rg -n 'FAILED|ERROR|Tests exited' "$functional_test_log"
      tail -n 200 "$functional_test_log"
    fi

    rm -f "$functional_test_log"
    return "$functional_test_status"
  }

  run_focused_test test/functional/ui/syntax_conceal_spec.lua ''
  run_focused_test test/functional/ui/decorations_spec.lua conceal
  ```

  Run these commands outside the sandbox for local IPC support. Stop the batch
  and report the captured diagnostics when any invocation returns failure.
- Run `git diff --check`.
- Report test failures and environmental limitations clearly.

## Git

- Do not create or amend a commit without an explicit user request.
- Before committing or amending, show the complete proposed commit message
  and wait for explicit approval.
- Wrap commit-message bodies at no more than 80 columns.
- `AGENTS.md` is local working guidance and does not need to be committed.
  Do not include it in a commit unless explicitly requested.

## Editor Guidance

- When providing Neovim Git-review instructions, assume the user uses
  vim-fugitive.
