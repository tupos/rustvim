#!/usr/bin/env bash

set -euo pipefail

RVIM_SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
RVIM_WORKSPACE_DIR="$(cd -- "${RVIM_SCRIPT_DIR}/.." && pwd)"
RVIM_BUILD_DIR="${RUSTVIM_BUILD_DIR:-build}"

case "${RVIM_BUILD_DIR}" in
  /*)
    ;;
  *)
    RVIM_BUILD_DIR="${RVIM_WORKSPACE_DIR}/${RVIM_BUILD_DIR}"
    ;;
esac

RVIM_CMAKE_CACHE="${RVIM_BUILD_DIR}/CMakeCache.txt"
if [[ ! -r "${RVIM_CMAKE_CACHE}" ]]; then
  printf '%s\n' \
    "cargo-linker: ${RVIM_CMAKE_CACHE} is unavailable; configure CMake first" \
    >&2
  exit 1
fi

RVIM_C_COMPILER=
while IFS='=' read -r RVIM_CACHE_KEY RVIM_CACHE_VALUE; do
  if [[ "${RVIM_CACHE_KEY}" == CMAKE_C_COMPILER:* ]]; then
    RVIM_C_COMPILER="${RVIM_CACHE_VALUE}"
    break
  fi
done < "${RVIM_CMAKE_CACHE}"

if [[ -z "${RVIM_C_COMPILER}" ]]; then
  printf '%s\n' "cargo-linker: CMAKE_C_COMPILER is missing from ${RVIM_CMAKE_CACHE}" >&2
  exit 1
fi

exec "${RVIM_C_COMPILER}" "$@"
