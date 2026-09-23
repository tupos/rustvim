#pragma once

#include <stdbool.h>
#include <stddef.h>

#include "nvim/api/private/defs.h"  // IWYU pragma: keep
#include "nvim/grid_defs.h"  // IWYU pragma: keep
#include "nvim/types_defs.h"  // IWYU pragma: keep
#include "nvim/ui_defs.h"  // IWYU pragma: keep

typedef struct {
  int row;
  int column;
  int height;
  int width;
  bool valid;
  bool should_draw;
} CompositorGridPlacementRequest;

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
  Integer row;
  Integer start_col;
  Integer end_col;
  Integer clear_col;
  LineFlags flags;
  bool message_was_scrolled;
} CompositorRawLineRequest;

typedef struct {
  Integer row;
  Integer start_col;
  Integer end_col;
  Integer clear_col;
  Integer requested_clear_col;
  LineFlags flags;
  bool row_outside;
  bool columns_clipped;
  bool discard;
  bool recompose;
} CompositorRawLinePlan;

typedef struct {
  Integer top;
  Integer bottom;
  Integer left;
  Integer right;
  Integer row_delta;
} CompositorScrollRequest;

typedef struct {
  Integer top;
  Integer bottom;
  Integer left;
  Integer right;
  Integer first_recompose_row;
  Integer recompose_end_row;
  int first_grid_row;
  int grid_start_column;
  bool recompose;
} CompositorScrollPlan;

typedef void (*CompositorAreaCallback)(
  Integer start_row, Integer end_row, Integer start_column, Integer end_column);

extern bool ui_comp_resize_buffer(Integer width);
extern bool ui_comp_release_buffer(void);

extern bool ui_comp_place_existing_grid(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *grid, CompositorGridPlacementRequest request,
  CompositorAreaCallback compose_area, bool *moved);

extern bool ui_comp_place_new_grid(
  ScreenGrid **layer_grids, size_t layer_count,
  ScreenGrid *grid, ScreenGrid *current_window_grid,
  CompositorGridPlacementRequest request, bool on_top,
  CompositorAreaCallback compose_area);

extern bool ui_comp_adjust_layer_order(
  ScreenGrid **layer_grids, size_t layer_count,
  size_t layer_index, bool raise);

extern bool ui_comp_remove_layer(
  ScreenGrid **layer_grids, size_t layer_count,
  ScreenGrid *grid, bool *removed);

extern bool ui_comp_raise_layer(
  ScreenGrid **layer_grids, size_t layer_count,
  ScreenGrid *grid, size_t target_index,
  CompositorAreaCallback compose_area);

extern size_t ui_comp_find_layer_at_coord(
  ScreenGrid *const *layer_grids, size_t layer_count,
  int row, int column, bool require_mouse_input);

extern bool ui_comp_compose_line(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *message_grid, CompositorLineRequest request);

extern bool ui_comp_emit_clear_line(
  Integer row, Integer start_col, Integer end_col, sattr_T attribute);

extern bool ui_comp_should_compose_scroll_row(
  ScreenGrid *grid, int row, int start_col);

extern bool ui_comp_plan_raw_line(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *current_grid, ScreenGrid *message_grid,
  CompositorRawLineRequest request, CompositorRawLinePlan *output);

extern bool ui_comp_plan_scroll(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *current_grid, ScreenGrid *message_grid,
  bool message_was_scrolled, CompositorScrollRequest request,
  CompositorScrollPlan *output);

#include "ui_compositor.h.generated.h"
