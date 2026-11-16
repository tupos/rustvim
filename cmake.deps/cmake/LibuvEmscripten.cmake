# The project hook runs before uv_a exists, so extend it at directory completion.
cmake_language(DEFER CALL target_sources uv_a PRIVATE
  "${CMAKE_CURRENT_SOURCE_DIR}/src/unix/no-fsevents.c"
  "${CMAKE_CURRENT_SOURCE_DIR}/src/unix/no-proctitle.c"
  "${CMAKE_CURRENT_SOURCE_DIR}/src/unix/posix-hrtime.c")
cmake_language(DEFER CALL target_compile_definitions uv_a PRIVATE _GNU_SOURCE)
