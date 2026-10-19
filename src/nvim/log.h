#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdio.h>

#include "auto/config.h"
#include "nvim/func_attr.h"
#include "nvim/macros_defs.h"

// USDT probes. Example invocation:
//     NVIM_PROBE(nvim_foo_bar, 1, string.data);
#ifdef HAVE_SYS_SDT_H
# include <sys/sdt.h>  // IWYU pragma: keep

# define NVIM_PROBE(name, n, ...) STAP_PROBE##n(neovim, name, __VA_ARGS__)
#else
# define NVIM_PROBE(name, n, ...)
#endif

// uncrustify:off
#if NVIM_HAS_INCLUDE(<sanitizer/asan_interface.h>)
# include <sanitizer/asan_interface.h>  // IWYU pragma: keep
#endif
// uncrustify:on

#define LOGLVL_DBG 1
#define LOGLVL_INF 2
#define LOGLVL_WRN 3
#define LOGLVL_ERR 4

#define LOG(level, ...) logmsg((level), NULL, __func__, __LINE__, true, __VA_ARGS__)

#ifdef NVIM_LOG_DEBUG
# define DLOG(...) logmsg(LOGLVL_DBG, NULL, __func__, __LINE__, true, __VA_ARGS__)
# define DLOGN(...) logmsg(LOGLVL_DBG, NULL, __func__, __LINE__, false, __VA_ARGS__)
# define ILOG(...) logmsg(LOGLVL_INF, NULL, __func__, __LINE__, true, __VA_ARGS__)
# define ILOGN(...) logmsg(LOGLVL_INF, NULL, __func__, __LINE__, false, __VA_ARGS__)
#else
# define DLOG(...)
# define DLOGN(...)
# define ILOG(...)
# define ILOGN(...)
#endif

#define WLOG(...) logmsg(LOGLVL_WRN, NULL, __func__, __LINE__, true, __VA_ARGS__)
#define WLOGN(...) logmsg(LOGLVL_WRN, NULL, __func__, __LINE__, false, __VA_ARGS__)
#define ELOG(...) logmsg(LOGLVL_ERR, NULL, __func__, __LINE__, true, __VA_ARGS__)
#define ELOGN(...) logmsg(LOGLVL_ERR, NULL, __func__, __LINE__, false, __VA_ARGS__)

#ifdef HAVE_EXECINFO_BACKTRACE
# define LOG_CALLSTACK() log_callstack(__func__, __LINE__)
# define LOG_CALLSTACK_TO_FILE(fp) log_callstack_to_file(fp, __func__, __LINE__)
#endif

#ifndef NVIM_LOG_DEBUG
EXTERN int g_min_log_level INIT( = LOGLVL_WRN);
#else
EXTERN int g_min_log_level INIT( = 0);
#endif

/// Initializes the Rust logger and its tracing subscriber during native startup.
void log_init(void);
/// Formats one native message; returns false when filtered, recursive, or failed.
bool logmsg(int log_level, const char *context, const char *func_name, int line_num, bool eol,
            const char *fmt, ...) REAL_FATTR_PRINTF(6, 7);
/// Emits debug text only when the build enables native debug logging.
void log_debug_message(const char *message);
/// Writes libuv's native handle diagnostic through the Rust logger.
void log_uv_handles(void *loop);

#ifdef HAVE_EXECINFO_BACKTRACE
/// Writes a mixed native stack, consuming streams other than stdout and stderr.
void log_callstack_to_file(FILE *log_file, const char *func_name, int line_num);
/// Writes a mixed native stack to the selected logging destination.
void log_callstack(const char *func_name, int line_num);
#endif

/// Native formatter using vsnprintf's length convention; it retains no pointers.
typedef int (*LogFormatter)(char *buffer, size_t capacity, const char *format, void *arguments)
  REAL_FATTR_PRINTF(3, 0);

/// Borrows native arguments synchronously while Rust formats and writes a message.
bool rustvim_logmsg(int log_level, const char *context, const char *func_name, int line_num,
                    bool eol, bool fatal, const char *format, void *arguments,
                    LogFormatter formatter);
/// Adapts libuv's native diagnostic under Rust-owned output locking.
void rustvim_log_uv_handles(void *loop, FILE *standard_error);
/// Writes a Rust-captured stack, optionally consuming a native stream.
void rustvim_log_callstack(FILE *output, bool close_output, const char *func_name, int line_num);
