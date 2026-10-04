#pragma once

#include <stdbool.h>
#include <stddef.h>

#include "nvim/api/private/defs.h"  // IWYU pragma: keep
#include "nvim/grid_defs.h"  // IWYU pragma: keep
#include "nvim/types_defs.h"  // IWYU pragma: keep
#include "nvim/ui_defs.h"  // IWYU pragma: keep

typedef struct {
  handle_T grid_handle;
  int row;
  int column;
} CompositorGridHit;

extern void ui_comp_init(void);
extern void ui_comp_free_all_mem(void);
extern void ui_comp_syn_init(void);
extern void ui_comp_attach(void);
extern void ui_comp_detach(void);
extern void ui_comp_update_grid_layer(
  Integer grid_handle, int z_index, bool blending, bool accepts_mouse_input);
extern bool ui_comp_put_grid(
  Integer grid_handle, int row, int column, int height, int width,
  int z_index, bool blending, bool valid, bool on_top,
  bool accepts_mouse_input,
  Integer current_window_grid_handle);

extern bool ui_comp_set_screen_valid(bool valid);

extern void ui_comp_grid_resize(Integer grid, Integer width, Integer height);
extern void ui_comp_grid_cursor_goto(Integer grid, Integer row, Integer column);
extern void ui_comp_grid_scroll(
  Integer grid, Integer top, Integer bottom, Integer left, Integer right,
  Integer rows, Integer columns);
extern void ui_comp_raw_line(
  Integer grid, Integer row, Integer start_col, Integer end_col,
  Integer clear_col, Integer clear_attr, LineFlags flags,
  const schar_T *characters, const sattr_T *attributes);
extern void ui_comp_msg_set_pos(
  Integer grid, Integer row, Boolean scrolled, String separator_character,
  Integer z_index, Integer compositor_index);
extern bool ui_comp_get_cell_at_coord(
  int row, int column, GridCell *cell);
extern bool ui_comp_find_mouse_grid_at_coord(
  int row, int column, CompositorGridHit *hit);

extern bool ui_comp_grid_get_cell_at(
  Integer grid_handle, int origin_row, int origin_column,
  int row, int column, GridCell *cell);
extern bool ui_comp_grid_hit_at_coord(
  Integer grid_handle, int origin_row, int origin_column,
  int row, int column, CompositorGridHit *hit);

extern bool ui_comp_grid_publication_pending(Integer grid_handle);

extern bool ui_comp_cursor_is_covered_by_higher_layer(
  Integer current_grid_handle, int current_grid_z_index, int row, int column,
  int minimum_z_index_gap);
