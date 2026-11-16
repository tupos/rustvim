if(EMSCRIPTEN)
  if(CMAKE_VERSION VERSION_LESS 3.19)
    message(FATAL_ERROR "The Emscripten libuv build requires CMake 3.19 or newer")
  endif()
  set(LIBUV_PLATFORM_FILE "${CMAKE_CURRENT_LIST_DIR}/LibuvEmscripten.cmake")
  set(LIBUV_PLATFORM_ARGS "-DCMAKE_PROJECT_libuv_INCLUDE=${LIBUV_PLATFORM_FILE}")
endif()

get_externalproject_options(libuv ${DEPS_IGNORE_SHA})
ExternalProject_Add(libuv
  DOWNLOAD_DIR ${DEPS_DOWNLOAD_DIR}/libuv
  CMAKE_ARGS ${DEPS_CMAKE_ARGS} ${LIBUV_PLATFORM_ARGS}
    -D CMAKE_INSTALL_LIBDIR=lib
    -D BUILD_TESTING=OFF
    -D LIBUV_BUILD_SHARED=OFF
    -D UV_LINT_W4=OFF
  ${EXTERNALPROJECT_OPTIONS})

if(EMSCRIPTEN)
  ExternalProject_Add_StepDependencies(libuv configure "${LIBUV_PLATFORM_FILE}")
endif()
