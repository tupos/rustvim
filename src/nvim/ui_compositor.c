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

static int composed_uis = 0;
kvec_t(ScreenGrid *) layers = KV_INITIAL_VALUE;

#ifndef NDEBUG
static int chk_width = 0, chk_height = 0;
#endif

static ScreenGrid *curgrid;

static bool valid_screen = true;
static int msg_current_row = INT_MAX;
static bool msg_was_scrolled = false;

static int msg_sep_row = -1;
static schar_T msg_sep_char = schar_from_ascii(' ');

static int dbghl_normal, dbghl_clear, dbghl_composed, dbghl_recompose;

void ui_comp_init(void)
{
  kv_push(layers, &default_grid);
  curgrid = &default_grid;
}

#ifdef EXITFREE
void ui_comp_free_all_mem(void)
{
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
  composed_uis++;
  ui->composed = true;
}

void ui_comp_detach(RemoteUI *ui)
{
  composed_uis--;
  if (composed_uis == 0) {
    bool released = ui_comp_release_buffer();
    assert(released);
    (void)released;
  }
  ui->composed = false;
}

bool ui_comp_should_draw(void)
{
  return composed_uis != 0 && valid_screen;
}

/// Raises or lowers the layer, syncing comp_index with zindex.
///
/// This function adjusts the position of a layer in the layers array
/// based on its zindex, either raising or lowering it.
///
/// @param[in]  layer_idx  Index of the layer to be raised or lowered.
/// @param[in]  raise      Raise the layer if true, else lower it.
void ui_comp_layers_adjust(size_t layer_idx, bool raise)
{
  bool adjusted = ui_comp_adjust_layer_order(
    layers.items, layers.size, layer_idx, raise);
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
  bool moved;
  grid->pending_comp_index_update = true;

  if (grid->comp_index != 0) {
    moved = (row != grid->comp_row) || (col != grid->comp_col);
    if (ui_comp_should_draw()) {
      // Redraw the area covered by the old position, and is not covered
      // by the new position. Disable the grid so that compose_area() will not
      // use it.
      grid->comp_disabled = true;
      compose_area(grid->comp_row, row,
                   grid->comp_col, grid->comp_col + grid->comp_width);
      if (grid->comp_col < col) {
        compose_area(MAX(row, grid->comp_row),
                     MIN(row + height, grid->comp_row + grid->comp_height),
                     grid->comp_col, col);
      }
      if (col + width < grid->comp_col + grid->comp_width) {
        compose_area(MAX(row, grid->comp_row),
                     MIN(row + height, grid->comp_row + grid->comp_height),
                     col + width, grid->comp_col + grid->comp_width);
      }
      compose_area(row + height, grid->comp_row + grid->comp_height,
                   grid->comp_col, grid->comp_col + grid->comp_width);
      grid->comp_disabled = false;
    }
    grid->comp_row = row;
    grid->comp_col = col;
  } else {
    moved = true;
#ifndef NDEBUG
    for (size_t i = 0; i < kv_size(layers); i++) {
      if (kv_A(layers, i) == grid) {
        abort();
      }
    }
#endif

    size_t insert_at = kv_size(layers);
    while (insert_at > 0 && kv_A(layers, insert_at - 1)->zindex > grid->zindex) {
      insert_at--;
    }

    if (curwin && kv_A(layers, insert_at - 1) == &curwin->w_grid_alloc
        && kv_A(layers, insert_at - 1)->zindex == grid->zindex
        && !on_top) {
      insert_at--;
    }
    // not found: new grid
    kv_pushp(layers);
    for (size_t i = kv_size(layers) - 1; i > insert_at; i--) {
      kv_A(layers, i) = kv_A(layers, i - 1);
      kv_A(layers, i)->comp_index = i;
      kv_A(layers, i)->pending_comp_index_update = true;
    }
    kv_A(layers, insert_at) = grid;

    grid->comp_row = row;
    grid->comp_col = col;
    grid->comp_index = insert_at;
    grid->pending_comp_index_update = true;
  }

  grid->comp_height = height;
  grid->comp_width = width;
  if (moved && valid && ui_comp_should_draw()) {
    compose_area(grid->comp_row, grid->comp_row + grid->rows,
                 grid->comp_col, grid->comp_col + grid->cols);
  }
  return moved;
}

void ui_comp_remove_grid(ScreenGrid *grid)
{
  assert(grid != &default_grid);
  bool removed = false;
  bool valid = ui_comp_remove_layer(
    layers.items, layers.size, grid, &removed);
  assert(valid);
  if (!valid || !removed) {
    return;
  }

  if (curgrid == grid) {
    curgrid = &default_grid;
  }

  (void)kv_pop(layers);

  // recompose the area under the grid
  // inefficient when being overlapped: only draw up to grid->comp_index
  ui_comp_compose_grid(grid);
}

bool ui_comp_set_grid(handle_T handle)
{
  if (curgrid->handle == handle) {
    return true;
  }
  ScreenGrid *grid = NULL;
  for (size_t i = 0; i < kv_size(layers); i++) {
    if (kv_A(layers, i)->handle == handle) {
      grid = kv_A(layers, i);
      break;
    }
  }
  if (grid != NULL) {
    curgrid = grid;
    return true;
  }
  return false;
}

void ui_comp_raise_grid(ScreenGrid *grid, size_t new_index)
{
  bool raised = ui_comp_raise_layer(
    layers.items, layers.size, grid, new_index, compose_area);
  assert(raised);
  (void)raised;
}

void ui_comp_grid_cursor_goto(Integer grid_handle, Integer r, Integer c)
{
  if (!ui_comp_set_grid((int)grid_handle)) {
    return;
  }
  int cursor_row = curgrid->comp_row + (int)r;
  int cursor_col = curgrid->comp_col + (int)c;

  // TODO(bfredl): maybe not the best time to do this, for efficiency we
  // should configure all grids before entering win_update()
  if (curgrid != &default_grid) {
    size_t new_index = kv_size(layers) - 1;

    while (new_index > 1 && kv_A(layers, new_index)->zindex > curgrid->zindex) {
      new_index--;
    }

    if (curgrid->comp_index < new_index) {
      ui_comp_raise_grid(curgrid, new_index);
    }
  }

  if (cursor_col >= default_grid.cols || cursor_row >= default_grid.rows) {
    // TODO(bfredl): this happens with 'writedelay', refactor?
    // abort();
    return;
  }
  ui_composed_call_grid_cursor_goto(1, cursor_row, cursor_col);
}

ScreenGrid *ui_comp_mouse_focus(int row, int col)
{
  size_t layer_index = ui_comp_find_layer_at_coord(
    layers.items, layers.size, row, col, true);
  if (layer_index < kv_size(layers)) {
    return kv_A(layers, layer_index);
  }

  if (ui_has(kUIMultigrid)) {
    FOR_ALL_WINDOWS_IN_TAB(wp, curtab) {
      ScreenGrid *grid = &wp->w_grid_alloc;
      if (grid->mouse_enabled && row >= wp->w_winrow && row < wp->w_winrow + grid->rows
          && col >= wp->w_wincol && col < wp->w_wincol + grid->cols) {
        return grid;
      }
    }
  }
  return NULL;
}

/// Compute which grid is on top at supplied screen coordinates
ScreenGrid *ui_comp_get_grid_at_coord(int row, int col)
{
  size_t layer_index = ui_comp_find_layer_at_coord(
    layers.items, layers.size, row, col, false);
  if (layer_index < kv_size(layers)) {
    return kv_A(layers, layer_index);
  }

  FOR_ALL_WINDOWS_IN_TAB(wp, curtab) {
    ScreenGrid *grid = &wp->w_grid_alloc;
    if (row >= grid->comp_row && row < grid->comp_row + grid->rows
        && col >= grid->comp_col && col < grid->comp_col + grid->cols
        && !wp->w_config.hide) {
      return grid;
    }
  }
  return &default_grid;
}

/// Baseline implementation. This is always correct, but we can sometimes
/// do something more efficient (where efficiency means smaller deltas to
/// the downstream UI.)
static void compose_line(Integer row, Integer startcol, Integer endcol, LineFlags flags)
{
  assert(endcol <= chk_width);
  assert(row < chk_height);

  CompositorLineRequest request = {
    .row = row,
    .start_col = startcol,
    .end_col = endcol,
    .flags = flags,
    .message_separator_row = msg_sep_row,
    .message_separator_character = msg_sep_char,
    .message_separator_attribute =
      row == msg_sep_row ? (sattr_T)HL_ATTR(HLF_MSGSEP) : 0,
    .screen_columns = Columns,
    .abort_on_invalid = (rdb_flags & kOptRdbFlagInvalid) != 0,
  };
  bool composed = ui_comp_compose_line(
    layers.items, layers.size, &msg_grid, request);
  assert(composed);
  (void)composed;
}

static void compose_debug(Integer startrow, Integer endrow, Integer startcol, Integer endcol,
                          int syn_id, bool delay)
{
  if (!(rdb_flags & kOptRdbFlagCompositor) || startcol >= endcol) {
    return;
  }

  endrow = MIN(endrow, default_grid.rows);
  endcol = MIN(endcol, default_grid.cols);
  int attr = syn_id2attr(syn_id);

  if (delay) {
    debug_delay(endrow - startrow);
  }

  for (int row = (int)startrow; row < endrow; row++) {
    bool emitted = ui_comp_emit_clear_line(row, startcol, endcol, attr);
    assert(emitted);
    (void)emitted;
  }

  if (delay) {
    debug_delay(endrow - startrow);
  }
}

static void debug_delay(Integer lines)
{
  ui_call_flush();
  uint64_t wd = (uint64_t)llabs(p_wd);
  uint64_t factor = (uint64_t)MAX(MIN(lines, 5), 1);
  os_sleep(factor * wd);
}

static void compose_area(Integer startrow, Integer endrow, Integer startcol, Integer endcol)
{
  compose_debug(startrow, endrow, startcol, endcol, dbghl_recompose, true);
  endrow = MIN(endrow, default_grid.rows);
  endcol = MIN(endcol, default_grid.cols);
  if (endcol <= startcol) {
    return;
  }
  for (int r = (int)startrow; r < endrow; r++) {
    compose_line(r, startcol, endcol, kLineFlagInvalid);
  }
}

/// compose the area under the grid.
///
/// This is needed when some option affecting composition is changed,
/// such as 'pumblend' for popupmenu grid.
void ui_comp_compose_grid(ScreenGrid *grid)
{
  if (ui_comp_should_draw()) {
    compose_area(grid->comp_row, grid->comp_row + grid->rows,
                 grid->comp_col, grid->comp_col + grid->cols);
  }
}

void ui_comp_raw_line(Integer grid, Integer row, Integer startcol, Integer endcol, Integer clearcol,
                      Integer clearattr, LineFlags flags, const schar_T *chunk,
                      const sattr_T *attrs)
{
  if (!ui_comp_should_draw() || !ui_comp_set_grid((int)grid)) {
    return;
  }

  assert(endcol <= clearcol);

  CompositorRawLineRequest request = {
    .row = row,
    .start_col = startcol,
    .end_col = endcol,
    .clear_col = clearcol,
    .flags = flags,
    .message_was_scrolled = msg_was_scrolled,
  };
  CompositorRawLinePlan plan;
  bool planned = ui_comp_plan_raw_line(
    layers.items, layers.size, curgrid, &msg_grid, request, &plan);
  assert(planned);
  if (!planned) {
    return;
  }

  // TODO(bfredl): this should not really be necessary. But on some condition
  // when resizing nvim, a window will be attempted to be drawn on the older
  // and possibly larger global screen size.
  if (plan.row_outside) {
    DLOG("compositor: invalid row %" PRId64 " on grid %" PRId64, plan.row, grid);
    return;
  }
  if (plan.columns_clipped) {
    DLOG("compositor: invalid last column %" PRId64 " on grid %" PRId64,
         plan.requested_clear_col, grid);
  }
  if (plan.discard) {
    return;
  }

  // TODO(bfredl): eventually should just fix compose_line to respect clearing
  // and optimize it for uncovered lines.
  if (plan.recompose) {
    compose_debug(
      plan.row, plan.row + 1, plan.start_col, plan.clear_col, dbghl_composed, true);
    compose_line(plan.row, plan.start_col, plan.clear_col, plan.flags);
  } else {
    compose_debug(
      plan.row, plan.row + 1, plan.start_col, plan.end_col,
      dbghl_normal, plan.end_col >= plan.clear_col);
    compose_debug(
      plan.row, plan.row + 1, plan.end_col, plan.clear_col, dbghl_clear, true);
#ifndef NDEBUG
    for (Integer i = 0; i < plan.end_col - plan.start_col; i++) {
      assert(attrs[i] >= 0);
    }
#endif
    ui_composed_call_raw_line(
      1, plan.row, plan.start_col, plan.end_col, plan.clear_col,
      clearattr, plan.flags, chunk, attrs);
  }
}

/// The screen is invalid and will soon be cleared
///
/// Don't redraw floats until screen is cleared
bool ui_comp_set_screen_valid(bool valid)
{
  bool old_val = valid_screen;
  valid_screen = valid;
  if (!valid) {
    msg_sep_row = -1;
  }
  return old_val;
}

void ui_comp_msg_set_pos(Integer grid, Integer row, Boolean scrolled, String sep_char,
                         Integer zindex, Integer compindex)
{
  msg_grid.pending_comp_index_update = true;
  msg_grid.comp_row = (int)row;
  if (scrolled && row > 0) {
    msg_sep_row = (int)row - 1;
    if (sep_char.data) {
      msg_sep_char = schar_from_buf(sep_char.data, sep_char.size);
    }
  } else {
    msg_sep_row = -1;
  }

  if (row > msg_current_row && ui_comp_should_draw()) {
    compose_area(MAX(msg_current_row - 1, 0), row, 0, default_grid.cols);
  } else if (row < msg_current_row && ui_comp_should_draw()
             && (msg_current_row < Rows || (scrolled && !msg_was_scrolled))) {
    int delta = msg_current_row - (int)row;
    if (msg_grid.blending) {
      int first_row = MAX((int)row - (scrolled ? 1 : 0), 0);
      compose_area(first_row, Rows - delta, 0, Columns);
    } else {
      // scroll separator together with message text
      int first_row = MAX((int)row - (msg_was_scrolled ? 1 : 0), 0);
      ui_composed_call_grid_scroll(1, first_row, Rows, 0, Columns, delta, 0);
      if (scrolled && !msg_was_scrolled && row > 0) {
        compose_area(row - 1, row, 0, Columns);
      }
    }
  }

  msg_current_row = (int)row;
  msg_was_scrolled = scrolled;
}

void ui_comp_grid_scroll(Integer grid, Integer top, Integer bot, Integer left, Integer right,
                         Integer rows, Integer cols)
{
  if (!ui_comp_should_draw() || !ui_comp_set_grid((int)grid)) {
    return;
  }

  CompositorScrollRequest request = {
    .top = top,
    .bottom = bot,
    .left = left,
    .right = right,
    .row_delta = rows,
  };
  CompositorScrollPlan plan;
  bool planned = ui_comp_plan_scroll(
    layers.items, layers.size, curgrid, &msg_grid, msg_was_scrolled, request, &plan);
  assert(planned);
  if (!planned) {
    return;
  }

  if (plan.recompose) {
    // TODO(bfredl):
    // 1. check if rectangles actually overlap
    // 2. calculate subareas that can scroll.
    compose_debug(plan.top, plan.bottom, plan.left, plan.right, dbghl_recompose, true);
    int grid_row = plan.first_grid_row;
    for (Integer row = plan.first_recompose_row; row < plan.recompose_end_row;
         row++, grid_row++) {
      // TODO(bfredl): workaround for win_update() performing two scrolls in a
      // row, where the latter might scroll invalid space created by the first.
      // ideally win_update() should keep track of this itself and not scroll
      // the invalid space.
      if (ui_comp_should_compose_scroll_row(
            curgrid, grid_row, plan.grid_start_column)) {
        compose_line(row, plan.left, plan.right, 0);
      }
    }
  } else {
    ui_composed_call_grid_scroll(
      1, plan.top, plan.bottom, plan.left, plan.right, rows, cols);
    if (rdb_flags & kOptRdbFlagCompositor) {
      debug_delay(2);
    }
  }
}

void ui_comp_grid_resize(Integer grid, Integer width, Integer height)
{
  if (grid == 1) {
    ui_composed_call_grid_resize(1, width, height);
#ifndef NDEBUG
    chk_width = (int)width;
    chk_height = (int)height;
#endif
    bool resized = ui_comp_resize_buffer(width);
    assert(resized);
    (void)resized;
  }
}
