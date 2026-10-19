// Native printf and diagnostic adapters for the Rust logging backend.

#include <stdarg.h>
#include <stdbool.h>
#include <stddef.h>
#include <stdio.h>

#include "auto/config.h"
#include "nvim/globals.h"
#include "nvim/log.h"
#include "nvim/memory.h"

/// Formats one attempt without consuming the enclosing logmsg() argument list.
static int REAL_FATTR_PRINTF(3, 0) log_format(char *buffer, size_t capacity, const char *format,
                                           void *arguments)
{
  va_list copy;
  va_copy(copy, *(va_list *)arguments);
  int written = vsnprintf(buffer, capacity, format, copy);
  va_end(copy);
  return written;
}

/// Keeps native varargs in C while Rust owns filtering, buffering, and output.
bool logmsg(int log_level, const char *context, const char *func_name, int line_num, bool eol,
            const char *fmt, ...)
{
  bool fatal = false;
#ifdef EXITFREE
  fatal = entered_free_all_mem;
#endif
  va_list args;
  va_start(args, fmt);
  bool result = rustvim_logmsg(log_level, context, func_name, line_num, eol, fatal,
                               fmt, &args, log_format);
  va_end(args);
  return result;
}

/// Preserves the native build's debug-only logging entry point.
void log_debug_message(const char *message)
{
#ifdef NVIM_LOG_DEBUG
  DLOG("%s", message);
#else
  (void)message;
#endif
}

/// Supplies the native standard-error stream required by libuv's FILE API.
void log_uv_handles(void *loop)
{
  rustvim_log_uv_handles(loop, stderr);
}

#ifdef HAVE_EXECINFO_BACKTRACE
/// Transfers regular streams while retaining ownership of standard streams.
void log_callstack_to_file(FILE *log_file, const char *const func_name, const int line_num)
{
  rustvim_log_callstack(log_file, log_file != stderr && log_file != stdout, func_name, line_num);
}

/// Requests a Rust-captured stack at the logger's selected destination.
void log_callstack(const char *const func_name, const int line_num)
{
  rustvim_log_callstack(NULL, false, func_name, line_num);
}
#endif
