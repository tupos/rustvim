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

#include <assert.h>
#include <limits.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#include "nvim/api/private/defs.h"
#include "nvim/ascii_defs.h"
#include "nvim/buffer_defs.h"
#include "nvim/decoration.h"
#include "nvim/globals.h"
#include "nvim/grid.h"
#include "nvim/highlight.h"
#include "nvim/log.h"
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

#ifdef VIM_IMPL
// DONE
static bool grid_invalid_row(ScreenGrid *grid, int row)
{
  return grid->attrs[grid->line_offset[row]] < 0;
}

static ScreenGrid *grid_line_grid = NULL;
static int grid_line_row = -1;
static int grid_line_coloff = 0;
static int grid_line_maxcol = 0;
static int grid_line_first = INT_MAX;
static int grid_line_last = 0;
static int grid_line_clear_to = 0;
static int grid_line_bg_attr = 0;
static int grid_line_clear_attr = 0;
static int grid_line_flags = 0;

/// DONE
/// Start a group of grid_line_puts calls that builds a single grid line.
///
/// Must be matched with a grid_line_flush call before moving to
/// another line.
void grid_line_start(GridView *view, int row)
{
  int col = 0;
  ScreenGrid *grid = grid_adjust(view, &row, &col);
  screengrid_line_start(grid, row, col);
}

void screengrid_line_start(ScreenGrid *grid, int row, int col)
{
  grid_line_maxcol = grid->cols;
  assert(grid_line_grid == NULL);
  grid_line_row = row;
  grid_line_grid = grid;
  grid_line_coloff = col;
  grid_line_first = (int)linebuf_size;
  grid_line_maxcol = MIN(grid_line_maxcol, grid->cols - grid_line_coloff);
  grid_line_last = 0;
  grid_line_clear_to = 0;
  grid_line_bg_attr = 0;
  grid_line_clear_attr = 0;
  grid_line_flags = 0;

  assert((size_t)grid_line_maxcol <= linebuf_size);

  if (full_screen && (rdb_flags & kOptRdbFlagInvalid)) {
    assert(linebuf_char);
    // Current batch must not depend on previous contents of linebuf_char.
    // Set invalid values which will cause assertion failures later if they are used.
    memset(linebuf_char, 0xFF, sizeof(schar_T) * linebuf_size);
    memset(linebuf_attr, 0xFF, sizeof(sattr_T) * linebuf_size);
  }
}

/// DONE
/// Get present char from current rendered screen line
///
/// This indicates what already is on screen, not the pending render buffer.
///
/// @return char or space if out of bounds
schar_T grid_line_getchar(int col, int *attr)
{
  if (col < grid_line_maxcol) {
    col += grid_line_coloff;
    size_t off = grid_line_grid->line_offset[grid_line_row] + (size_t)col;
    if (attr != NULL) {
      *attr = grid_line_grid->attrs[off];
    }
    return grid_line_grid->chars[off];
  } else {
    // NUL is a very special value (right-half of double width), space is True Neutral™
    return schar_from_ascii(' ');
  }
}

// DONE
void grid_line_put_schar(int col, schar_T schar, int attr)
{
  assert(grid_line_grid);
  if (col >= grid_line_maxcol) {
    return;
  }

  linebuf_char[col] = schar;
  linebuf_attr[col] = attr;

  grid_line_first = MIN(grid_line_first, col);
  // TODO(bfredl): Y U NO DOUBLEWIDTH?
  grid_line_last = MAX(grid_line_last, col + 1);
  linebuf_vcol[col] = -1;
}

/// Put string "text" at "col" position relative to the grid line from the
/// recent grid_line_start() call.
///
/// @param textlen length of string or -1 to use strlen(text)
/// Note: only outputs within one row!
///
/// @return number of grid cells used
int grid_line_puts(int col, const char *text, int textlen, int attr)
{
  const char *ptr = text;
  int len = textlen;

  assert(grid_line_grid);

  int start_col = col;

  const int max_col = grid_line_maxcol;
  while (col < max_col && (len < 0 || (int)(ptr - text) < len) && *ptr != NUL) {
    // check if this is the first byte of a multibyte
    int mbyte_blen;
    if (len >= 0) {
      int maxlen = (int)((text + len) - ptr);
      mbyte_blen = utfc_ptr2len_len(ptr, maxlen);
      if (mbyte_blen > maxlen) {
        mbyte_blen = 1;
      }
    } else {
      mbyte_blen = utfc_ptr2len(ptr);
    }
    int firstc;
    schar_T schar = utfc_ptrlen2schar(ptr, mbyte_blen, &firstc);
    int mbyte_cells = utf_ptr2cells_len(ptr, mbyte_blen);
    if (mbyte_cells > 2 || schar == 0) {
      mbyte_cells = 1;
      schar = schar_from_char(0xFFFD);
    }

    if (col + mbyte_cells > max_col) {
      // Only 1 cell left, but character requires 2 cells:
      // display a '>' in the last column to avoid wrapping.
      schar = schar_from_ascii('>');
      mbyte_cells = 1;
    }

    // When at the start of the text and overwriting the right half of a
    // two-cell character in the same grid, truncate that into a '>'.
    if (ptr == text && col > grid_line_first && col < grid_line_last
        && linebuf_char[col] == 0) {
      linebuf_char[col - 1] = schar_from_ascii('>');
    }

    linebuf_char[col] = schar;
    linebuf_attr[col] = attr;
    linebuf_vcol[col] = -1;
    if (mbyte_cells == 2) {
      linebuf_char[col + 1] = 0;
      linebuf_attr[col + 1] = attr;
      linebuf_vcol[col + 1] = -1;
    }

    col += mbyte_cells;
    ptr += mbyte_blen;
  }

  if (col > start_col) {
    grid_line_first = MIN(grid_line_first, start_col);
    grid_line_last = MAX(grid_line_last, col);
  }

  return col - start_col;
}

// DONE
int grid_line_fill(int start_col, int end_col, schar_T sc, int attr)
{
  end_col = MIN(end_col, grid_line_maxcol);
  if (start_col >= end_col) {
    return end_col;
  }

  for (int col = start_col; col < end_col; col++) {
    linebuf_char[col] = sc;
    linebuf_attr[col] = attr;
    linebuf_vcol[col] = -1;
  }

  grid_line_first = MIN(grid_line_first, start_col);
  grid_line_last = MAX(grid_line_last, end_col);
  return end_col;
}

// DONE
/// @param bg_attr     applies to both the buffered line and the columns to clear
/// @param clear_attr  applies only to the columns to clear
void grid_line_clear_end(int start_col, int end_col, int bg_attr, int clear_attr)
{
  if (grid_line_first > start_col) {
    grid_line_first = start_col;
    grid_line_last = start_col;
  }
  grid_line_clear_to = end_col;
  grid_line_bg_attr = bg_attr;
  grid_line_clear_attr = clear_attr;
}

/// DONE
/// move the cursor to a position in a currently rendered line.
void grid_line_cursor_goto(int col)
{
  ui_grid_cursor_goto(grid_line_grid->handle, grid_line_row, col);
}

// DONE
void grid_line_mirror(int width)
{
  grid_line_clear_to = MAX(grid_line_last, grid_line_clear_to);
  if (grid_line_first >= grid_line_clear_to) {
    return;
  }
  linebuf_mirror(&grid_line_first, &grid_line_last, &grid_line_clear_to, width);
  grid_line_flags |= SLF_RIGHTLEFT;
}

// DONE
void linebuf_mirror(int *firstp, int *lastp, int *clearp, int width)
{
  int first = *firstp;
  int last = *lastp;

  size_t n = (size_t)(last - first);
  int mirror = width - 1;  // Mirrors are more fun than television.
  schar_T *scratch_char = (schar_T *)linebuf_scratch;
  memcpy(scratch_char + first, linebuf_char + first, n * sizeof(schar_T));
  for (int col = first; col < last; col++) {
    int rev = mirror - col;
    if (col + 1 < last && scratch_char[col + 1] == 0) {
      linebuf_char[rev - 1] = scratch_char[col];
      linebuf_char[rev] = 0;
      col++;
    } else {
      linebuf_char[rev] = scratch_char[col];
    }
  }

  // for attr and vcol: assumes doublewidth chars are self-consistent
  sattr_T *scratch_attr = (sattr_T *)linebuf_scratch;
  memcpy(scratch_attr + first, linebuf_attr + first, n * sizeof(sattr_T));
  for (int col = first; col < last; col++) {
    linebuf_attr[mirror - col] = scratch_attr[col];
  }

  colnr_T *scratch_vcol = (colnr_T *)linebuf_scratch;
  memcpy(scratch_vcol + first, linebuf_vcol + first, n * sizeof(colnr_T));
  for (int col = first; col < last; col++) {
    linebuf_vcol[mirror - col] = scratch_vcol[col];
  }

  *firstp = width - *clearp;
  *clearp = width - first;
  *lastp = width - last;
}

/// DONE
/// End a group of grid_line_puts calls and send the screen buffer to the UI layer.
void grid_line_flush(void)
{
  ScreenGrid *grid = grid_line_grid;
  grid_line_grid = NULL;
  grid_line_clear_to = MAX(grid_line_last, grid_line_clear_to);
  assert(grid_line_clear_to <= grid_line_maxcol);
  if (grid_line_first >= grid_line_clear_to) {
    return;
  }

  grid_put_linebuf(grid, grid_line_row, grid_line_coloff, grid_line_first, grid_line_last,
                   grid_line_clear_to, grid_line_bg_attr, grid_line_clear_attr, -1,
                   grid_line_flags);
}

/// DONE
/// flush grid line but only if on a valid row
///
/// This is a stopgap until message.c has been refactored to behave
void grid_line_flush_if_valid_row(void)
{
  if (grid_line_row < 0 || grid_line_row >= grid_line_grid->rows) {
    if (rdb_flags & kOptRdbFlagInvalid) {
      abort();
    } else {
      grid_line_grid = NULL;
      return;
    }
  }
  grid_line_flush();
}

// DONE
void grid_clear(GridView *grid, int start_row, int end_row, int start_col, int end_col, int attr)
{
  for (int row = start_row; row < end_row; row++) {
    grid_line_start(grid, row);
    end_col = MIN(end_col, grid_line_maxcol);
    if (grid_line_row >= grid_line_grid->rows || start_col >= end_col) {
      grid_line_grid = NULL;  // TODO(bfredl): make callers behave instead
      return;
    }
    grid_line_clear_end(start_col, end_col, attr, 0);
    grid_line_flush();
  }
}

/// DONE
/// Check whether the given character needs redrawing:
/// - the (first byte of the) character is different
/// - the attributes are different
/// - the character is multi-byte and the next byte is different
/// - the character is two cells wide and the second cell differs.
static int grid_char_needs_redraw(ScreenGrid *grid, int col, size_t off_to, int cols)
{
  return (cols > 0
          && ((linebuf_char[col] != grid->chars[off_to]
               || linebuf_attr[col] != grid->attrs[off_to]
               || (cols > 1 && linebuf_char[col + 1] == 0
                   && linebuf_char[col + 1] != grid->chars[off_to + 1]))
              || rdb_flags & kOptRdbFlagNodelta));
}

/// DONE -- BUT CHECK IT ONCE AGAIN!!!
/// Move one buffered line to the window grid, but only the characters that
/// have actually changed.  Handle insert/delete character.
///
/// @param coloff  gives the first column on the grid for this line.
/// @param endcol  gives the columns where valid characters are.
/// @param clear_width  see SLF_RIGHTLEFT.
/// @param clear_attr   combined with "bg_attr" for the columns to clear.
/// @param flags  can have bits:
/// - SLF_RIGHTLEFT  rightleft text, like a window with 'rightleft' option set:
///   - When false, clear columns "endcol" to "clear_width".
///   - When true, clear columns "col" to "endcol".
/// - SLF_WRAP  hint to UI that "row" contains a line wrapped into the next row.
/// - SLF_INC_VCOL:
///   - When false, use "last_vcol" for grid->vcols[] of the columns to clear.
///   - When true, use an increasing sequence starting from "last_vcol + 1" for
///     grid->vcols[] of the columns to clear.
void grid_put_linebuf(ScreenGrid *grid, int row, int coloff, int col, int endcol, int clear_width,
                      int bg_attr, int clear_attr, colnr_T last_vcol, int flags)
{
  bool redraw_next;                         // redraw_this for next character
  bool clear_next = false;
  assert(0 <= row && row < grid->rows);
  // TODO(bfredl): check all callsites and eliminate
  // Check for illegal col, just in case
  if (endcol > grid->cols) {
    endcol = grid->cols;
  }

  // Safety check. Avoids clang warnings down the call stack.
  if (grid->chars == NULL || row >= grid->rows || coloff >= grid->cols) {
    DLOG("invalid state, skipped");
    return;
  }

  bool invalid_row = grid != &default_grid && grid_invalid_row(grid, row) && col == 0;
  size_t off_to = grid->line_offset[row] + (size_t)coloff;
  const size_t max_off_to = grid->line_offset[row] + (size_t)grid->cols;

  // When at the start of the text and overwriting the right half of a
  // two-cell character in the same grid, truncate that into a '>'.
  if (col > 0 && grid->chars[off_to + (size_t)col] == 0) {
    linebuf_char[col - 1] = schar_from_ascii('>');
    linebuf_attr[col - 1] = grid->attrs[off_to + (size_t)col - 1];
    col--;
  }

  int clear_start = endcol;
  if (flags & SLF_RIGHTLEFT) {
    clear_start = col;
    col = endcol;
    endcol = clear_width;
    clear_width = col;
  }

  if (p_arshape && !p_tbidi && endcol > col) {
    line_do_arabic_shape(linebuf_char + col, endcol - col);
  }

  if (bg_attr) {
    for (int c = col; c < endcol; c++) {
      linebuf_attr[c] = hl_combine_attr(bg_attr, linebuf_attr[c]);
    }
  }

  redraw_next = grid_char_needs_redraw(grid, col, off_to + (size_t)col, endcol - col);

  int start_dirty = -1;
  int end_dirty = 0;

  while (col < endcol) {
    int char_cells = 1;  // 1: normal char
                         // 2: occupies two display cells
    if (col + 1 < endcol && linebuf_char[col + 1] == 0) {
      char_cells = 2;
    }
    bool redraw_this = redraw_next;  // Does character need redraw?
    size_t off = off_to + (size_t)col;
    redraw_next = grid_char_needs_redraw(grid, col + char_cells,
                                         off + (size_t)char_cells,
                                         endcol - col - char_cells);

    if (redraw_this) {
      if (start_dirty == -1) {
        start_dirty = col;
      }
      end_dirty = col + char_cells;
      // When writing a single-width character over a double-width
      // character and at the end of the redrawn text, need to clear out
      // the right half of the old character.
      // Also required when writing the right half of a double-width
      // char over the left half of an existing one
      if (col + char_cells == endcol && off + (size_t)char_cells < max_off_to
          && grid->chars[off + (size_t)char_cells] == NUL) {
        clear_next = true;
      }

      grid->chars[off] = linebuf_char[col];
      if (char_cells == 2) {
        grid->chars[off + 1] = linebuf_char[col + 1];
      }

      grid->attrs[off] = linebuf_attr[col];
      // For simplicity set the attributes of second half of a
      // double-wide character equal to the first half.
      if (char_cells == 2) {
        grid->attrs[off + 1] = linebuf_attr[col];
      }
    }

    grid->vcols[off] = linebuf_vcol[col];
    if (char_cells == 2) {
      grid->vcols[off + 1] = linebuf_vcol[col + 1];
    }

    col += char_cells;
  }

  if (clear_next) {
    // Clear the second half of a double-wide character of which the left
    // half was overwritten with a single-wide character.
    grid->chars[off_to + (size_t)col] = schar_from_ascii(' ');
    end_dirty++;
  }

  // When clearing the left half of a double-wide char also clear the right half.
  if (off_to + (size_t)clear_width < max_off_to
      && grid->chars[off_to + (size_t)clear_width] == 0) {
    clear_width++;
  }

  int clear_dirty_start = -1, clear_end = -1;
  if (flags & SLF_RIGHTLEFT) {
    for (col = clear_width - 1; col >= clear_start; col--) {
      size_t off = off_to + (size_t)col;
      grid->vcols[off] = (flags & SLF_INC_VCOL) ? ++last_vcol : last_vcol;
    }
  }
  clear_attr = hl_combine_attr(bg_attr, clear_attr);
  // blank out the rest of the line
  // TODO(bfredl): we could cache winline widths
  for (col = clear_start; col < clear_width; col++) {
    size_t off = off_to + (size_t)col;
    if (grid->chars[off] != schar_from_ascii(' ')
        || grid->attrs[off] != clear_attr
        || rdb_flags & kOptRdbFlagNodelta) {
      grid->chars[off] = schar_from_ascii(' ');
      grid->attrs[off] = clear_attr;
      if (clear_dirty_start == -1) {
        clear_dirty_start = col;
      }
      clear_end = col + 1;
    }
    if (!(flags & SLF_RIGHTLEFT)) {
      grid->vcols[off] = (flags & SLF_INC_VCOL) ? ++last_vcol : last_vcol;
    }
  }

  if ((flags & SLF_RIGHTLEFT) && start_dirty != -1 && clear_dirty_start != -1) {
    if (grid->throttled || clear_dirty_start >= start_dirty - 5) {
      // cannot draw now or too small to be worth a separate "clear" event
      start_dirty = clear_dirty_start;
    } else {
      ui_line(grid, row, invalid_row, coloff + clear_dirty_start, coloff + clear_dirty_start,
              coloff + clear_end, clear_attr, flags & SLF_WRAP);
    }
    clear_end = end_dirty;
  } else {
    if (start_dirty == -1) {  // clear only
      start_dirty = clear_dirty_start;
      end_dirty = clear_dirty_start;
    } else if (clear_end < end_dirty) {  // put only
      clear_end = end_dirty;
    } else {
      end_dirty = endcol;
    }
  }

  if (clear_end > start_dirty) {
    if (!grid->throttled) {
      ui_line(grid, row, invalid_row, coloff + start_dirty, coloff + end_dirty, coloff + clear_end,
              clear_attr, flags & SLF_WRAP);
    } else if (grid->dirty_col) {
      // TODO(bfredl): really get rid of the extra pseudo terminal in message.c
      // by using a linebuf_char copy for "throttled message line"
      if (clear_end > grid->dirty_col[row]) {
        grid->dirty_col[row] = clear_end;
      }
    }
  }
}
#endif // VIM_IMPL

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
