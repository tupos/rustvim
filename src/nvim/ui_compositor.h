#pragma once

#include <stdbool.h>
#include <stddef.h>

#include "nvim/api/private/defs.h"  // IWYU pragma: keep
#include "nvim/grid_defs.h"  // IWYU pragma: keep
#include "nvim/types_defs.h"  // IWYU pragma: keep
#include "nvim/ui_defs.h"  // IWYU pragma: keep

extern void ui_comp_init(void);
extern void ui_comp_free_all_mem(void);
extern void ui_comp_syn_init(void);
extern void ui_comp_attach(void);
extern void ui_comp_detach(void);
extern void ui_comp_layers_adjust(ScreenGrid *grid, bool raise);
extern bool ui_comp_put_grid(
  ScreenGrid *grid, int row, int column, int height, int width,
  bool valid, bool on_top, Integer current_window_grid_handle);
extern void ui_comp_remove_grid(ScreenGrid *grid);
extern void ui_comp_compose_grid(ScreenGrid *grid);

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

extern void ui_comp_hide_message_separator(void);

#include "ui_compositor.h.generated.h"
