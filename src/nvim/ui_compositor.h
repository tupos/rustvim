#pragma once

#include <stdbool.h>
#include <stddef.h>

#include "nvim/api/private/defs.h"  // IWYU pragma: keep
#include "nvim/grid_defs.h"  // IWYU pragma: keep
#include "nvim/types_defs.h"  // IWYU pragma: keep
#include "nvim/ui_defs.h"  // IWYU pragma: keep

typedef struct {
  Integer row;
  Integer start_col;
  Integer end_col;
  LineFlags flags;
  int message_separator_row;
  schar_T message_separator_character;
  sattr_T message_separator_attribute;
  int screen_columns;
  bool abort_on_invalid;
} CompositorLineRequest;

typedef struct {
  int top;
  int bottom;
  int left;
  int right;
  size_t current_layer_index;
  bool message_was_scrolled;
} CompositorCoveredAboveRequest;

extern bool ui_comp_resize_buffer(Integer width);
extern bool ui_comp_release_buffer(void);

extern bool ui_comp_compose_line(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *message_grid, CompositorLineRequest request);

extern bool ui_comp_emit_clear_line(
  Integer row, Integer start_col, Integer end_col, sattr_T attribute);

extern bool ui_comp_should_compose_scroll_row(
  ScreenGrid *grid, int row, int start_col);

extern bool ui_comp_is_covered_above(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *message_grid, CompositorCoveredAboveRequest request);

#include "ui_compositor.h.generated.h"
