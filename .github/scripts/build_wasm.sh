#!/usr/bin/env bash

set -euo pipefail

if [[ $# -ne 2 ]]; then
  printf 'Usage: %s RUST_TOOLCHAIN {wasm-package|wasmtest}\n' "$0" >&2
  exit 2
fi
wasm_rust_toolchain=$1
wasm_target=$2
case "$wasm_target" in
  wasm-package|wasmtest) ;;
  *) printf 'Unknown WASM build target: %s\n' "$wasm_target" >&2; exit 2 ;;
esac

wasm_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
wasm_host_build="$wasm_root/build"
wasm_host_deps="$wasm_root/.deps"
wasm_build="$wasm_root/build/wasm"
wasm_deps="$wasm_root/.deps/wasm"

if [[ ! -f "$wasm_host_deps/CMakeCache.txt" ]]; then
  cmake -S "$wasm_root/cmake.deps" -B "$wasm_host_deps" -G Ninja
fi
cmake --build "$wasm_host_deps" -j"$(nproc)"

if [[ ! -f "$wasm_host_build/CMakeCache.txt" ]]; then
  cmake -S "$wasm_root" -B "$wasm_host_build" -G Ninja \
    -DCMAKE_BUILD_TYPE=Release -DENABLE_LIBINTL=OFF
fi
wasm_host_config="$(sed -n 's/^CMAKE_BUILD_TYPE:[^=]*=//p' "$wasm_host_build/CMakeCache.txt")"
wasm_host_config="${wasm_host_config:-Release}"
cmake --build "$wasm_host_build" --config "$wasm_host_config" --target nvim -j"$(nproc)"

wasm_sdk="${EMSDK:-$wasm_root/.deps/emsdk}"
if [[ ! -f "$wasm_sdk/emsdk_env.sh" ]]; then
  printf 'Install and activate Emscripten in %s or set EMSDK.\n' "$wasm_sdk" >&2
  exit 1
fi
# shellcheck source=/dev/null
source "$wasm_sdk/emsdk_env.sh"

emcmake cmake -S "$wasm_root/cmake.deps" -B "$wasm_deps" -G Ninja \
  -DUSE_BUNDLED_LUAJIT=OFF -DUSE_BUNDLED_LUA=ON \
  -DUSE_BUNDLED_TS_PARSERS=ON -DCMAKE_C_FLAGS=-pthread
cmake --build "$wasm_deps" -j"$(nproc)"

emcmake cmake -S "$wasm_root" -B "$wasm_build" -G Ninja \
  -C "$wasm_host_build/host-tools-$wasm_host_config.cmake" \
  -DCMAKE_BUILD_TYPE=Release -DENABLE_LIBINTL=OFF -DENABLE_LTO=OFF \
  -DPREFER_LUA=ON -DCOMPILE_LUA=OFF -DCMAKE_C_FLAGS=-pthread \
  -DDEPS_PREFIX="$wasm_deps/usr" -DRUST_TARGET=wasm32-unknown-emscripten \
  -DRUST_TOOLCHAIN="$wasm_rust_toolchain"
cmake --build "$wasm_build" --target "$wasm_target" -j"$(nproc)"
printf 'WASM browser bundle: %s/site\n' "$wasm_build"
