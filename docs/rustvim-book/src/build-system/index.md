# Build system

Rustvim's build combines C and Rust code with generated source files and
runtime files. Three tools have different responsibilities:

| Tool | Responsibility |
| --- | --- |
| Make | Provides commands such as `make`, `make rust-toolchains`, and `make wasmtest`. |
| CMake | Configures dependencies, generators, C compilation, Cargo invocations, and the final link. |
| Cargo | Compiles the Rust workspace and produces the Rust static library used by CMake. |

For a native build, run these commands from the repository root:

```sh
make rust-toolchains
make -j"$(nproc)"
```

The default build uses `.deps` for bundled third-party dependencies and
`build` for generated files and editor binaries. The native executable is
`build/bin/nvim`. CMake records its configuration in `build/CMakeCache.txt`.

The stable compiler is selected by `rust-toolchain.toml`. The Makefile
selects one nightly toolchain for formatting and the WASM build.
`make rust-toolchains` installs both.

A cross build also needs programs that run on the build machine. Lua and a
native editor execute generators; the resulting editor is compiled for the
target platform. The [WASM build](wasm.md) explains this separation and the
browser runtime that uses the generated module.
