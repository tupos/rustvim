#pragma once

#include <stdbool.h>
#include <stddef.h>  // IWYU pragma: keep

#include "nvim/buffer_defs.h"  // IWYU pragma: keep
#include "nvim/grid_defs.h"  // IWYU pragma: keep
#include "nvim/macros_defs.h"
#include "nvim/pos_defs.h"
#include "nvim/types_defs.h"

#define DEFAULT_GRID_HANDLE 1  // handle for the default_grid

/// While resizing the screen this flag is set.
EXTERN bool resizing_screen INIT( = 0);

/// Flags for final grid line submission.
enum {
  SLF_RIGHTLEFT = 1,
  SLF_WRAP      = 2,
  SLF_INC_VCOL  = 4,
};

/// Stable subset of WinConfig passed to the Rust border renderer.
typedef struct {
  const char *chars;
  const int *attrs;
  int title;
  int title_pos;
  VirtText title_chunks;
  int title_width;
  int title_attr;
  int footer;
  int footer_pos;
  VirtText footer_chunks;
  int footer_width;
  int footer_attr;
} GridBorderConfig;

/// Values needed to prepare and submit a completed window line in Rust.
typedef struct {
  int row;
  int end_col;
  int width;
  int bg_attr;
  int smoothscroll_marker_attr;
  colnr_T last_vcol;
  int flags;
  bool clear_end;
  bool right_left;
  bool put_smoothscroll_marker;
  bool preserve_line_number;
} GridLineSubmissionConfig;

typedef struct {
  handle_T grid_handle;
  bool resized;
  bool row_offset_changed;
} WindowGridUpdate;

typedef struct {
  int end_col;
  colnr_T end_vcol;
  size_t color_cols_consumed;
  bool color_cols_exhausted;
} LinebufRenderTextResult;

typedef enum {
  kMessageGridUnchanged = 0,
  kMessageGridResized = 1,
  kMessageGridReleaseRequired = 2,
} MessageGridTransition;

typedef struct {
  int position;
  int scrolled_lines;
  int screen_rows;
  int screen_columns;
  sattr_T clear_attribute;
} MessageGridScrollFlushRequest;

typedef struct {
  schar_T separator_character;
  int z_index;
} MessageGridPositionConfig;

typedef struct {
  handle_T grid_handle;
  int screen_rows;
  int screen_columns;
  bool should_allocate;
  int scrolled_lines;
  bool pager_active;
  int command_height;
  int current_position;
  MessageGridPositionConfig position;
  bool blending;
  handle_T current_window_grid_handle;
  sattr_T clear_attribute;
} MessageSurfaceRequest;

typedef struct {
  handle_T grid_handle;
  int position;
  bool redraw_cmdline;
} MessageSurfaceUpdate;

typedef void (*MessageGridPositionEmitter)(
  handle_T grid_handle, int row, bool scrolled,
  schar_T separator_character, int z_index, int compositor_index);

extern bool grid_is_allocated(handle_T grid_handle);
extern bool grid_requires_compositor_invalidation(handle_T grid_handle,
                                                  bool clear_requested);
extern void grid_revalidate_for_redraw(handle_T grid_handle);
extern void grid_require_full_redraw(handle_T grid_handle);
extern bool grid_view_has_target(handle_T window_grid_handle);
extern bool grid_view_targets_default(handle_T window_grid_handle);
extern bool grid_view_requires_redraw(handle_T window_grid_handle);
extern bool grid_view_width_matches_target(handle_T window_grid_handle, int width);
extern void grid_view_cursor_goto(handle_T window_grid_handle, int row, int col);
extern void grid_message_view_cursor_goto(int row, int col);
extern void grid_initialize_default(void);
extern bool grid_configure_default(int rows, int columns);
extern WindowGridUpdate grid_configure_for_window(
  handle_T grid_handle, int rows, int columns, bool use_dedicated_grid,
  int row_offset, int col_offset);
extern bool grid_configure_for_popup_menu(handle_T grid_handle, int rows, int columns,
                                          bool contents_invalid);
extern void grid_release_popup_menu(handle_T grid_handle);
extern MessageGridTransition grid_prepare_for_messages(handle_T grid_handle, int rows, int columns,
                                                       bool should_allocate,
                                                       int scrolled_lines);
extern void grid_release_message(handle_T grid_handle);
extern void grid_activate_message_view(handle_T grid_handle);
extern void grid_reset_message_view(void);
extern MessageSurfaceUpdate grid_update_message_surface(
  MessageSurfaceRequest request,
  MessageGridPositionEmitter emit_position);
extern void grid_set_message_position(handle_T grid_handle, int row, bool scrolled,
                                      MessageGridPositionConfig config,
                                      MessageGridPositionEmitter emit_position);
extern int grid_scroll_message_up(handle_T grid_handle, int current_position,
                                  bool enable_throttling, bool zero_cmdheight,
                                  bool discount_first_scroll,
                                  MessageGridPositionConfig config,
                                  MessageGridPositionEmitter set_position);
extern void grid_prepare_message_redraw(handle_T grid_handle, int rows_to_clear,
                                        int valid_rows);
extern int grid_reset_message_scroll(handle_T grid_handle, int position,
                                     int rows_to_clear,
                                     MessageGridPositionConfig config,
                                     MessageGridPositionEmitter set_position);
extern void grid_flush_message_scroll(handle_T grid_handle,
                                      MessageGridScrollFlushRequest request,
                                      MessageGridPositionConfig config,
                                      MessageGridPositionEmitter emit_position);
extern void grid_refresh_message_ui(handle_T grid_handle, int position, bool scrolled,
                                    MessageGridPositionConfig config,
                                    MessageGridPositionEmitter emit_position);
extern void grid_free(handle_T grid_handle);
extern void grid_free_all_mem(void);
extern bool grid_clear_default(bool valid);
extern void grid_clear_default_rows(int start_row, int end_row, bool valid);
extern void grid_scroll_window_lines(handle_T window_grid_handle, int row, int line_count,
                                     int view_height, int view_width);
extern handle_T grid_view_translate(handle_T window_grid_handle, int *row_off, int *col_off);
extern handle_T grid_message_view_translate(int *row_off, int *col_off);
extern bool grid_view_translate_to_local(handle_T window_grid_handle, int *row,
                                         int *col);
extern void grid_invalidate(handle_T grid_handle);
extern void grid_view_invalidate_row(handle_T window_grid_handle, int row);
extern schar_T schar_from_str(const char *str);
extern schar_T schar_from_buf(const char *buf, size_t len);
extern bool schar_cache_clear_if_full(void);
extern void schar_cache_clear(void);
extern bool schar_high(schar_T sc);
extern size_t schar_get(char *buf_out, schar_T sc);
extern int schar_get_first_codepoint(schar_T sc);
extern size_t schar_get_adv(char **buf_out, schar_T sc);
extern size_t schar_len(schar_T sc);
extern char schar_get_ascii(schar_T sc);
extern int schar_cells(schar_T sc);
extern bool grid_view_get_cell(
  handle_T window_grid_handle, handle_T expected_grid,
  int row, int col, GridCell *cell);
extern GridCellInspectionResult grid_inspect_cell(
  handle_T popup_grid_handle, handle_T window_grid_handle,
  int64_t requested_grid, int64_t row, int64_t col,
  GridCell *cell);
extern schar_T schar_from_char(int c);

extern void grid_window_line_start(handle_T window_grid_handle, int row);
extern void grid_line_start_grid(handle_T grid_handle, int row, int col);
extern void grid_message_view_line_start(int row);
extern schar_T grid_line_getchar(int col, int *attr);
extern void grid_line_put_schar(int col, schar_T schar, int attr);
extern void grid_line_put_truncation(int start_col, int width, schar_T marker, int attr,
                                     bool right_to_left);
extern int grid_line_puts(int col, const char *text, int textlen, int attr);
extern int grid_line_fill(int start_col, int end_col, schar_T sc, int attr);
extern void grid_line_clear_end(int start_col, int end_col, int bg_attr, int clear_attr);
extern void grid_line_cursor_goto(int col);
extern void grid_line_mirror(int width);
extern void linebuf_reset(int width);
extern int linebuf_fill_chars_and_attrs(int start_col, int width, schar_T sc,
                                        int attr);
extern int linebuf_put_cell(int col, schar_T sc, int attr, colnr_T vcol);
extern int linebuf_put_rendered_glyph(int col, schar_T sc, int attr,
                                      colnr_T vcol);
extern LinebufRenderTextResult linebuf_render_text(
  int col, const char *text, size_t text_len, int max_col, int attr,
  bool inc_vcol, colnr_T first_vcol, const colnr_T *vcols,
  size_t vcols_len, const colnr_T *color_cols, size_t color_cols_len,
  int color_col_attr, OptInt tabstop, const colnr_T *variable_tabstops);
extern int linebuf_render_fold_column(
  int start_col, int attr, int width, int level, int lowest_level,
  bool starts_here, bool is_closed, bool virtual_line, schar_T open,
  schar_T closed, schar_T separator, schar_T inner,
  colnr_T *out_vcols, schar_T *out_chars);
extern int linebuf_render_virtual_text(int col, VirtText virtual_text,
                                       int hl_mode, int max_col, colnr_T vcol,
                                       int skip_cells, bool eol_hl,
                                       OptInt tabstop, const colnr_T *variable_tabstops);
extern int linebuf_update_cell(int col, schar_T sc, bool replace_char, int attr,
                               colnr_T vcol);
extern void linebuf_put_char_pair(int col, schar_T first, schar_T second);
extern void linebuf_fill_vcols(int start_col, int width, colnr_T vcol);
extern void linebuf_fill_vcols_from(int start_col, int width, colnr_T first_vcol);
extern void grid_line_flush(void);
extern void grid_flush_message_line(bool right_left);
extern void grid_clear_region(handle_T grid_handle, int start_row, int end_row, int start_col,
                              int end_col, int attr);
extern void grid_message_view_clear(int start_row, int end_row, int start_col, int end_col,
                                    int attr);
extern void grid_put_window_line(handle_T window_grid_handle,
                                 const GridLineSubmissionConfig *config);
extern void grid_draw_border(handle_T grid_handle, const GridBorderConfig *config, const int *adj,
                             int winbl, int win_hl_attr_normal);

extern handle_T grid_assign_handle(void);

extern void grid_ins_lines(handle_T grid_handle, int row, int line_count, int end, int col,
                           int width);

extern void alloc_initial_line_buffer(void);

/// Put a ASCII character in a screen cell.
///
/// If `x` is a compile time constant, schar_from_ascii(x) will also be.
/// But the specific value varies per platform.
#ifdef ORDER_BIG_ENDIAN
# define schar_from_ascii(x) ((schar_T)((x) << 24))
#else
# define schar_from_ascii(x) ((schar_T)(x))
#endif
