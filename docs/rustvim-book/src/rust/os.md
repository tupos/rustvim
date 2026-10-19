# rust_os

Source directory: `src/nvim/rust_os/`.

This crate provides OS helpers shared by Rust code and the remaining C
implementation. It resolves and caches the home directory, builds application
state paths, and extracts path tails.

The implementation uses owned Rust paths and strings. Native callers reach
the same implementation through the adapters in `ffi.rs`.

## Modules

`lib.rs` exposes four modules:

| Module | Responsibility |
| --- | --- |
| `env.rs` | Home-directory resolution and the shared home cache |
| `stdpaths.rs` | Application state directories and state subpaths |
| `path.rs` | Path-tail extraction with platform-specific separator rules |
| `ffi.rs` | C entry points, pointer conversion, and native allocation |

The first three modules use `#![forbid(unsafe_code)]`.
Raw-pointer access and calls to the native allocator belong to `ffi.rs`.

## Resolving the home directory

`env::home_directory()` resolves the current home without changing the cache.
A nonempty `HOME` takes precedence on both platforms.

On Unix, resolution falls back to `std::env::home_dir()`, then the current
working directory. The selected path is canonicalized when possible, resolving
symbolic links without changing the process's working directory. If
canonicalization fails, the selected path is retained.

On Windows, resolution falls back to `HOMEDRIVE` combined with `HOMEPATH`,
then `std::env::home_dir()`. An empty or missing `HOMEPATH` contributes `/`.
A leading `%VARIABLE%` reference in the selected value is expanded once.
If no home is available, the result is `C:/`.
Backslashes become forward slashes, and drive-relative paths retain their
meaning without canonicalization.

## The shared home cache

`HomeDirectoryCache` owns two representations of the same home:

```rust
pub(crate) struct HomeDirectoryCache {
    path: Option<PathBuf>,
    native: Option<CString>,
}
```

A single `Mutex<HomeDirectoryCache>` protects replacement of both values.

| Rust function | Effect |
| --- | --- |
| `env::initialize_home_directory()` | Resolves home and replaces the cache |
| `env::cached_home_directory()` | Returns an owned path snapshot, or `None` before initialization |
| `env::clear_home_directory()` | Clears the cached path and native string |

C startup calls `init_homedir()` before logging initialization. Assigning
`HOME` through Neovim calls it again. Unsetting `HOME` retains the cached
value, so existing defaults and tilde expansion keep using the previous home.

Rust readers clone the path while holding the mutex. Their snapshots remain
valid after the cache changes.

C readers borrow the cached `CString`. Its pointer remains valid until the
next initialization or clear. Native callers must finish using borrowed
pointers before an update and must neither modify nor free the string.
The mutex protects cache access; the native caller controls the lifetime of
a pointer after the adapter returns.

The Rust path retains Unix filename bytes or Windows UTF-16 units.
The C representation preserves Unix bytes and uses lossy UTF-8 conversion on
Windows.

## Application state paths

`stdpaths::state_directory(home)` takes an optional cached home and reads the
other environment values on each call. Supplying home explicitly preserves
the cache's behavior when `HOME` has been unset.

The state-directory base is selected as follows:

| Platform | Base selection, in priority order |
| --- | --- |
| Unix | `XDG_STATE_HOME`, otherwise cached home plus `.local/state` |
| Windows | Nonempty `XDG_STATE_HOME`, nonempty `LOCALAPPDATA`, otherwise cached home plus `AppData/Local` |

An explicitly empty `XDG_STATE_HOME` on Unix produces a relative application
path. Windows ignores empty values. When no cached home is supplied, the
home-based fallback uses the literal `~`.

The application name is `NVIM_APPNAME`, or `nvim` when missing or empty.
Windows appends `-data` to that name.
Environment values are used literally, including embedded variable references
and tildes. Windows separators are normalized while preserving native UTF-16
units.

`stdpaths::user_state_subpath(home, name, trailing_separators, escape_commas)`
appends a name to that directory. Textual concatenation preserves the base
even when the name starts with a separator, matching the C helper.
It can escape each comma with a backslash and append an exact number of
forward slashes. These flags preserve the representations used by native
path options.

These helpers construct paths. Callers create directories and open files.

## Path tails

`path::tail(&str)` borrows the text after the last platform separator.
A trailing separator produces an empty tail, and dot components stay literal:
`dir/.` yields `.`, while `dir/..` yields `..`.

Unix recognizes only `/`; backslashes and colons remain filename characters.
Windows recognizes both slash forms and skips an initial drive prefix and
leading separators, including colons. For example, `C:server` yields
`server` on Windows and stays `C:server` on Unix.

Tracing uses this helper for parent and server names in log headers.

## C integration and ownership

The declarations live in `src/nvim/os/os.h`.

| C entry point | Operation and ownership |
| --- | --- |
| `init_homedir()` | Replaces the Rust-owned cache |
| `free_homedir()` | Clears the cache during explicit memory cleanup |
| `rustvim_cached_homedir()` | Returns a borrowed cache pointer, or `NULL` |
| `rustvim_state_directory()` | Returns a newly allocated path, released with `xfree()` |
| `stdpaths_user_state_subpath()` | Returns a newly allocated subpath, released with `xfree()` |

Allocated return strings use Neovim's `xmalloc()`, keeping allocation and
deallocation compatible with native callers. A null name passed to
`stdpaths_user_state_subpath()` produces a null result.

C's `os_homedir()` still reports an error when the cache is uninitialized.
Its successful result is the borrowed Rust-owned string.
`get_xdg_home(kXDGStateHome)` delegates to the Rust state-directory adapter;
the other XDG directory cases remain in C.

The final static-library crate, `rustvim_lib`, reexports `rust_os`.
`rustvim_tracing` depends on it directly for the cached home, default log
path, and path-tail helper. Those Rust calls use the safe modules directly.

## Tests

From the repository root:

```sh
cargo test -p rust_os
```

Resolution tests supply environment values and system-home results to internal
helpers. Cache tests use local cache instances, allowing deterministic parallel
runs.

Coverage includes home precedence, cache replacement and snapshot lifetime,
native string stability, state-directory defaults, comma escaping, trailing
separators, path encoding, and platform-specific path-tail rules.
