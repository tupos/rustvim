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
  Integer start_row;
  Integer end_row;
  Integer start_column;
  Integer end_column;
} CompositorRecomposeArea;

typedef struct {
  sattr_T message_separator_attribute;
  int screen_columns;
  bool abort_on_invalid;
} CompositorLineSettings;

typedef struct {
  Integer row;
  schar_T separator_character;
  int screen_rows;
  int screen_columns;
  bool scrolled;
  bool has_separator_character;
} CompositorMessagePositionRequest;

typedef void (*CompositorDebugCallback)(
  Integer start_row, Integer end_row, Integer start_column, Integer end_column,
  int highlight, bool delay);

typedef struct {
  CompositorDebugCallback debug;
  CompositorLineSettings line_settings;
  int recomposed_debug_highlight;
} CompositorAreaCallbacks;

typedef void (*CompositorDelayCallback)(Integer lines);

typedef void (*CompositorRawLineDiagnosticCallback)(
  Integer value, Integer grid_handle);

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
  CompositorDebugCallback debug;
  CompositorLineSettings line_settings;
  CompositorRawLineDiagnosticCallback log_invalid_row;
  CompositorRawLineDiagnosticCallback log_clipped_columns;
  int normal_debug_highlight;
  int clear_debug_highlight;
  int composed_debug_highlight;
} CompositorRawLineCallbacks;

typedef struct {
  Integer grid_handle;
  Integer top;
  Integer bottom;
  Integer left;
  Integer right;
  Integer row_delta;
  Integer column_delta;
} CompositorScrollRequest;

typedef struct {
  CompositorDebugCallback debug;
  CompositorLineSettings line_settings;
  CompositorDelayCallback delay;
  int recomposed_debug_highlight;
  bool delay_after_forward;
} CompositorScrollCallbacks;

extern void ui_comp_register_composed_ui(void);
extern bool ui_comp_unregister_composed_ui(void);
extern bool ui_comp_set_screen_valid(bool valid);

extern bool ui_comp_process_grid_resize(
  Integer grid, Integer width, Integer height);
extern bool ui_comp_release_buffer(void);

extern bool ui_comp_place_grid_in_layer_stack(
  ScreenGrid **layer_grids, size_t *layer_count,
  ScreenGrid *grid, ScreenGrid *current_window_grid,
  CompositorGridPlacementRequest request,
  ScreenGrid *message_grid, CompositorAreaCallbacks callbacks,
  bool *moved);

extern bool ui_comp_clip_recompose_area(
  ScreenGrid *grid, CompositorRecomposeArea area,
  CompositorRecomposeArea *output);

extern bool ui_comp_recompose_grid(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *message_grid, ScreenGrid *grid, CompositorAreaCallbacks callbacks);

extern bool ui_comp_grid_cursor_goto_in_layer_stack(
  ScreenGrid **layer_grids, size_t layer_count,
  ScreenGrid *message_grid, Integer grid_handle,
  Integer row, Integer column,
  CompositorAreaCallbacks callbacks);

extern bool ui_comp_adjust_layer_order(
  ScreenGrid **layer_grids, size_t layer_count,
  size_t layer_index, bool raise);

extern bool ui_comp_remove_grid_from_layer_stack(
  ScreenGrid **layer_grids, size_t *layer_count,
  ScreenGrid *message_grid, ScreenGrid *grid,
  CompositorAreaCallbacks callbacks);

extern size_t ui_comp_find_layer_at_coord(
  ScreenGrid *const *layer_grids, size_t layer_count,
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
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *current_grid, int row, int column, int minimum_z_index_gap);

extern void ui_comp_hide_message_separator(void);

extern bool ui_comp_update_message_position(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *message_grid, ScreenGrid *default_grid,
  CompositorMessagePositionRequest request, CompositorAreaCallbacks callbacks);

extern bool ui_comp_emit_clear_line(
  Integer row, Integer start_col, Integer end_col, sattr_T attribute);

extern bool ui_comp_process_raw_line(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *message_grid, CompositorRawLineRequest request,
  const schar_T *characters, const sattr_T *attributes,
  CompositorRawLineCallbacks callbacks);

extern bool ui_comp_process_scroll(
  ScreenGrid *const *layer_grids, size_t layer_count,
  ScreenGrid *message_grid, CompositorScrollRequest request,
  CompositorScrollCallbacks callbacks);

#include "ui_compositor.h.generated.h"
