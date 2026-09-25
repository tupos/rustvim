// Compositor: merge floating grids with the main grid for display in
// TUI and non-multigrid UIs.
//
// Layer-based compositing: https://en.wikipedia.org/wiki/Digital_compositing

#include <assert.h>
#include <stdbool.h>

#include "nvim/api/private/defs.h"
#include "nvim/buffer_defs.h"
#include "nvim/globals.h"
#include "nvim/grid.h"
#include "nvim/message.h"
#include "nvim/types_defs.h"
#include "nvim/ui.h"
#include "nvim/ui_compositor.h"

#include "ui_compositor.c.generated.h"

void ui_comp_init(void)
{
  bool initialized = ui_comp_initialize_layer_registry(&default_grid);
  assert(initialized);
  (void)initialized;
}

#ifdef EXITFREE
void ui_comp_free_all_mem(void)
{
  bool cleared = ui_comp_clear_layer_registry();
  assert(cleared);
  (void)cleared;

  bool released = ui_comp_release_buffer();
  assert(released);
  (void)released;
}
#endif

void ui_comp_syn_init(void)
{
  ui_comp_initialize_debug_highlights();
}

void ui_comp_attach(RemoteUI *ui)
{
  ui_comp_register_composed_ui();
  ui->composed = true;
}

void ui_comp_detach(RemoteUI *ui)
{
  if (ui_comp_unregister_composed_ui()) {
    bool released = ui_comp_release_buffer();
    assert(released);
    (void)released;
  }
  ui->composed = false;
}

/// Raises or lowers the layer, syncing comp_index with zindex.
///
/// This function adjusts the position of a layer in the Rust registry based
/// on its zindex, either raising or lowering it.
///
/// @param[in]  grid   Grid whose layer is to be raised or lowered.
/// @param[in]  raise  Raise the layer if true, else lower it.
void ui_comp_layers_adjust(ScreenGrid *grid, bool raise)
{
  bool adjusted = ui_comp_adjust_grid_layer_order(grid->handle, raise);
  assert(adjusted);
  (void)adjusted;
}

/// Places `grid` at (col,row) position with (width * height) size.
/// Adds `grid` as the top layer if it is a new layer.
///
/// TODO(bfredl): later on the compositor should just use win_float_pos events,
/// though that will require slight event order adjustment: emit the win_pos
/// events in the beginning of update_screen(), rather than in ui_flush()
bool ui_comp_put_grid(ScreenGrid *grid, int row, int col, int height, int width, bool valid,
                      bool on_top)
{
  bool moved = false;
  CompositorGridPlacementRequest request = {
    .row = row,
    .column = col,
    .height = height,
    .width = width,
    .valid = valid,
    .on_top = on_top,
  };

  Integer current_window_grid_handle = curwin ? curwin->w_grid_alloc.handle : 0;
  bool placed = ui_comp_process_grid_placement(
    grid, current_window_grid_handle,
    request, &msg_grid, &moved);
  assert(placed);
  return placed && moved;
}

void ui_comp_remove_grid(ScreenGrid *grid)
{
  assert(grid != &default_grid);
  bool valid = ui_comp_process_grid_removal(&msg_grid, grid->handle);
  assert(valid);
  (void)valid;
}

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

/// compose the area under the grid.
///
/// This is needed when some option affecting composition is changed,
/// such as 'pumblend' for popupmenu grid.
void ui_comp_compose_grid(ScreenGrid *grid)
{
  bool recomposed = ui_comp_recompose_grid(&msg_grid, grid->handle);
  assert(recomposed);
  (void)recomposed;
}
