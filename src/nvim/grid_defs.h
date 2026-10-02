#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#include "nvim/pos_defs.h"
#include "nvim/types_defs.h"

enum {
  kZIndexDefaultGrid = 0,
  kZIndexFloatDefault = 50,
  kZIndexPopupMenu = 100,
  kZIndexMessages = 200,
  kZIndexCmdlinePopupMenu = 250,
};

/// Complete contents of one rendered screen cell.
typedef struct {
  schar_T character;
  sattr_T attribute;
  colnr_T virtual_column;
} GridCell;

typedef enum {
  kGridCellInspectionInvalidGrid = 0,
  kGridCellInspectionMissingCell = 1,
  kGridCellInspectionFound = 2,
} GridCellInspectionResult;

typedef struct {
  int args[3];
  int icell;
  int ncells;
  int coloff;
  int cur_attr;
  int clear_width;
  bool wrap;
} GridLineEvent;
