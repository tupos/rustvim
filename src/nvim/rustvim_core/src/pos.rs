/// Line number type
pub type LineNum = i32;
pub type VimLineNumT = LineNum;

/// Column number type
pub type ColNum = i32;

/// Maximal (invalid) line number
pub static MAX_LINE_NUM: i32 = 0x7fffffff;

/// MAXCOL used to be INT_MAX, but with 64 bit ints that results in running
/// out of memory when trying to allocate a very long line.
/// Maximal column number
pub static MAX_COL_NUM: i32 = 0x7fffffff;

/// Minimum line number
pub static MIN_LINE_NUM: i32 = 1;

/// Minimum column number
pub static MIN_COL_NUM: i32 = 1;

/// position in file or buffer
pub struct Pos {
    /// line number
    line_num: LineNum,
    /// column number
    col: ColNum,
    /// TODO ??
    col_add: ColNum,
}

/// position in file or buffer, but without coladd
pub struct LPos {
    /// line number
    line_num: LineNum,
    /// column number
    col: ColNum,
}
