#pragma once

#include <stdbool.h>

#include "nvim/types_defs.h"

extern bool rustvim_context_register(void);
extern bool rustvim_tracing_initialize(void);
extern void rustvim_rendering_initialize(void);
extern void rustvim_rendering_free_all_mem(void);
extern void rustvim_ui_schedule_grid_cursor(handle_T grid_handle, int row, int column);
extern void rustvim_ui_mark_grid_cursor_moved(handle_T grid_handle);
extern void rustvim_ui_reset_grid_cursor_position(void);
extern int rustvim_ui_current_grid_cursor_row(void);
extern int rustvim_ui_current_grid_cursor_column(void);
extern bool rustvim_ui_flush_pending_grid_cursor(void);
