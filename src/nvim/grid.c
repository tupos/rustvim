// Low-level functions to manipulate individual character cells on the
// screen grid.
//
// Most of the routines in this file perform screen (grid) manipulations. The
// given operation is performed physically on the screen. The corresponding
// change is also made to the internal screen image. In this way, the editor
// anticipates the effect of editing changes on the appearance of the screen.
// That way, when we call update_screen() a complete redraw isn't usually
// necessary. Another advantage is that we can keep adding code to anticipate
// screen changes, and in the meantime, everything still works.
//
// The grid_*() functions write to the screen and handle updating grid->lines[].

#include <stdint.h>

#include "nvim/api/private/defs.h"
#include "nvim/ascii_defs.h"
#include "nvim/buffer_defs.h"
#include "nvim/decoration.h"
#include "nvim/globals.h"
#include "nvim/grid.h"
#include "nvim/highlight.h"
#include "nvim/map_defs.h"
#include "nvim/mbyte.h"
#include "nvim/memory.h"
#include "nvim/message.h"
#include "nvim/option_vars.h"
#include "nvim/optionstr.h"
#include "nvim/types_defs.h"
#include "nvim/ui.h"
#include "nvim/ui_defs.h"

#include "grid.c.generated.h"

// temporary buffer for rendering a single screenline, so it can be
// compared with previous contents to calculate smallest delta.
// Per-cell attributes
size_t linebuf_size = 0;

int last_grid_handle = DEFAULT_GRID_HANDLE;

#ifdef ORDER_BIG_ENDIAN
# define schar_idx(sc) (sc & (0x00FFFFFF))
#else
# define schar_idx(sc) (sc >> 8)
#endif

#ifdef EXITFREE
/// Doesn't allow reinit, so must only be called by free_all_mem!
void grid_free_all_mem(void)
{
  grid_free(&default_grid);
  grid_free(&msg_grid);
  XFREE_CLEAR(msg_grid.dirty_col);
  xfree(linebuf_char);
  xfree(linebuf_attr);
  xfree(linebuf_vcol);
  xfree(linebuf_scratch);
  set_destroy(glyph, &glyph_cache);
}
#endif

/// (Re)allocates a window grid if size changed while in ext_multigrid mode.
/// Updates size, offsets and handle for the grid regardless.
///
/// If "doclear" is true, don't try to copy from the old grid rather clear the
/// resized grid.
void win_grid_alloc(win_T *wp)
{
  GridView *grid = &wp->w_grid;
  ScreenGrid *grid_allocated = &wp->w_grid_alloc;

  int total_rows = wp->w_height_outer;
  int total_cols = wp->w_width_outer;

  bool want_allocation = ui_has(kUIMultigrid) || wp->w_floating;
  bool has_allocation = (grid_allocated->chars != NULL);

  if (wp->w_view_height > wp->w_lines_size) {
    wp->w_lines_valid = 0;
    xfree(wp->w_lines);
    wp->w_lines = xcalloc((size_t)wp->w_view_height + 1, sizeof(wline_T));
    wp->w_lines_size = wp->w_view_height;
  }

  bool was_resized = false;
  if (want_allocation && (!has_allocation
                          || grid_allocated->rows != total_rows
                          || grid_allocated->cols != total_cols)) {
    grid_alloc(grid_allocated, total_rows, total_cols, wp->w_grid_alloc.valid, false);
    grid_allocated->valid = true;
    if (wp->w_floating && wp->w_config.border) {
      wp->w_redr_border = true;
    }
    was_resized = true;
  } else if (!want_allocation && has_allocation) {
    // Single grid mode, all rendering will be redirected to default_grid.
    // Only keep track of the size and offset of the window.
    grid_free(grid_allocated);
    grid_allocated->valid = false;
    was_resized = true;
  } else if (want_allocation && has_allocation && !wp->w_grid_alloc.valid) {
    grid_invalidate(grid_allocated);
    grid_allocated->valid = true;
  }

  if (want_allocation) {
    grid->target = grid_allocated;
    grid->row_offset = wp->w_winrow_off;
    grid->col_offset = wp->w_wincol_off;
  } else {
    grid->target = &default_grid;
    grid->row_offset = wp->w_winrow + wp->w_winrow_off;
    grid->col_offset = wp->w_wincol + wp->w_wincol_off;
  }

  // send grid resize event if:
  // - a grid was just resized
  // - screen_resize was called and all grid sizes must be sent
  // - the UI wants multigrid event (necessary)
  if ((resizing_screen || was_resized) && want_allocation) {
    ui_call_grid_resize(grid_allocated->handle,
                        grid_allocated->cols, grid_allocated->rows);
    ui_check_cursor_grid(grid_allocated->handle);
  }
}

/// @param overflow Number of cells to skip.
static void grid_draw_bordertext(VirtText vt, int col, int winbl, const int *hl_attr,
                                 int win_hl_attr_normal, BorderTextType bt, int overflow)
{
  int default_attr = hl_attr[bt == kBorderTextTitle ? HLF_BTITLE : HLF_BFOOTER];
  default_attr = hl_apply_winblend(winbl, default_attr);
  default_attr = hl_combine_attr(win_hl_attr_normal, default_attr);

  if (overflow > 0) {
    grid_line_puts(1, "<", -1, default_attr);
    col += 1;
    overflow += 1;
  }

  for (size_t i = 0; i < kv_size(vt);) {
    int attr = -1;
    char *text = next_virt_text_chunk(vt, &i, &attr);
    if (text == NULL) {
      break;
    }
    // Skip characters from the beginning when title overflows available width.
    if (overflow > 0) {
      int cells = (int)mb_string2cells(text);
      // Skip entire chunk if overflow is larger than chunk width.
      if (overflow >= cells) {
        overflow -= cells;
        continue;
      }
      // Skip partial characters within the chunk.
      char *p = text;
      while (*p && overflow > 0) {
        overflow -= utf_ptr2cells(p);
        p += utfc_ptr2len(p);
      }
      text = p;
    }
    if (attr == -1) {  // No highlight specified.
      attr = default_attr;
    } else {
      attr = hl_apply_winblend(winbl, attr);
      attr = hl_combine_attr(win_hl_attr_normal, attr);
    }
    col += grid_line_puts(col, text, -1, attr);
  }
}

static int get_bordertext_col(int total_col, int text_width, AlignTextPos align)
{
  switch (align) {
  case kAlignLeft:
    return 1;
  case kAlignCenter:
    return MAX((total_col - text_width) / 2 + 1, 1);
  case kAlignRight:
    return MAX(total_col - text_width + 1, 1);
  }
  UNREACHABLE;
}

/// draw border on floating window grid
void grid_draw_border(ScreenGrid *grid, WinConfig *config, int *adj, int winbl, int *hl_attr,
                      int win_hl_attr_normal)
{
  int default_adj[4] = { 1, 1, 1, 1 };
  if (adj == NULL) {
    adj = default_adj;
  }
  schar_T chars[8];
  int border_attrs[8];
  if (!hl_attr) {
    hl_attr = hl_attr_active;
  }

  for (int i = 0; i < 8; i++) {
    chars[i] = schar_from_str(config->border_chars[i]);
    border_attrs[i] = hl_combine_attr(win_hl_attr_normal, config->border_attr[i]);
  }

  int irow = grid->rows - adj[0] - adj[2];
  int icol = grid->cols - adj[1] - adj[3];

  if (adj[0]) {
    screengrid_line_start(grid, 0, 0);
    if (adj[3]) {
      grid_line_put_schar(0, chars[0], border_attrs[0]);
    }

    for (int i = 0; i < icol; i++) {
      grid_line_put_schar(i + adj[3], chars[1], border_attrs[1]);
    }

    if (config->title) {
      int title_col = get_bordertext_col(icol, config->title_width, config->title_pos);
      grid_draw_bordertext(config->title_chunks, title_col, winbl, hl_attr,
                           win_hl_attr_normal, kBorderTextTitle,
                           config->title_width - icol);
    }
    if (adj[1]) {
      grid_line_put_schar(icol + adj[3], chars[2], border_attrs[2]);
    }
    grid_line_flush();
  }

  for (int i = 0; i < irow; i++) {
    if (adj[3]) {
      screengrid_line_start(grid, i + adj[0], 0);
      grid_line_put_schar(0, chars[7], border_attrs[7]);
      grid_line_flush();
    }
    if (adj[1]) {
      int ic = (i == 0 && !adj[0] && chars[2]) ? 2 : 3;
      screengrid_line_start(grid, i + adj[0], 0);
      grid_line_put_schar(icol + adj[3], chars[ic], border_attrs[ic]);
      grid_line_flush();
    }
  }

  if (adj[2]) {
    screengrid_line_start(grid, irow + adj[0], 0);
    if (adj[3]) {
      grid_line_put_schar(0, chars[6], border_attrs[6]);
    }

    for (int i = 0; i < icol; i++) {
      int ic = (i == 0 && !adj[3] && chars[6]) ? 6 : 5;
      grid_line_put_schar(i + adj[3], chars[ic], border_attrs[ic]);
    }

    if (config->footer) {
      int footer_col = get_bordertext_col(icol, config->footer_width, config->footer_pos);
      grid_draw_bordertext(config->footer_chunks, footer_col, winbl, hl_attr, win_hl_attr_normal,
                           kBorderTextFooter, config->footer_width - icol);
    }
    if (adj[1]) {
      grid_line_put_schar(icol + adj[3], chars[4], border_attrs[4]);
    }
    grid_line_flush();
  }
}

win_T *get_win_by_grid_handle(handle_T handle)
{
  FOR_ALL_WINDOWS_IN_TAB(wp, curtab) {
    if (wp->w_grid_alloc.handle == handle) {
      return wp;
    }
  }
  return NULL;
}
