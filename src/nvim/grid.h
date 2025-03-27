#pragma once

#include <stdbool.h>
#include <stddef.h>  // IWYU pragma: keep

#include "nvim/buffer_defs.h"  // IWYU pragma: keep
#include "nvim/grid_defs.h"  // IWYU pragma: keep
#include "nvim/macros_defs.h"
#include "nvim/pos_defs.h"
#include "nvim/types_defs.h"

/// By default, all windows are drawn on a single rectangular grid, represented by
/// this ScreenGrid instance. In multigrid mode each window will have its own
/// grid, then this is only used for global screen elements that hasn't been
/// externalized.
///
/// Note: before the screen is initialized and when out of memory these can be
/// NULL.
EXTERN ScreenGrid default_grid INIT( = SCREEN_GRID_INIT);
EXTERN GridView default_gridview INIT( = { .target = &default_grid });

#define DEFAULT_GRID_HANDLE 1  // handle for the default_grid

/// While resizing the screen this flag is set.
EXTERN bool resizing_screen INIT( = 0);

EXTERN schar_T *linebuf_char INIT( = NULL);
EXTERN sattr_T *linebuf_attr INIT( = NULL);
EXTERN colnr_T *linebuf_vcol INIT( = NULL);
EXTERN char *linebuf_scratch INIT( = NULL);

/// flags for grid_put_linebuf()
enum {
  SLF_RIGHTLEFT = 1,
  SLF_WRAP      = 2,
  SLF_INC_VCOL  = 4,
};

extern void grid_alloc(ScreenGrid *grid, int rows, int columns, bool copy, bool valid);
extern void grid_free(ScreenGrid *grid);
extern void grid_clear_line(ScreenGrid *grid, size_t off, int width, bool valid);
extern ScreenGrid *grid_adjust(GridView *grid, int *row_off, int *col_off);
extern void grid_invalidate(ScreenGrid *grid);
extern schar_T schar_from_str(const char *str);
extern schar_T schar_from_buf(const char *buf, size_t len);
extern bool schar_cache_clear_if_full(void);
extern void schar_cache_clear(void);
extern bool schar_high(schar_T sc);
extern size_t schar_get(char *buf_out, schar_T sc);
extern size_t schar_get_adv(char **buf_out, schar_T sc);
extern size_t schar_len(schar_T sc);
extern bool schar_in_arabic_block(schar_T sc);
extern char schar_get_ascii(schar_T sc);
extern schar_T grid_getchar(ScreenGrid *grid, int row, int col, int *attrp);
extern schar_T schar_from_char(int c);

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

#include "grid.h.generated.h"
