# rustvim_tracing

Source directory: `src/nvim/rustvim_tracing/`.

This crate sends Rust tracing events and native Neovim log messages through
one Rust logger. It owns severity filtering, destination selection, reusable
message storage, instance names, record formatting, and stack capture.

The crate root reexports `tracing::Level` and the tracing macros.
`initialize(logger)` installs the subscriber and transfers ownership of the
logger to it.

## Modules

| Module | Responsibility |
| --- | --- |
| `buffer.rs` | Reusable storage for native printf formatting |
| `logger.rs` | Destination policy, severity, recursion guards, output locking, and backtraces |
| `writer.rs` | UTF-8 record formatting and synchronous writes |
| `layer.rs` | Subscriber ownership, event filtering, and structured-field formatting |
| `ffi.rs` | Native entry points, metadata, printf callbacks, and stream adapters |

The buffer, logger, writer, and layer modules use
`#![forbid(unsafe_code)]`. Native pointers and platform stream conversions
are handled in `ffi.rs`.

## Startup and destination selection

C startup calls `log_init()` after home-directory and option initialization.
The Rust entry point reads `NVIM_LOG_FILE` with `std::env::var_os()` and uses
`rust_os` to construct the default `logs/nvim.log` state subpath.

`Logger::initialize()` tries these destinations in order:

1. The requested `NVIM_LOG_FILE`.
2. The default state-directory log path.
3. `nvim.log` in the current working directory.
4. Standard error.

An empty requested value or the literal `$NVIM_LOG_FILE` selects the default.
Other variable references and tildes remain literal. Windows backslashes are
normalized to forward slashes.

Probing append access may create a file. Before trying the default path,
initialization creates its parent directories, requesting mode `0700` on
Unix. Failed preferred paths are retained for startup diagnostics.
The adapter publishes that retained path as `__NVIM_LOG_FILE_WANT` and a
selected fallback file as `NVIM_LOG_FILE`.

The subscriber is installed once. `NvimLogLayer` owns the `Logger`; native
adapters borrow it through the current tracing dispatcher for a synchronous
operation. `rustvim_lib` reexports the crate into the final static library.

## Logger state and record types

`Logger` keeps the destination and startup diagnostics alongside:

- An atomic minimum severity.
- A set of active thread IDs for recursion detection.
- A mutex-protected `LogBuffer` for native formatting.
- A mutex-protected `LogState` containing the cached instance name and reusable
  record bytes.
- An atomic flag recording whether recursion has already been reported.

Three types describe the inputs and the completed entry:

| Type | Contents |
| --- | --- |
| `LogMetadata` | Owned runtime context: timestamp, UI status, parent/server names, and process ID |
| `LogMessage` | Borrowed message inputs: severity, context, source location, payload, and newline flag |
| `LogRecord` | The completed entry, including the final instance name, ready for the writer |

The logger combines metadata and a message into a record.
Record text is borrowed only for the synchronous write.

## Severity filtering

The minimum severity is an `AtomicI32` owned by each logger.
`enabled()` checks it before formatting; `prepare()` also checks it before
reserving the current thread.

| Native threshold | Accepted messages |
| --- | --- |
| `0` | Every tracing severity |
| `1` | DEBUG and TRACE, plus higher severities |
| `2` | INFO, WARN, and ERROR |
| `3` | WARN and ERROR |
| `4` | ERROR |
| Above `4` | None |

The initial minimum is WARN. Native debug startup calls `log_set_level(0)`
immediately after `log_init()`.
Native `DLOG` and `ILOG` macros are also controlled by `NVIM_LOG_DEBUG` at
compile time.

`Logger::set_minimum_level()` atomically replaces the threshold and returns
the previous value. The C entry point `log_set_level()` exposes that operation.
Lua's `vim._with({ log_level = ... }, callback)` saves and restores the level,
including nested scopes and callback errors.

The tracing layer returns `Interest::sometimes()` for callsites, keeping
their filtering responsive to threshold changes.

## Recursion and output ownership

`prepare()` returns a `LogGuard` after inserting the current thread ID into
the logger's active set. Another attempt from the same thread is rejected
before it can reuse the formatting buffer or acquire the output lock.
Other threads may reserve the same logger.

The active-set mutex is released before formatting callbacks run.
The buffer and output state have separate mutexes.
Dropping the guard removes the thread ID on normal returns, errors, and
panic unwinding. A `PhantomData<Rc<()>>` marker keeps it on its creating
thread.

The FFI adapter counts recursion rejections in `g_stats.log_skip` and
schedules the first `E5430` recursion diagnostic for each logger.
Native `logmsg()` attempts before subscriber installation also increment
the skipped counter.

## Native printf formatting

The variadic `logmsg()` function remains in `src/nvim/log.c`.
It passes the live `va_list` and a formatting callback to
`rustvim_logmsg()`. Each callback attempt uses `va_copy()` before calling
`vsnprintf()`, so retries leave the enclosing argument list intact.

Rust filters the message and reserves its thread before invoking that callback.
`LogGuard::format_message()` lends initialized Rust-owned bytes to C and
consumes the completed message synchronously.

`LogBuffer` starts with 1024 writable bytes, including space for the
terminating NUL. This is an initial allocation size.
When `vsnprintf()` reports a larger required length, the buffer grows to
that length plus one and formatting is retried. The allocation is retained
for later messages.

The buffer validates the reported length and terminator.
Invalid UTF-8 becomes replacement characters; embedded NULs within the
counted payload are preserved. Formatting reservations are released after
failure or panic unwinding.

The native callback borrows the buffer and arguments only for the call.
`logmsg()` returns `false` for filtered or rejected attempts and for
formatting, metadata, or output failures.

## Rust events and structured fields

For example, `rustvim_tracing::warn!(row = 3, "window-line rendering failed")`
produces the payload `window-line rendering failed row=3`.

`NvimLogLayer` checks the logger's threshold before dispatch.
The event adapter reserves a `LogGuard` before visiting fields, so a field's
`Debug` implementation cannot recursively reuse the logger.

`EventFields` separates the `message` field from other fields.
It emits the message first, followed by space-separated `name=value` fields
formatted with `Debug`. The event target supplies the context prefix;
file or module metadata and a line number supply the source location.
Embedded NULs in event text become the literal escape `\0`.

The current layer formats events. Span creation, entry, and exit do not emit
their own log records.

## Instance names and output

The FFI adapter builds `LogMetadata` from local time, the process ID, the
`NVIM` environment variable, `v:servername`, and the native UI-client flag.
Parent and server path tails are extracted through `rust_os::path::tail()`.

Parent names take precedence over server names:

| Available identity | Normal instance | UI client |
| --- | --- | --- |
| Parent | `c/<parent>` | `ui/c/<parent>` |
| Server | `<server>` | `ui/<server>` |
| Neither | `?.<pid>` | `ui.<pid>` |

Assigned non-UI names are cached. Provisional names and UI names are refreshed
from later metadata.

`writer::write_record()` emits the severity label, timestamp, instance name,
context/source location, and message. TRACE shares the `DBG` label.
Instance names are padded to a minimum of ten bytes and emitted in full.
The newline flag controls the final linefeed.

The logger assembles each entry in reusable record storage, writes it, and
flushes the destination. Partial and interrupted writes are retried, and
write/flush errors propagate as `io::Result`.

Each ordinary write reopens the selected log path in append mode, supporting
log rotation. An open failure writes a diagnostic directly to standard error
and uses standard error for that operation.

## Diagnostics and the native boundary

Stack and libuv diagnostics use `prepare_unfiltered()`, retaining recursion
protection while bypassing the severity threshold.

Stack capture uses `std::backtrace::Backtrace::force_capture()` before taking
the output lock. It captures regardless of `RUST_BACKTRACE` and
`RUST_LIB_BACKTRACE`; symbol names and source locations depend on available
unwind and debug information. C frames can appear in the same stack.
The implementation performs no external `addr2line` command invocation.

The C stack wrappers select the normal destination or supply a `FILE *`.
Regular supplied streams transfer ownership; standard streams are flushed
and retained. Libuv handle diagnostics use its native printer while the
logger holds the output lock.

The special `Logger::fatal()` path writes a UTF-8 cleanup diagnostic to
standard error and aborts, independently of normal output state.

`ffi.rs` still adapts native calendar time, `v:servername`, the UI-client
flag, skipped-log statistics, scheduled errors, environment publication, and
native streams. Declarations for the C logging entry points live in
`src/nvim/log.h`.

## Tests

From the repository root:

```sh
cargo test -p rustvim_tracing
```

Rust tests cover reusable buffers, long messages, UTF-8 conversion, filtering,
nested level restoration, independent loggers, recursion, panic unwinding,
record formatting, write failures, and backtrace output.

`test/functional/core/log_spec.lua` exercises the native ABI, Lua severity
scopes, printf arguments, log rotation, UI/child names, and C stack frames.
