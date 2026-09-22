#pragma once

#include <stdbool.h>

#include "nvim/api/private/defs.h"  // IWYU pragma: keep
#include "nvim/grid_defs.h"  // IWYU pragma: keep
#include "nvim/types_defs.h"  // IWYU pragma: keep
#include "nvim/ui_defs.h"  // IWYU pragma: keep

typedef struct {
  bool copied;
  bool followed_by_continuation;
} CompositorGridSegmentResult;

extern CompositorGridSegmentResult ui_comp_copy_grid_segment(
  ScreenGrid *grid, int row, int start_col, int end_col,
  schar_T *characters, sattr_T *attributes);

extern bool ui_comp_blend_grid_segment(
  ScreenGrid *background_grid, int row, int start_col, int blend_end_col,
  int end_col, schar_T *characters, sattr_T *attributes);

#include "ui_compositor.h.generated.h"
