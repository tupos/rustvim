# Cargo builds Wasmtime with Rustvim; only prepare matching C headers here.
get_externalproject_options(wasmtime ${DEPS_IGNORE_SHA})
ExternalProject_Add(wasmtime
  DOWNLOAD_DIR ${DEPS_DOWNLOAD_DIR}/wasmtime
  CONFIGURE_COMMAND ""
  BUILD_COMMAND ""
  INSTALL_COMMAND ${CMAKE_COMMAND}
    -D WASMTIME_DISABLE_ALL_FEATURES=ON
    -D WASMTIME_FEATURE_CRANELIFT=ON
    -D WASMTIME_FEATURE_GC=ON
    -D WASMTIME_FEATURE_GC_NULL=ON
    -D CMAKE_INSTALL_PREFIX=${DEPS_INSTALL_DIR}
    -P <SOURCE_DIR>/crates/c-api/cmake/install-headers.cmake
  ${EXTERNALPROJECT_OPTIONS})
