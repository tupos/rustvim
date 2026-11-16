# WASM build

The WASM build compiles the C and Rust editor to WebAssembly and packages it
with a JavaScript loader, runtime files, and a browser frontend.
Emscripten supplies the compiler, C runtime, virtual filesystem, and
JavaScript integration.

## Build and run

Install and activate the Emscripten SDK in `.deps/emsdk`, or set `EMSDK` to
its directory. Run from the repository root:

```sh
make rust-toolchains
make wasm
make wasmtest
python3 build/wasm/site/serve.py
```

Open `http://localhost:8002` for the browser frontend. The build script
sources `emsdk_env.sh` itself, so the SDK need not be on the invoking shell's
`PATH`.

`make wasm` builds and packages the editor. `make wasmtest` also executes
the packaged editor under the SDK's Node.js. Neither command installs the
SDK or Rust toolchains.

## Build stages and directories

The Makefile passes its nightly toolchain selection to
`.github/scripts/build_wasm.sh`. The script performs these stages:

1. Build the native dependencies and editor.
2. Load the Emscripten SDK.
3. Cross-compile the dependencies.
4. Configure the editor with native generator paths and the WASM target.
5. Build the browser package and, when requested, run its smoke test.

The native and target outputs are separate:

```text
.deps/usr/                  Native dependencies
.deps/emsdk/                Emscripten SDK, when installed here
.deps/wasm/usr/             WASM dependencies and static parser archives
build/bin/nvim             Native editor used by generators
build/host-tools-Release.cmake
                           Native generator paths for a Release host build
build/wasm/                WASM CMake build and Cargo output
build/wasm/site/           Browser package
```

The native build can use another configuration. Its generated host-tools
filename contains that configuration instead of `Release`.

## Why native tools are needed

The build executes Lua generators and uses Neovim to generate help tags.
Those programs must run on the build machine while the target editor is
being compiled.

The native CMake configuration writes their actual paths into
`host-tools-<configuration>.cmake`. For example:

```cmake
set(NVIM_HOST_PRG "$<TARGET_FILE:nvim_bin>" CACHE FILEPATH "Native editor" FORCE)
```

This line is part of CMake's generated file content. CMake replaces the
target expression with the native executable's path.

The cross-build command loads that file with `cmake -C`. It supplies
`LUA_PRG`, `LUA_GEN_PRG`, `NLUA0_HOST_PRG`, and `NVIM_HOST_PRG`.
Runtime-generation dependencies refer to the native editor, so generating
files does not require executing the WASM editor.

## Cross-compiling dependencies

The build script configures a separate dependency tree:

```sh
emcmake cmake -S cmake.deps -B .deps/wasm -G Ninja \
  -DUSE_BUNDLED_LUAJIT=OFF -DUSE_BUNDLED_LUA=ON \
  -DUSE_BUNDLED_TS_PARSERS=ON -DCMAKE_C_FLAGS=-pthread
cmake --build .deps/wasm -j"$(nproc)"
```

`emcmake` selects Emscripten's CMake toolchain. This build uses Lua instead
of LuaJIT and compiles the Tree-sitter parsers as static archives. The
`-pthread` flag makes the dependency objects compatible with the editor's
shared-memory build.

The dependency CMake files also supply libuv's required POSIX helper
sources and forward the target compiler flags to Lua's Makefile.

## Compiling Rust and linking the editor

The cross build selects `wasm32-unknown-emscripten` as Cargo's target.
It uses the nightly selected in the Makefile, with:

```text
-Zbuild-std=std,panic_unwind
-Ctarget-feature=+atomics,+bulk-memory,+mutable-globals
```

`-Zbuild-std` rebuilds Rust's standard library with the selected target
features. This requires nightly and its `rust-src` component.
The toolchain installation rule also installs `rustfmt` and the Emscripten
target. Host support remains available in the same nightly installation.

Cargo still executes native build scripts and procedural macros during a
cross build. On Linux GNU hosts, `scripts/cargo-linker.sh` selects a native
C compiler for these host programs when the CMake target is Emscripten.

Cargo produces `librustvim_lib.a`. CMake links it with the editor's C
objects, C dependencies, and static parsers through Emscripten.
The `static:-bundle` Cargo link modifier keeps C dependencies outside the
Rust archive, leaving them as separate inputs to the final link.

`cmake/Wasm.cmake` configures the browser module. Its important options are:

| Option | Effect |
| --- | --- |
| `-pthread` | Enables threading and shared-memory support. |
| `--no-entry`, `-sINVOKE_RUN=0` | Leaves startup under JavaScript's control through `nvim_main`. |
| `-sMODULARIZE=1`, `-sEXPORT_NAME=createNvim` | Provides the `createNvim()` JavaScript factory. |
| `EXPORTED_FUNCTIONS`, `EXPORTED_RUNTIME_METHODS` | Exposes selected C functions and JavaScript helpers. |
| `--preload-file=${WASM_RUNTIME_DIR}@/runtime` | Packages runtime files at the virtual path `/runtime`. |
| `-Wl,--fatal-warnings` | Rejects linker warnings, including C/Rust signature mismatches. |

Most settings were carried over from the previous browser build. The
8 MiB stack and 256 MiB initial memory are inherited choices, not measured
minimum requirements. The configuration also retains `-fexceptions`,
`EXIT_RUNTIME=0`, filesystem support, and the IDBFS backend.

The frontend currently does not mount IDBFS. Including that backend and
calling `FS.syncfs()` do not by themselves provide persistent storage.

## Runtime files and static parsers

The package combines checked-in `runtime/` files with generated syntax
files and help tags. Emscripten puts these in `nvim.data` and loads them into
its virtual filesystem under `/runtime`.

The eight bundled parsers are linked from archives such as:

```text
.deps/wasm/usr/lib/nvim/parser/lua.a
.deps/wasm/usr/lib/nvim/parser/c.a
```

`src/static_ts_registry.c` maps names to linked-in parser functions.
Lua's automatic parser discovery first asks the static loader:

```lua
if vim._ts_add_language_from_static and vim._ts_add_language_from_static(lang, symbol_name) then
  return true
end
```

The C binding reads the registry, checks the parser's Tree-sitter ABI
version, and registers it in the existing language map. An unknown name
returns `false`, allowing Lua to continue searching runtime files.

The `symbol_name` option can select a built-in parser under another
registered language name. For an unloaded language, an explicit
`opts.path` goes through the file loader instead.

The C initialization code also exposes registry names through
`vim._ts_static_languages`, which Lua includes in completion. Built-in
parser discovery requires no empty `.so` files.

## Browser package and worker

The generated package contains:

| File | Role |
| --- | --- |
| `nvim.wasm` | Compiled C and Rust editor. |
| `nvim.js` | Emscripten loader providing `createNvim()`. |
| `nvim.data` | Packaged runtime files. |
| `nvim-worker.js` | Starts the editor in a Web Worker and connects its input/output. |
| `rpc.js`, `msgpackrpc.js`, `msgpack.js` | Transport and MessagePack-RPC support. |
| `app.js`, `index.html`, `style.css` | Browser interface and display. |
| `serve.py` | Local HTTP server with shared-memory headers. |

`rpc.js` starts a Web Worker. The worker loads `nvim.js`, instantiates the
module, prepares the filesystem and input/output hooks, and calls
`nvim_main` with `--embed`.

The editor loop runs in the worker while the main browser thread handles
the interface. Input reaches the worker through a shared ring buffer;
output is sent back as messages and decoded as MessagePack-RPC. The browser
frontend applies Neovim's UI events to its displayed grid.

The worker sets:

```javascript
m.ENV.HOME = "/home/user";
m.ENV.VIMRUNTIME = "/runtime";
```

These paths belong to Emscripten's virtual filesystem. `/home/user` is an
arbitrary browser-side home directory. Setting `HOME` does not create it;
`FS.mkdirTree()` creates the directories used for configuration and data.

Shared memory requires browser cross-origin isolation. The supplied server
sends:

```text
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

Serving the files without appropriate headers can make
`SharedArrayBuffer` unavailable.

## What the smoke test checks

`make wasmtest` runs `test/wasm/smoke.cjs` against the packaged `nvim.js`.
It loads the actual WASM editor, checks shared memory, and executes
`test/wasm/smoke.lua` inside it.

The Lua checks cover Unicode editing, floating windows, Lua error
handling, all eight static parsers, completion, aliases, explicit paths,
and parsing a Lua syntax tree. They also require the runtime parser
directory to contain no parser files.

This tests the compiled editor under Node.js. Browser drawing, keyboard
input, and the worker's RPC transport still require browser testing.
