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
  Integer start_row;
  Integer end_row;
  Integer start_column;
  Integer end_column;
} CompositorRecomposeArea;

typedef struct {
  int grid_handle;
  int row;
  int column;
} CompositorCursorRequest;

typedef struct {
  size_t layer_index;
  int row;
  int column;
  size_t raise_to;
  bool should_raise;
  bool emit;
} CompositorCursorPlan;

typedef struct {
  Integer row;
  Integer start_col;
  Integer end_col;
  LineFlags flags;
  sattr_T message_separator_attribute;
  int screen_columns;
  bool abort_on_invalid;
} CompositorLineRequest;

typedef struct {
  Integer row;
  schar_T separator_character;
  int screen_rows;
  int screen_columns;
  bool scrolled;
  bool has_separator_character;
  bool should_draw;
} CompositorMessagePositionRequest;

typedef struct {
  Integer scroll_top;
  Integer scroll_bottom;
  Integer scroll_left;
  Integer scroll_right;
  Integer scroll_rows;
  Integer recompose_start_row;
  Integer recompose_end_row;
  Integer recompose_start_column;
  Integer recompose_end_column;
  bool scroll;
  bool recompose;
} CompositorMessagePositionPlan;

typedef struct {
  int grid_handle;
  Integer row;
  Integer start_col;
  Integer end_col;
  Integer clear_col;
  LineFlags flags;
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
  int grid_handle;
  Integer top;
  Integer bottom;
  Integer left;
  Integer right;
  Integer row_delta;
} CompositorScrollRequest;

typedef struct {
  size_t current_layer_index;
  Integer top;
  Integer bottom;
  Integer left;
  Integer right;
  Integer first_recompose_row;
  Integer recompose_end_row;
  int first_grid_row;
  int grid_start_column;
  bool discard;
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

extern bool ui_comp_grid_recompose_area(
  ScreenGrid *grid, CompositorRecomposeArea *output);

extern bool ui_comp_place_new_grid(
  ScreenGrid **layer_grids, size_t layer_count,
  ScreenGrid *grid, ScreenGrid *current_window_grid,
  CompositorGridPlacementRequest request, bool on_top,
  CompositorAreaCallback compose_area);

extern bool ui_comp_plan_cursor(
  ScreenGrid *const *layer_grids, size_t layer_count,
  CompositorCursorRequest request, CompositorCursorPlan *output);

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

extern bool ui_comp_grid_contains_at(
  ScreenGrid *grid, int row, int column);

extern bool ui_comp_grid_accepts_mouse_at(
  ScreenGrid *grid, int origin_row, int origin_column,
  int row, int column);

extern bool ui_comp_cursor_is_obscured_by_grid(
  ScreenGrid *top_grid, ScreenGrid *current_grid,
  ScreenGrid *default_grid, int minimum_z_index_gap);

extern bool ui_comp_compose_line(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *message_grid, CompositorLineRequest request);

extern void ui_comp_hide_message_separator(void);

extern bool ui_comp_plan_message_position(
  ScreenGrid *message_grid, ScreenGrid *default_grid, CompositorMessagePositionRequest request,
  CompositorMessagePositionPlan *output);

extern bool ui_comp_emit_clear_line(
  Integer row, Integer start_col, Integer end_col, sattr_T attribute);

extern bool ui_comp_should_compose_scroll_row(
  ScreenGrid *grid, int row, int start_col);

extern bool ui_comp_plan_raw_line(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *message_grid, CompositorRawLineRequest request,
  CompositorRawLinePlan *output);

extern bool ui_comp_plan_scroll(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *message_grid,
  CompositorScrollRequest request, CompositorScrollPlan *output);

#include "ui_compositor.h.generated.h"
