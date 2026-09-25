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
  bool on_top;
} CompositorGridPlacementRequest;

typedef struct {
  Integer row;
  schar_T separator_character;
  bool scrolled;
  bool has_separator_character;
} CompositorMessagePositionRequest;

typedef struct {
  Integer grid_handle;
  Integer row;
  Integer start_col;
  Integer end_col;
  Integer clear_col;
  Integer clear_attr;
  LineFlags flags;
} CompositorRawLineRequest;

typedef struct {
  Integer grid_handle;
  Integer top;
  Integer bottom;
  Integer left;
  Integer right;
  Integer row_delta;
  Integer column_delta;
} CompositorScrollRequest;

extern bool ui_comp_initialize_layer_registry(ScreenGrid *default_grid);
extern bool ui_comp_clear_layer_registry(void);
extern void ui_comp_initialize_debug_highlights(void);

extern void ui_comp_register_composed_ui(void);
extern bool ui_comp_unregister_composed_ui(void);
extern bool ui_comp_set_screen_valid(bool valid);

extern bool ui_comp_process_grid_resize(
  Integer grid, Integer width, Integer height);
extern bool ui_comp_release_buffer(void);

extern bool ui_comp_process_grid_placement(
  ScreenGrid *grid, Integer current_window_grid_handle,
  CompositorGridPlacementRequest request,
  ScreenGrid *message_grid, bool *moved);

extern bool ui_comp_recompose_grid(
  ScreenGrid *message_grid, Integer grid_handle);

extern bool ui_comp_process_grid_cursor_goto(
  ScreenGrid *message_grid, Integer grid_handle,
  Integer row, Integer column);

extern bool ui_comp_adjust_grid_layer_order(
  Integer grid_handle, bool raise);

extern bool ui_comp_process_grid_removal(
  ScreenGrid *message_grid, Integer grid_handle);

extern ScreenGrid *ui_comp_find_grid_at_coord(
  int row, int column, bool require_mouse_input);

extern bool ui_comp_grid_contains_at(
  ScreenGrid *grid, int row, int column);

extern bool ui_comp_grid_accepts_mouse_at(
  ScreenGrid *grid, int origin_row, int origin_column,
  int row, int column);

extern bool ui_comp_cursor_is_covered_by_higher_layer(
  ScreenGrid *current_grid, int row, int column,
  int minimum_z_index_gap);

extern bool ui_comp_cursor_is_covered_by_higher_layer_in_stack(
  Integer current_grid_handle, int current_grid_z_index,
  int row, int column, int minimum_z_index_gap);

extern void ui_comp_hide_message_separator(void);

extern bool ui_comp_update_message_position(
  ScreenGrid *message_grid, ScreenGrid *default_grid,
  CompositorMessagePositionRequest request);

extern bool ui_comp_process_raw_line(
  ScreenGrid *message_grid, CompositorRawLineRequest request,
  const schar_T *characters, const sattr_T *attributes);

extern bool ui_comp_process_scroll(
  ScreenGrid *message_grid, CompositorScrollRequest request);

#include "ui_compositor.h.generated.h"
