// Compositor: merge floating grids with the main grid for display in
// TUI and non-multigrid UIs.
//
// Layer-based compositing: https://en.wikipedia.org/wiki/Digital_compositing

#include <stdbool.h>

#include "nvim/api/private/defs.h"
#include "nvim/buffer_defs.h"
#include "nvim/globals.h"
#include "nvim/grid.h"
#include "nvim/types_defs.h"
#include "nvim/ui.h"
#include "nvim/ui_compositor.h"

#include "ui_compositor.c.generated.h"

ScreenGrid *ui_comp_mouse_focus(int row, int col)
{
  ScreenGrid *layer_grid = ui_comp_find_grid_at_coord(row, col, true);
  if (layer_grid != NULL) {
    return layer_grid;
  }

  if (ui_has(kUIMultigrid)) {
    FOR_ALL_WINDOWS_IN_TAB(wp, curtab) {
      ScreenGrid *grid = &wp->w_grid_alloc;
      if (ui_comp_grid_accepts_mouse_at(
            grid, wp->w_winrow, wp->w_wincol, row, col)) {
        return grid;
      }
    }
  }
  return NULL;
}

/// Compute which grid is on top at supplied screen coordinates
ScreenGrid *ui_comp_get_grid_at_coord(int row, int col)
{
  ScreenGrid *layer_grid = ui_comp_find_grid_at_coord(row, col, false);
  if (layer_grid != NULL) {
    return layer_grid;
  }

  FOR_ALL_WINDOWS_IN_TAB(wp, curtab) {
    ScreenGrid *grid = &wp->w_grid_alloc;
    if (ui_comp_grid_contains_at(grid, row, col) && !wp->w_config.hide) {
      return grid;
    }
  }
  return &default_grid;
}

bool ui_comp_cursor_is_covered_by_higher_layer(ScreenGrid *current_grid, int row, int col,
                                               int minimum_z_index_gap)
{
  return ui_comp_cursor_is_covered_by_higher_layer_in_stack(
    current_grid->handle, current_grid->zindex, row, col, minimum_z_index_gap);
}
