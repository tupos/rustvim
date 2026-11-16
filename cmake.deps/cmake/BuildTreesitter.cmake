if(ENABLE_WASMTIME)
  # The final executable resolves Wasmtime's C API from the Rustvim archive.
  set(TREESITTER_ARGS
    -D TREE_SITTER_FEATURE_WASM=ON
    -D WASMTIME_LIBRARY:STRING=rustvim_lib)
endif()

get_externalproject_options(treesitter ${DEPS_IGNORE_SHA})
ExternalProject_Add(treesitter
  DOWNLOAD_DIR ${DEPS_DOWNLOAD_DIR}/treesitter
  CMAKE_ARGS ${DEPS_CMAKE_ARGS} ${TREESITTER_ARGS}
  ${EXTERNALPROJECT_OPTIONS})

if(USE_BUNDLED_WASMTIME)
  add_dependencies(treesitter wasmtime)
endif()
