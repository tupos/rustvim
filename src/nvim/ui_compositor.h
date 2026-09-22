#pragma once

#include <stdbool.h>

#include "nvim/api/private/defs.h"  // IWYU pragma: keep
#include "nvim/grid_defs.h"  // IWYU pragma: keep
#include "nvim/types_defs.h"  // IWYU pragma: keep
#include "nvim/ui_defs.h"  // IWYU pragma: keep

typedef struct {
  Integer start_col;
  Integer end_col;
  bool active;
} CompositorLineRange;

extern bool ui_comp_resize_buffer(Integer width);
extern bool ui_comp_release_buffer(void);
extern CompositorLineRange ui_comp_begin_line(
  Integer start_col, Integer end_col, LineFlags flags);

extern bool ui_comp_compose_grid_segment(
  ScreenGrid *foreground_grid, ScreenGrid *background_grid,
  int foreground_row, int start_col, int end_col,
  int background_row, Integer output_start_col);

extern bool ui_comp_compose_separator_segment(
  ScreenGrid *background_grid, int background_row,
  Integer output_start_col, Integer output_end_col,
  schar_T character, sattr_T attribute);

extern bool ui_comp_finish_line(
  Integer row, LineFlags flags, bool allow_wrap, bool abort_on_invalid);

extern bool ui_comp_emit_clear_line(
  Integer row, Integer start_col, Integer end_col, sattr_T attribute);

extern bool ui_comp_should_compose_scroll_row(
  ScreenGrid *grid, int row, int start_col);

#include "ui_compositor.h.generated.h"
