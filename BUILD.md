# Building Rustvim

Rustvim contains both C and Rust code. CMake builds the editor and invokes
Cargo to compile and link its Rust components. The Makefile provides shortcuts
for configuring, building, testing, and installing.

Before upgrading, check the [upstream breaking changes](https://neovim.io/doc/user/news.html#news-breaking).

## Quick start

1. Install the [native build prerequisites](#build-prerequisites) and
   [Rust tooling](#rust-and-cargo) for your system.
2. `git clone https://github.com/tupos/rustvim.git`
3. `cd rustvim`
4. `make rust-toolchain` to install the Rust toolchain selected by the repository.
    - On OpenBSD, use the system Rust/Cargo package instead.
5. `make CMAKE_BUILD_TYPE=RelWithDebInfo`
    - To install to a custom location, set `CMAKE_INSTALL_PREFIX`. See also [INSTALL.md](./INSTALL.md#building-rustvim-from-source).
    - On BSD, use `gmake` instead of `make`.
    - To build on Windows, see [Building on Windows](#building-on-windows). MSVC (Visual Studio) is recommended.
6. `sudo make install`
    - Default install location is `/usr/local`.
    - On Debian/Ubuntu, you can instead try `cd build && cpack -G DEB && sudo dpkg -i nvim-linux-<arch>.deb`
      (with `<arch>` either `x86_64` or `arm64`). This is a best-effort packaging option.

**Notes**:

- Run the commands from the repository root.
- `make` downloads and builds dependencies and produces `build/bin/nvim`
  (`build/bin/nvim.exe` on Windows). The executable name remains `nvim`.
- Native dependencies (libuv, LuaJIT, etc.) are downloaded to `.deps/`.
  Cargo downloads Rust dependencies to its cache, normally under `~/.cargo/`,
  and CMake places Rust build output in `build/rust_target/`.
- CMake links the Rust implementation into the editor; installing a separate
  Rust library is not required to run it.
- Run without installing with `VIMRUNTIME=runtime ./build/bin/nvim`.
- Install [Ninja](https://ninja-build.org/) for faster builds; it is selected
  automatically when available.
- Install [ccache](https://ccache.dev/) for faster C rebuilds. It is used by
  default. To disable it, use `CCACHE_DISABLE=true make`.

## Development checks and tests

Install the nightly toolchain needed by the Rust formatter:

```bash
make rust-nightly-toolchain
```

Run the Rust checks from the repository root:

```bash
make formatrust
make lintrust
RUST_TEST_THREADS=1 make rusttest
```

`formatrust` formats the complete workspace with the nightly selected in the
Makefile. `lintrust` runs Clippy for all workspace targets, rejects warnings,
and uses `--keep-going` to check independent crates after an error.
`rusttest` builds the native libraries and runs the Rust workspace tests.
Use one test thread because some tests share process-wide editor state.

For a focused functional test:

```bash
NVIM_RPLUGIN_MANIFEST="$PWD/build/Xtest_rplugin_manifest" \
  make functionaltest TEST_FILE=test/functional/core/startup_spec.lua
```

Omit `TEST_FILE` to run the complete functional suite. For the other test
targets and filtering options, see [dev_test.txt](./runtime/doc/dev_test.txt).

### Running Cargo directly

Configure CMake and build the native dependencies first, for example with
`make CMAKE_BUILD_TYPE=Debug`. You can then run:

```bash
cargo test --workspace --locked --target-dir build/rust_target -- --test-threads=1
```

The Rust build script builds `libnvim` as needed and reads native dependency
paths from CMake's cache. On GNU/Linux, the Cargo linker wrapper also uses the
C compiler selected by CMake.

For a different configured build directory, set `RUSTVIM_BUILD_DIR`:

```bash
RUSTVIM_BUILD_DIR=build-clang \
  cargo test --workspace --locked --target-dir build-clang/rust_target -- --test-threads=1
```

With a multi-configuration CMake generator, also set
`RUSTVIM_CMAKE_CONFIG` to the configuration to build, such as `Debug`.

## Building

First make sure you installed the [build prerequisites](#build-prerequisites). Now that you have the dependencies, you can try other build targets explained below.

The _build type_ determines the level of used compiler optimizations and debug information:

- `Release`: Full compiler optimizations and no debug information. Expect the best performance from this build type. Often used by package maintainers.
- `Debug`: Full debug information; few optimizations. Use this for development to get meaningful output from debuggers like GDB or LLDB. This is the default if `CMAKE_BUILD_TYPE` is not specified.
- `RelWithDebInfo` ("Release With Debug Info"): Enables many optimizations and adds enough debug info so that when Rustvim crashes, you can still get a backtrace.

CMake selects the corresponding Cargo profile:

| CMake build type | Cargo profile with LTO disabled | Cargo profile with LTO enabled |
| --- | --- | --- |
| `Debug` | `dev` | `dev` |
| `Release` | `release` | `release-lto` |
| `RelWithDebInfo` | `release-debinfo` | `release-debinfo-lto` |
| `MinSizeRel` | `strip-release` | `strip-release-lto` |

LTO is controlled by `ENABLE_LTO` and is enabled by default except for MinGW.
The `strip-release` profiles strip symbols from an optimized release build.

For a release build, use:

```bash
make CMAKE_BUILD_TYPE=Release
```
(Do not add a `-j` flag if `ninja` is installed! The build will be in parallel automatically.)

Afterwards, the `nvim` executable can be found in `build/bin`. To verify the build type after compilation, run:

```bash
./build/bin/nvim --version
```

The output includes the Rust compiler, Cargo version, target, profile,
optimization level, debug-information setting, and target features.

To install the editor and runtime files to a custom location, use:

```bash
make CMAKE_BUILD_TYPE=RelWithDebInfo CMAKE_INSTALL_PREFIX="$HOME/local/rustvim"
make install
```

CMake caches its settings in `build/CMakeCache.txt`. Reconfigure explicitly
when changing the build type or install prefix:

```bash
cmake -S . -B build -DCMAKE_BUILD_TYPE=Debug \
  -DCMAKE_INSTALL_PREFIX="$HOME/local/rustvim"
cmake --build build
```

Use a fresh build directory when changing compilers or target platforms.
`make distclean` removes both `build/` and `.deps/`, including any Emscripten
SDK installed under `.deps/emsdk`.

By default (`USE_BUNDLED=1`), Rustvim downloads and statically links its native dependencies. In order to be able to use a debugger on these libraries, you might want to compile them with debug information as well:

<!-- THIS CAUSES SCREEN INTERFERENCE
```
make distclean
VERBOSE=1 DEBUG=1 make deps
```
-->
```bash
make distclean
make deps
```

### PUC Lua

To build with "PUC Lua" instead of LuaJit:
```bash
make CMAKE_EXTRA_FLAGS="-DPREFER_LUA=ON" DEPS_CMAKE_FLAGS="-DUSE_BUNDLED_LUAJIT=OFF -DUSE_BUNDLED_LUA=ON"
```

### Build options

View the full list of CMake options defined in this project:
```bash
cmake -B build -LH
```

## Building on Windows

### Windows / MSVC

**MSVC (Visual Studio) is the recommended way to build on Windows.**

Install [rustup](https://rustup.rs/) and ensure `rustup`, `rustc`, and `cargo`
are on `PATH` before starting the IDE or developer shell. From the repository
root, install the toolchain selected by `rust-toolchain.toml`:

```powershell
rustup toolchain install --no-self-update
```

Use an MSVC Rust host toolchain that matches the C build architecture.
Rustvim's release workflow builds x86-64 and ARM64.

1. Install [Visual Studio](https://visualstudio.microsoft.com/thank-you-downloading-visual-studio/?sku=Community) (2022 or later) with the _Desktop development with C++_ workload.
2. Open the Rustvim project folder.
    - Visual Studio should detect the cmake files and automatically start building...
3. Choose the `nvim.exe (bin\nvim.exe)` target and hit F5.
    - If the build fails, it may be because Visual Studio started the build with `x64-{Debug,Release}` before you switched the configuration to `x86-Release`.
      - Right-click _CMakeLists.txt → Delete Cache_.
      - Right-click _CMakeLists.txt → Generate Cache_.
    - If you see an "access violation" from `ntdll`, you can ignore it and continue.
4. If you see an error like `uv.dll not found`, try the `nvim.exe (Install)` target. Then switch back to `nvim.exe (bin\nvim.exe)`.

### Windows / MSVC PowerShell

To build from the command line (i.e. invoke the `cmake` commands yourself),

1. Ensure you have the Visual Studio environment variables, using any of the following:
    - Using the [Visual Studio Developer Command Prompt or Visual Studio Developer PowerShell](https://learn.microsoft.com/en-us/visualstudio/ide/reference/command-prompt-powershell?view=vs-2022)
    - Invoking `Import-VisualStudioVars` in PowerShell from [this PowerShell module](https://github.com/Pscx/Pscx)
    - Invoking `VsDevCmd.bat` in Command Prompt
      ```
      VsDevCmd.bat -arch=x64
      ```
   This is to make sure that `luarocks` finds the Visual Studio installation, and doesn't fall back to MinGW with errors like:
   ```
   'mingw32-gcc' is not recognized as an internal or external command
   ```
2. From the "Developer PowerShell" or "Developer Command Prompt":
   ```bash
   cmake -S cmake.deps -B .deps -G Ninja -D CMAKE_BUILD_TYPE=Release
   cmake --build .deps --config Release
   cmake -B build -G Ninja -D CMAKE_BUILD_TYPE=Release
   cmake --build build --config Release
   ```
    - With Ninja, select a debug build by using
      `-D CMAKE_BUILD_TYPE=Debug` when configuring `build`.
    - Omit `-G Ninja` to use the "Visual Studio" generator; select the build
      configuration with `--config Debug` or `--config Release`.
    - Run Rust tests with
      `cmake --build build --config Release --target rusttest`, with
      `RUST_TEST_THREADS` set to `1`.

### Windows / CLion

1. Install [CLion](https://www.jetbrains.com/clion/).
2. Open the Rustvim project in CLion with Cargo available on `PATH`.
3. Select _Build → Build All in 'Release'_.

### Windows / Cygwin

These inherited Neovim instructions have not been verified for Rustvim.
The Cygwin C environment also needs compatible Rust target support.

Since https://github.com/neovim/neovim/pull/36417 , building on Cygwin may be as easy as:
```bash
make && make install
```

If that fails, an alternative is:

1. Install all dependencies the normal way.
  - The `cygport` repo contains Cygport files (e.g. `APKBUILD`, `PKGBUILD`) for all the dependencies not available in the Cygwin distribution, and describes any special commands or arguments needed to build. The Cygport definitions also try to describe the required dependencies for each one. Unless custom commands are provided, Cygport just calls `autogen`/`cmake`, `make`, `make install`, etc. in a clean and consistent way.
  - https://github.com/cascent/neovim-cygwin was built on Cygwin 2.9.0. Newer `libuv` should require slightly less patching. Some SSP stuff changed in Cygwin 2.10.0, so that might change things too when building Neovim.
2. Build without "bundled" dependencies (except treesitter parsers).
   ```bash
   cmake -S cmake.deps -B .deps -G Ninja -D CMAKE_BUILD_TYPE=RelWithDebInfo -DUSE_BUNDLED=OFF -DUSE_BUNDLED_TS=ON
   cmake --build .deps
   cmake -B build -G Ninja -D CMAKE_BUILD_TYPE=RelWithDebInfo
   cmake --build build
   ```

### Windows / MSYS2 / MinGW

These inherited Neovim instructions have not been verified for Rustvim.
Install a Rust GNU toolchain matching the MinGW architecture; an MSVC Rust
toolchain is not interchangeable with it.

1. From the MSYS2 shell, install these packages:
   ```bash
   pacman -S \
       mingw-w64-ucrt-x86_64-gcc \
       mingw-w64-x86_64-{cmake,make,ninja,diffutils}
   ```
2. From the Windows Command Prompt (`cmd.exe`), set up the `PATH` and build.

   ```cmd
   set PATH=c:\msys64\ucrt64\bin;c:\msys64\usr\bin;%PATH%
   ```
3. You have two options:
    - Build using `cmake` and `Ninja` generator:
      ```cmd
      cmake -S cmake.deps -B .deps -G Ninja -D CMAKE_BUILD_TYPE=RelWithDebInfo
      cmake --build .deps
      cmake -B build -G Ninja -D CMAKE_BUILD_TYPE=RelWithDebInfo
      cmake --build build
      ```
      If you cannot install neovim with `ninja install` due to permission restriction, you can install neovim in a directory you have write access to.
      ```cmd
      cmake -S cmake.deps -B .deps -G Ninja -D CMAKE_BUILD_TYPE=RelWithDebInfo
      cmake --build .deps
      cmake -B build -G Ninja -D CMAKE_INSTALL_PREFIX=C:\nvim -D CMAKE_BUILD_TYPE=RelWithDebInfo
      cmake --build build
      ```
    - Or, alternatively, you can use `mingw32-make`:
      ```cmd
      mingw32-make deps
      mingw32-make CMAKE_BUILD_TYPE=RelWithDebInfo
      :: Or you can do the previous command specifying a custom prefix
      :: (Default is C:\Program Files (x86)\nvim)
      :: mingw32-make CMAKE_BUILD_TYPE=RelWithDebInfo CMAKE_INSTALL_PREFIX=C:\nvim
      mingw32-make install
      ```

### Windows WSL

Build Ubuntu/Debian linux binary on [WSL](https://learn.microsoft.com/en-us/windows/wsl/install) (Windows Subsystem for Linux).

```bash
# Install build prerequisites
sudo apt-get install ninja-build gettext cmake build-essential

# Install rustup inside WSL as described under "Rust and Cargo", then:
make rust-toolchain

# Build the linux binary in WSL
make CMAKE_BUILD_TYPE=RelWithDebInfo

# Install the linux binary in WSL (with `<arch>` either `x86_64` or `arm64`)
cd build && cpack -G DEB && sudo dpkg -i nvim-linux-<arch>.deb

# Verify the installation
nvim --version && which nvim # should be debug build in /usr/bin/nvim
```

**Note**: If you encounter linker errors or segfaults during the build, Windows libraries in your PATH may be interfering. Use a clean PATH to avoid conflicts:

```bash
PATH="/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin" make CMAKE_BUILD_TYPE=RelWithDebInfo
```

## Localization

### Localization build

Translations are turned off by default. Enable by building Nvim with the CMake flag `ENABLE_TRANSLATIONS=ON`.
Doing this will create `.mo` files in `build/src/nvim/po`. Example:

```bash
make CMAKE_EXTRA_FLAGS="-DENABLE_TRANSLATIONS=ON"
```

* If you see `msgfmt: command not found`, you need to install [`gettext`](http://en.wikipedia.org/wiki/Gettext). On most systems, the package is just called `gettext`.

### Localization check

To check the translations for `$LANG`, run `make -C build check-po-$LANG`. Examples:

```bash
cmake --build build --target check-po-de
cmake --build build --target check-po-pt_BR
```

- `check-po-$LANG` generates a detailed report in `./build/src/nvim/po/check-${LANG}.log`. (The report is generated by `nvim`, not by `msgfmt`.)

### Localization update

To update the `src/nvim/po/$LANG.po` file with the latest strings, run the following:

```bash
cmake --build build --target update-po-$LANG
```

- **Note**: Run `src/nvim/po/cleanup.vim` after updating.

## Compiler options

To see the chain of includes, use the `-H` option ([#918](https://github.com/neovim/neovim/issues/918)):

```bash
echo '#include "./src/nvim/buffer.h"' | \
> clang -I.deps/usr/include -Isrc -std=c99 -P -E -H - 2>&1 >/dev/null | \
> grep -v /usr/
```

- `grep -v /usr/` is used to filter out system header files.
- `-save-temps` can be added as well to see expanded macros or commented assembly.

## Custom Makefile

You can customize the build process locally by creating a `local.mk`, which is referenced at the top of the main `Makefile`. It's listed in `.gitignore`, so it can be used across branches. **A new target in `local.mk` overrides the default make-target.**

Here's a sample `local.mk` which adds a target to force a rebuild but *does not* override the default-target:

```make
all:

rebuild:
	rm -rf build
	make
```

## Third-party dependencies

Reference the [Debian package](https://packages.debian.org/sid/source/neovim) (or alternatively, the [Homebrew formula](https://github.com/Homebrew/homebrew-core/blob/master/Formula/n/neovim.rb)) for the precise list of dependencies/versions.

To build the bundled dependencies using CMake:

```bash
cmake -S cmake.deps -B .deps -G Ninja -D CMAKE_BUILD_TYPE=RelWithDebInfo
cmake --build .deps
```

By default the native libraries and headers are placed in `.deps/usr`.
With Rust installed, build Rustvim:

```bash
cmake -B build -G Ninja -D CMAKE_BUILD_TYPE=RelWithDebInfo
cmake --build build
```

### Build without "bundled" dependencies

These settings control native dependencies. Cargo still uses `Cargo.lock`
to select Rust dependencies.

1. Manually install the dependencies:
    - libuv libluv libutf8proc luajit lua-lpeg tree-sitter tree-sitter-c tree-sitter-lua tree-sitter-markdown tree-sitter-query tree-sitter-vim tree-sitter-vimdoc unibilium
2. Run CMake:
   ```bash
   cmake -B build -G Ninja -D CMAKE_BUILD_TYPE=RelWithDebInfo
   cmake --build build
   ```
   If all the dependencies are not available in the package, you can use only some of the bundled dependencies as follows (example of using `ninja`):
   ```bash
   cmake -S cmake.deps -B .deps -G Ninja -D CMAKE_BUILD_TYPE=RelWithDebInfo -DUSE_BUNDLED=OFF -DUSE_BUNDLED_TS=ON
   cmake --build .deps
   cmake -B build -G Ninja -D CMAKE_BUILD_TYPE=RelWithDebInfo
   cmake --build build
   ```
3. Run `make`, `ninja`, or whatever build tool you told CMake to generate.
    - Using `ninja` is strongly recommended.
4. If treesitter parsers are not bundled, they need to be available in a `parser/` runtime directory (e.g. `/usr/share/nvim/runtime/parser/`).

### Build offline

On systems with old or missing libraries, *without* network access, you may want
to build *with* bundled dependencies. This is supported as follows.

1. On a network-enabled machine, do either of the following, to get the
   dependency sources in `.deps` in a form that the build will work with:
    - Fetch https://github.com/neovim/deps/tree/master/src into `.deps/build/src/`.
      (https://github.com/neovim/deps/tree/master/src is an auto-updated, "cleaned up", snapshot of `.deps/build/src/`).
    - Run `make deps` to generate `.deps/`, then clean it up using [these commands](https://github.com/neovim/neovim/blob/1c12073db6c64eb365748f153f96be9b0fe61070/.github/workflows/build.yml#L67-L74).
2. On the network-enabled machine, install the required Rust toolchains and
   fetch the locked Rust dependencies:

   ```bash
   make rust-toolchain
   cargo fetch --locked
   ```

   Use `make rust-toolchains` instead if formatting or WASM builds are needed.
   Prepare for the same host and target platforms as the offline build.
3. Copy the prepared `.deps` and Cargo cache to the isolated machine.
   Cargo normally stores downloaded sources and archives under
   `${CARGO_HOME:-$HOME/.cargo}/registry` and any Git dependencies under
   `${CARGO_HOME:-$HOME/.cargo}/git`. The selected Rust toolchain must also
   be installed on that machine before it goes offline.
4. Build with `USE_EXISTING_SRC_DIR` enabled and Cargo offline:

   ```bash
   make deps DEPS_CMAKE_FLAGS=-DUSE_EXISTING_SRC_DIR=ON
   CARGO_NET_OFFLINE=true make
   ```

### Build without unibilium

Unibilium is the only dependency which is licensed under LGPLv3 (there are no
GPLv3-only dependencies). This library is used for loading the terminfo database at
runtime, and can be disabled if the internal definitions for common terminals
are good enough. To avoid this dependency, build with support for loading
custom terminfo at runtime, use

```bash
make CMAKE_EXTRA_FLAGS="-DENABLE_UNIBILIUM=0" DEPS_CMAKE_FLAGS="-DUSE_BUNDLED_UNIBILIUM=0"
```

To confirm at runtime that unibilium was not included, check `has('terminfo') == 0`.

### Build with specific "bundled" dependencies

Example of building with specific bundled and non-bundled dependencies:

```bash
make DEPS_CMAKE_FLAGS="-DUSE_BUNDLED=OFF -DUSE_BUNDLED_LUV=ON -DUSE_BUNDLED_TS=ON -DUSE_BUNDLED_LIBUV=ON"
```

### Build static binary (Linux)

1. Use a linux distribution which uses musl C. We will use Alpine Linux but any distro with musl should work. (glibc does not support static linking)
2. Run make passing the `STATIC_BUILD` variable: `make CMAKE_EXTRA_FLAGS="-DSTATIC_BUILD=1"`

In case you are not using Alpine Linux you can use a container to do the build the binary:

```bash
podman run \
  --rm \
  -it \
  -v "$PWD:/workdir" \
  -w /workdir \
  alpine:latest \
  sh -c 'apk add build-base cmake coreutils curl gettext-tiny-dev git linux-headers rust cargo && make CMAKE_EXTRA_FLAGS="-DSTATIC_BUILD=1"'
```

The resulting binary in `build/bin/nvim` will have all the dependencies statically linked:

```
build/bin/nvim: ELF 64-bit LSB executable, ARM aarch64, version 1 (SYSV), statically linked, BuildID[sha1]=b93fa8e678d508ac0a76a2e3da20b119105f1b2d, with debug_info, not stripped
```

## Build prerequisites

General requirements:

- A C compiler: GCC or Clang on Unix-like systems, or MSVC on Windows.
- GNU Make for the repository's Makefile shortcuts; use `gmake` on BSD.
- Rust and Cargo; see [Rust and Cargo](#rust-and-cargo).
- CMake version 3.16+, built with TLS/SSL support
  - Optional: Get the latest CMake from https://cmake.org/download/
    - Provides a shell script which works on most Linux systems. After running it, ensure the resulting `cmake` binary is in your $PATH so the Nvim build will find it.

The package commands below install native prerequisites. Install Rust
separately unless the command explicitly includes it.

### Rust and Cargo

The native build uses the toolchain in [rust-toolchain.toml](./rust-toolchain.toml).
Formatting and WASM builds use `RUST_NIGHTLY_TOOLCHAIN` from the
[Makefile](./Makefile). These files select the versions used by CI.

On systems supported by [rustup](https://rustup.rs/), install it first.
For example, on Linux or macOS:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
  sh -s -- -y --default-toolchain none
. "$HOME/.cargo/env"
```

Then, from the Rustvim checkout:

```bash
make rust-toolchain
```

For development or WASM builds, install both toolchains:

```bash
make rust-toolchains
```

The nightly rule installs host `rustfmt`, `rust-src`, and the
`wasm32-unknown-emscripten` standard library. Adding the WASM target retains
host support, so the same nightly can format the native workspace.

OpenBSD uses its packaged Rust and Cargo because rustup does not provide an
OpenBSD host toolchain. With system packages, ensure the compiler supports
the workspace's language features and locked dependencies. Rustup-based commands
require rustup and cannot install components into a system-managed compiler.

### Ubuntu / Debian

```bash
sudo apt-get install ninja-build gettext cmake curl build-essential git
```

### RHEL / Fedora

```bash
sudo dnf -y install ninja-build cmake gcc make gettext curl glibc-gconv-extra git
```

### openSUSE

```bash
sudo zypper install ninja cmake gcc-c++ gettext-tools curl git
```

### Arch Linux

```bash
sudo pacman -S base-devel cmake ninja curl git
```

### Alpine Linux

```bash
apk add build-base cmake coreutils curl gettext-tiny-dev git
```

### Void Linux

```bash
xbps-install base-devel cmake curl git
```

### NixOS / Nix

Neovim's `neovim-unwrapped` package and
[neovim-nightly-overlay](https://github.com/nix-community/neovim-nightly-overlay/)
build upstream Neovim. They do not include Rustvim's Cargo build requirements.
This repository currently has no Rustvim-specific Nix derivation.

**TODO:** Add a Nix derivation that builds Rustvim with CMake and Cargo.

### Haiku

These inherited Neovim dependency instructions have not been verified for
Rustvim. A compatible Rust host toolchain is also required.

Some deps can be pulled from haiku repos, the rest need "bundled" deps:
```bash
cmake -DUSE_BUNDLED_LIBUV=OFF -DUSE_BUNDLED_UNIBILIUM=OFF -DUSE_BUNDLED_LUAJIT=OFF -B .deps ./cmake.deps
make -C .deps
```

### FreeBSD

```bash
sudo pkg install cmake gmake ninja sha wget gettext curl git rustup-init
rustup-init -y --no-modify-path --default-toolchain none
. "$HOME/.cargo/env"
gmake rust-toolchain
```

If you get an error regarding a `sha256sum` mismatch, where the actual SHA-256 hash is `e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855`, then this is your issue (that's the `sha256sum` of an empty file).

### OpenBSD

```bash
doas pkg_add gmake cmake curl gettext-tools git ninja rust
```

While `ninja` is technically optional, the build is likely to fail without it. This is because `cmake` will use `make` in that case, and the bundled LuaJIT requires `gmake`. Instead of installing `ninja`, you could work around this by installing `luajit` and disabling the bundled LuaJIT:

```bash
gmake DEPS_CMAKE_FLAGS="-DUSE_BUNDLED_LUAJIT=0"
```

Another workaround is to edit `cmake/Deps.cmake` and comment out the line `set(MAKE_PRG "$(MAKE)")`. This will leave `MAKE_PRG` set to `gmake`, so LuaJIT will be built with `gmake`.

### macOS

#### macOS / Homebrew

1. Install Xcode Command Line Tools: `xcode-select --install`
2. Install [Homebrew](http://brew.sh)
3. Install Rustvim build dependencies:
    ```bash
    brew install ninja cmake gettext curl git
    ```
  - **Note**: If you see Wget certificate errors (for older macOS versions less than 10.10):
    ```bash
    brew install curl-ca-bundle
    echo CA_CERTIFICATE=$(brew --prefix curl-ca-bundle)/share/ca-bundle.crt >> ~/.wgetrc
    ```
  - **Note**: If you see `'stdio.h' file not found`, try the following:
    ```bash
    open /Library/Developer/CommandLineTools/Packages/macOS_SDK_headers_for_macOS_10.14.pkg
    ```

#### macOS / MacPorts

1. Install Xcode Command Line Tools: `xcode-select --install`
2. Install [MacPorts](http://www.macports.org)
3. Install Rustvim build dependencies:
    ```bash
    sudo port install ninja cmake gettext git
    ```
  - **Note**: If you see Wget certificate errors (for older macOS versions less than 10.10):
    ```bash
    sudo port install curl-ca-bundle
    echo CA_CERTIFICATE=/opt/local/share/curl/curl-ca-bundle.crt >> ~/.wgetrc
    ```
  - **Note**: If you see `'stdio.h' file not found`, try the following:
    ```bash
    open /Library/Developer/CommandLineTools/Packages/macOS_SDK_headers_for_macOS_10.14.pkg
    ```

#### Building for older macOS versions

From a newer macOS version, to build for older macOS versions, you will have to set the macOS deployment target:

```
make CMAKE_BUILD_TYPE=Release MACOSX_DEPLOYMENT_TARGET=11.0 DEPS_CMAKE_FLAGS="-DCMAKE_CXX_COMPILER=$(xcrun -find c++)"
```

Note that the C++ compiler is explicitly set so that it can be found when the deployment target is set.

## Cross-compiling

Cross-compilation outside the WASM build is not fully supported. CMake and
Cargo must target the same platform:

- Set CMake's target compiler or toolchain file.
- Set `RUST_TARGET` to the matching Cargo target triple, or set
  `CARGO_BUILD_TARGET` before the first CMake configuration.
- Install the target's Rust standard library, for example with
  `rustup target add TARGET_TRIPLE`.
- Provide native tools for code generation, including `LUA_PRG`,
  `LUA_GEN_PRG`, `NLUA0_HOST_PRG`, and `NVIM_HOST_PRG`.

Rustvim uses CMake and Cargo. The inherited Zig build does not integrate
Rustvim's Rust components.

### WebAssembly (experimental)

Rustvim builds WebAssembly with CMake, Cargo, and the
[Emscripten SDK](https://emscripten.org/docs/getting_started/downloads.html).
Install and activate the SDK in `.deps/emsdk`, or set `EMSDK` to its directory.
CI uses Emscripten 6.0.2. For the same SDK locally:

```sh
git clone https://github.com/emscripten-core/emsdk.git .deps/emsdk
.deps/emsdk/emsdk install 6.0.2
.deps/emsdk/emsdk activate 6.0.2
```

The SDK does not need to be added to your shell's `PATH`.
The current build script requires Bash, Ninja, and `nproc`.

Install the pinned Rust toolchains and build the browser bundle:

```sh
make rust-toolchains
make wasm
```

The build uses a native editor and Lua module for code generation, then
cross-compiles the editor and its libraries with Emscripten. Rust's standard
library is rebuilt with the shared-memory features required by the browser.

The output in `build/wasm/site` contains `nvim.js`, `nvim.wasm`,
`nvim.data`, and the browser frontend. Run the WASM smoke checks with:

```sh
make wasmtest
```

These execute the packaged editor under Node.js and check shared memory,
Unicode editing, floating windows, Lua errors, and the static Tree-sitter parsers.

To open the browser frontend:

```sh
python3 build/wasm/site/serve.py
```

Visit http://localhost:8002. The server supplies the cross-origin isolation
headers required for shared memory. See the
[WASM build chapter](./docs/rustvim-book/src/build-system/wasm.md) for the build
stages, generated files, and browser integration.
