if(NOT NVIM_HOST_PRG OR NOT NLUA0_HOST_PRG)
  message(FATAL_ERROR "WASM requires native build tools; use make wasm")
endif()

set(WASM_RUNTIME_DIR "${PROJECT_BINARY_DIR}/wasm-runtime")
set(WASM_SITE_DIR "${PROJECT_BINARY_DIR}/site")

foreach(lang lua c diff vim vimdoc query markdown markdown_inline)
  set(parser "${DEPS_PREFIX}/lib/nvim/parser/${lang}.a")
  if(NOT EXISTS "${parser}")
    message(FATAL_ERROR "Missing static parser ${parser}. Run 'make wasm' from the repository root to build the WASM dependencies.")
  endif()
  target_link_libraries(nvim_bin PRIVATE "${parser}")
endforeach()

file(GLOB_RECURSE WASM_RUNTIME_INPUTS CONFIGURE_DEPENDS "${PROJECT_SOURCE_DIR}/runtime/*")
get_directory_property(WASM_GENERATED_SYNTAX DIRECTORY "${PROJECT_SOURCE_DIR}/runtime" DEFINITION GENERATED_SYN_VIM)
get_directory_property(WASM_GENERATED_TAGS DIRECTORY "${PROJECT_SOURCE_DIR}/runtime" DEFINITION GENERATED_HELP_TAGS)
get_directory_property(WASM_PACKAGE_TAGS DIRECTORY "${PROJECT_SOURCE_DIR}/runtime" DEFINITION GENERATED_PACKAGE_TAGS)

set(WASM_RUNTIME_STAMP "${PROJECT_BINARY_DIR}/wasm-runtime.stamp")
add_custom_command(OUTPUT "${WASM_RUNTIME_STAMP}"
  COMMAND ${CMAKE_COMMAND} -E rm -rf "${WASM_RUNTIME_DIR}"
  COMMAND ${CMAKE_COMMAND} -E copy_directory "${PROJECT_SOURCE_DIR}/runtime" "${WASM_RUNTIME_DIR}"
  COMMAND ${CMAKE_COMMAND} -E copy_directory "${PROJECT_BINARY_DIR}/runtime" "${WASM_RUNTIME_DIR}"
  COMMAND ${CMAKE_COMMAND} -E touch "${WASM_RUNTIME_STAMP}"
  DEPENDS ${WASM_RUNTIME_INPUTS} ${WASM_GENERATED_SYNTAX} ${WASM_GENERATED_TAGS} ${WASM_PACKAGE_TAGS}
  VERBATIM)
add_custom_target(wasm_runtime DEPENDS "${WASM_RUNTIME_STAMP}")
add_dependencies(nvim_bin wasm_runtime)
set_property(TARGET nvim_bin APPEND PROPERTY LINK_DEPENDS "${WASM_RUNTIME_STAMP}")
set_target_properties(nvim_bin PROPERTIES ENABLE_EXPORTS FALSE SUFFIX ".js")
set_target_properties(nlua0 PROPERTIES EXCLUDE_FROM_ALL TRUE)
target_compile_definitions(nvim_bin PRIVATE MAKE_LIB)
target_sources(nvim_bin PRIVATE
  "${PROJECT_SOURCE_DIR}/src/wasm/wasm_stubs.c"
  "${PROJECT_SOURCE_DIR}/src/static_ts_registry.c")
target_link_options(nvim_bin PRIVATE
  -pthread
  -fexceptions
  --no-entry
  -Wl,--fatal-warnings
  -lidbfs.js
  "-sEXPORTED_FUNCTIONS=_nvim_main,_malloc,_free"
  "-sEXPORTED_RUNTIME_METHODS=FS,ENV,HEAPU8,IDBFS,lengthBytesUTF8,stringToUTF8,UTF8ToString,setValue,getValue,ccall,cwrap"
  -sMODULARIZE=1
  -sEXPORT_NAME=createNvim
  -sSTACK_SIZE=8388608
  -sINITIAL_MEMORY=268435456
  -sEXIT_RUNTIME=0
  -sINVOKE_RUN=0
  -sFORCE_FILESYSTEM=1
  "--preload-file=${WASM_RUNTIME_DIR}@/runtime")

file(GLOB WASM_FRONTEND CONFIGURE_DEPENDS
  "${PROJECT_SOURCE_DIR}/src/wasm/*.html"
  "${PROJECT_SOURCE_DIR}/src/wasm/*.js"
  "${PROJECT_SOURCE_DIR}/src/wasm/*.css"
  "${PROJECT_SOURCE_DIR}/src/wasm/serve.py")
add_custom_target(wasm-package
  COMMAND ${CMAKE_COMMAND} -E make_directory "${WASM_SITE_DIR}"
  COMMAND ${CMAKE_COMMAND} -E copy_if_different
    "$<TARGET_FILE:nvim_bin>"
    "${CMAKE_RUNTIME_OUTPUT_DIRECTORY}/nvim.wasm"
    "${CMAKE_RUNTIME_OUTPUT_DIRECTORY}/nvim.data"
    ${WASM_FRONTEND} "${WASM_SITE_DIR}"
  DEPENDS nvim_bin
  VERBATIM)

add_custom_target(wasmtest
  COMMAND ${CMAKE_CROSSCOMPILING_EMULATOR} "${PROJECT_SOURCE_DIR}/test/wasm/smoke.cjs" "${WASM_SITE_DIR}/nvim.js"
  DEPENDS wasm-package
  USES_TERMINAL
  VERBATIM)
