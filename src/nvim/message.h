#pragma once

#include <errno.h>
#include <stdbool.h>
#include <stddef.h>  // IWYU pragma: keep

#include "nvim/ex_cmds_defs.h"  // IWYU pragma: keep
#include "nvim/keycodes_defs.h"  // IWYU pragma: keep
#include "nvim/macros_defs.h"
#include "nvim/message_defs.h"  // IWYU pragma: keep
#include "nvim/types_defs.h"

/// Types of dialogs passed to do_dialog().
enum {
  VIM_GENERIC   = 0,
  VIM_ERROR     = 1,
  VIM_WARNING   = 2,
  VIM_INFO      = 3,
  VIM_QUESTION  = 4,
  VIM_LAST_TYPE = 4,  ///< sentinel value
};

/// Return values for functions like vim_dialogyesno()
enum {
  VIM_YES        = 2,
  VIM_NO         = 3,
  VIM_CANCEL     = 4,
  VIM_ALL        = 5,
  VIM_DISCARDALL = 6,
};

extern MessageHistoryEntry *msg_hist_last;

EXTERN bool msg_ext_need_clear INIT( = false);
/// Set to true to force grouping a set of message chunks into a single `cmdline_show` event.
EXTERN bool msg_ext_skip_flush INIT( = false);
/// Set to true when previous message should be overwritten.
EXTERN bool msg_ext_overwrite INIT( = false);
/// Set to true to avoid setting "verbose" kind for "last set" messages.
EXTERN bool msg_ext_skip_verbose INIT( = false);
/// Set to false for non-internal messages to determine UI callback |api-fast| context.
EXTERN bool msg_ext_fast INIT( = true);

/// allocated grid for messages. Used unless ext_messages is active.
/// See also the description at msg_scroll_flush()
EXTERN handle_T msg_grid_handle INIT( = 0);
EXTERN int msg_grid_pos INIT( = 0);

EXTERN int msg_listdo_overwrite INIT( = 0);

#include "message.h.generated.h"

// Prefer using semsg(), because perror() may send the output to the wrong
// destination and mess up the screen.
#define PERROR(msg) (void)semsg("%s: %s", (msg), strerror(errno))
