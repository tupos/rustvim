// Compositor: merge floating grids with the main grid for display in
// TUI and non-multigrid UIs.
//
// Layer-based compositing: https://en.wikipedia.org/wiki/Digital_compositing

#include <assert.h>
#include <inttypes.h>
#include <limits.h>
#include <stdbool.h>
#include <stdlib.h>
#include <uv.h>

#include "klib/kvec.h"
#include "nvim/api/private/defs.h"
#include "nvim/buffer_defs.h"
#include "nvim/globals.h"
#include "nvim/grid.h"
#include "nvim/highlight.h"
#include "nvim/highlight_defs.h"
#include "nvim/highlight_group.h"
#include "nvim/log.h"
#include "nvim/macros_defs.h"
#include "nvim/message.h"
#include "nvim/option_vars.h"
#include "nvim/os/time.h"
#include "nvim/types_defs.h"
#include "nvim/ui.h"
#include "nvim/ui_compositor.h"

#include "ui_compositor.c.generated.h"

kvec_t(ScreenGrid *) layers = KV_INITIAL_VALUE;

static int dbghl_normal, dbghl_clear, dbghl_composed, dbghl_recompose;

void ui_comp_init(void)
{
  bool initialized = ui_comp_initialize_layer_registry(default_grid.handle);
  assert(initialized);
  (void)initialized;
  kv_push(layers, &default_grid);
}

#ifdef EXITFREE
void ui_comp_free_all_mem(void)
{
  bool cleared = ui_comp_clear_layer_registry();
  assert(cleared);
  (void)cleared;
  kv_destroy(layers);

  bool released = ui_comp_release_buffer();
  assert(released);
  (void)released;
}
#endif

void ui_comp_syn_init(void)
{
  dbghl_normal = syn_check_group(S_LEN("RedrawDebugNormal"));
  dbghl_clear = syn_check_group(S_LEN("RedrawDebugClear"));
  dbghl_composed = syn_check_group(S_LEN("RedrawDebugComposed"));
  dbghl_recompose = syn_check_group(S_LEN("RedrawDebugRecompose"));
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
  bool adjusted = ui_comp_adjust_grid_layer_order(
    layers.items, layers.size, grid, raise);
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

  // Reserve and initialize one candidate slot without making it active.
  // Rust decides whether the grid is already registered and publishes a
  // larger layer count only when it inserts the candidate.
  kv_ensure_space(layers, 1);
  layers.items[layers.size] = grid;

  ScreenGrid *current_window_grid = curwin ? &curwin->w_grid_alloc : NULL;
  bool placed = ui_comp_place_grid_in_layer_stack(
    layers.items, &layers.size, grid, current_window_grid,
    request, &msg_grid, compositor_area_callbacks(),
    &moved);
  assert(placed);
  return placed && moved;
}

void ui_comp_remove_grid(ScreenGrid *grid)
{
  assert(grid != &default_grid);
  bool valid = ui_comp_remove_grid_from_layer_stack(
    layers.items, &layers.size, &msg_grid, grid,
    compositor_area_callbacks());
  assert(valid);
  (void)valid;
}

void ui_comp_grid_cursor_goto(Integer grid_handle, Integer r, Integer c)
{
  bool updated = ui_comp_grid_cursor_goto_in_layer_stack(
    layers.items, layers.size, &msg_grid, grid_handle, r, c,
    compositor_area_callbacks());
  assert(updated);
  (void)updated;
}

ScreenGrid *ui_comp_mouse_focus(int row, int col)
{
  ScreenGrid *layer_grid = ui_comp_find_grid_at_coord(
    layers.items, layers.size, row, col, true);
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
  ScreenGrid *layer_grid = ui_comp_find_grid_at_coord(
    layers.items, layers.size, row, col, false);
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
    layers.items, layers.size, current_grid, row, col, minimum_z_index_gap);
}

static CompositorLineSettings compositor_line_settings(void)
{
  return (CompositorLineSettings) {
    .message_separator_attribute = (sattr_T)HL_ATTR(HLF_MSGSEP),
    .screen_columns = Columns,
    .abort_on_invalid = (rdb_flags & kOptRdbFlagInvalid) != 0,
  };
}

static CompositorAreaCallbacks compositor_area_callbacks(void)
{
  return (CompositorAreaCallbacks) {
    .debug = compose_debug,
    .line_settings = compositor_line_settings(),
    .recomposed_debug_highlight = dbghl_recompose,
  };
}

static bool clip_area_to_default_grid(CompositorRecomposeArea *area)
{
  bool clipped = ui_comp_clip_recompose_area(&default_grid, *area, area);
  assert(clipped);
  return clipped;
}

static void compose_debug(Integer startrow, Integer endrow, Integer startcol, Integer endcol,
                          int syn_id, bool delay)
{
  if (!(rdb_flags & kOptRdbFlagCompositor) || startcol >= endcol) {
    return;
  }

  CompositorRecomposeArea area = {
    .start_row = startrow,
    .end_row = endrow,
    .start_column = startcol,
    .end_column = endcol,
  };
  if (!clip_area_to_default_grid(&area)) {
    return;
  }
  int attr = syn_id2attr(syn_id);

  if (delay) {
    debug_delay(area.end_row - area.start_row);
  }

  for (int row = (int)area.start_row; row < area.end_row; row++) {
    bool emitted =
      ui_comp_emit_clear_line(row, area.start_column, area.end_column, attr);
    assert(emitted);
    (void)emitted;
  }

  if (delay) {
    debug_delay(area.end_row - area.start_row);
  }
}

static void debug_delay(Integer lines)
{
  ui_call_flush();
  uint64_t wd = (uint64_t)llabs(p_wd);
  uint64_t factor = (uint64_t)MAX(MIN(lines, 5), 1);
  os_sleep(factor * wd);
}

/// compose the area under the grid.
///
/// This is needed when some option affecting composition is changed,
/// such as 'pumblend' for popupmenu grid.
void ui_comp_compose_grid(ScreenGrid *grid)
{
  bool recomposed = ui_comp_recompose_grid(
    layers.items, layers.size, &msg_grid, grid, compositor_area_callbacks());
  assert(recomposed);
  (void)recomposed;
}

static void log_invalid_raw_line_row(Integer row, Integer grid)
{
  DLOG("compositor: invalid row %" PRId64 " on grid %" PRId64, row, grid);
}

static void log_clipped_raw_line_columns(Integer requested_clear_col, Integer grid)
{
  DLOG("compositor: invalid last column %" PRId64 " on grid %" PRId64,
       requested_clear_col, grid);
}

void ui_comp_raw_line(Integer grid, Integer row, Integer startcol, Integer endcol, Integer clearcol,
                      Integer clearattr, LineFlags flags, const schar_T *chunk,
                      const sattr_T *attrs)
{
  CompositorRawLineRequest request = {
    .grid_handle = (int)grid,
    .row = row,
    .start_col = startcol,
    .end_col = endcol,
    .clear_col = clearcol,
    .clear_attr = clearattr,
    .flags = flags,
  };
  CompositorRawLineCallbacks callbacks = {
    .debug = compose_debug,
    .line_settings = compositor_line_settings(),
    .log_invalid_row = log_invalid_raw_line_row,
    .log_clipped_columns = log_clipped_raw_line_columns,
    .normal_debug_highlight = dbghl_normal,
    .clear_debug_highlight = dbghl_clear,
    .composed_debug_highlight = dbghl_composed,
  };
  bool processed = ui_comp_process_raw_line(
    layers.items, layers.size, &msg_grid, request, chunk, attrs, callbacks);
  assert(processed);
  (void)processed;
}

void ui_comp_msg_set_pos(Integer grid, Integer row, Boolean scrolled, String sep_char,
                         Integer zindex, Integer compindex)
{
  bool has_separator_character = sep_char.data != NULL;
  CompositorMessagePositionRequest request = {
    .row = row,
    .separator_character =
      has_separator_character ? schar_from_buf(sep_char.data, sep_char.size) : 0,
    .screen_rows = Rows,
    .screen_columns = Columns,
    .scrolled = scrolled,
    .has_separator_character = has_separator_character,
  };
  bool updated = ui_comp_update_message_position(
    layers.items, layers.size,
    &msg_grid, &default_grid,
    request, compositor_area_callbacks());
  assert(updated);
  (void)updated;
}

void ui_comp_grid_scroll(Integer grid, Integer top, Integer bot, Integer left, Integer right,
                         Integer rows, Integer cols)
{
  CompositorScrollRequest request = {
    .grid_handle = grid,
    .top = top,
    .bottom = bot,
    .left = left,
    .right = right,
    .row_delta = rows,
    .column_delta = cols,
  };
  CompositorScrollCallbacks callbacks = {
    .debug = compose_debug,
    .line_settings = compositor_line_settings(),
    .delay = debug_delay,
    .recomposed_debug_highlight = dbghl_recompose,
    .delay_after_forward = (rdb_flags & kOptRdbFlagCompositor) != 0,
  };
  bool processed = ui_comp_process_scroll(
    layers.items, layers.size, &msg_grid, request, callbacks);
  assert(processed);
  (void)processed;
}

void ui_comp_grid_resize(Integer grid, Integer width, Integer height)
{
  bool processed = ui_comp_process_grid_resize(grid, width, height);
  assert(processed);
  (void)processed;
}
