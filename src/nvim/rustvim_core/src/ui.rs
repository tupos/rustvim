pub(crate) mod output;

use std::{ffi::CString, sync::Mutex};

use crate::{
    defines::{Integer, OptInt},
    generated::enums::RdbFlags,
    grid::{GridId, LocalGridSpace, ScreenGrid, VimHandleT, VimSAttrT, VimSCharT},
};

use output::{
    CellBufferPool, OwnedRawLineCommand, RawLineCells, RawLineCommand, UiCommand, UiOutput,
};

static GRID_OUTPUT_BUFFER_POOL: Mutex<CellBufferPool> = Mutex::new(CellBufferPool::new());

// TODO: move to highlight
pub type RgbValue = i32;

unsafe extern "C" {
    static mut p_wd: OptInt;
    static mut rdb_flags: libc::c_uint;
    static mut pending_cursor_update: bool;

    static mut normal_fg: RgbValue;
    static mut normal_bg: RgbValue;
    static mut normal_sp: RgbValue;

    static mut cterm_normal_fg_color: libc::c_int;
    static mut cterm_normal_bg_color: libc::c_int;

    static mut starting: libc::c_int;

    fn ui_call_default_colors_set(
        rgb_fg: Integer,
        rgb_bg: Integer,
        rgb_sp: Integer,
        cterm_fg: Integer,
        cterm_bg: Integer,
    );
    fn ui_call_grid_resize(grid: Integer, width: Integer, height: Integer);
    fn ui_call_raw_line(
        grid: Integer,
        row: Integer,
        startcol: Integer,
        endcol: Integer,
        clearcol: Integer,
        clearattr: Integer,
        flags: libc::c_int,
        chunk: *const VimSCharT,
        attrs: *const VimSAttrT,
    );
    fn ui_call_grid_cursor_goto(grid: Integer, row: Integer, col: Integer);
    fn ui_call_flush();
    pub fn ui_grid_cursor_goto(grid_handle: VimHandleT, new_row: libc::c_int, new_col: libc::c_int);
    fn log_debug_message(message: *const libc::c_char);
    fn os_sleep(ms: u64);
    pub fn ui_call_grid_scroll(
        grid: Integer,
        top: Integer,
        bot: Integer,
        left: Integer,
        right: Integer,
        rows: Integer,
        cols: Integer,
    );
}

/// Identifies a normal grid and the width needed by redraw diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GridOutputTarget {
    grid: GridId,
    width: i32,
}

impl GridOutputTarget {
    /// Creates an output target for a grid with a nonnegative width.
    pub(crate) fn new(grid: GridId, width: i32) -> Self {
        assert!(width >= 0, "grid output width must be nonnegative");
        Self { grid, width }
    }

    /// Emits a command after the operation's grid borrow has ended.
    pub(crate) fn emit(self, command: UiCommand<LocalGridSpace>) {
        NvimGridOutput { target: self }.emit(command);
    }

    /// Emits owned line commands after their source-grid borrows have ended.
    pub(crate) fn emit_lines(self, commands: impl IntoIterator<Item = OwnedRawLineCommand>) {
        let mut output = NvimGridOutput { target: self };
        for command in commands {
            output.emit_raw_line(command);
        }
    }
}

/// Emits normal-grid commands through Neovim's C UI interface.
struct NvimGridOutput {
    target: GridOutputTarget,
}

impl UiOutput<LocalGridSpace> for NvimGridOutput {
    fn emit(&mut self, command: UiCommand<LocalGridSpace>) {
        let grid = Integer::from(self.target.grid.as_native_handle());
        match command {
            UiCommand::GridResize(size) => {
                // SAFETY: The dimensions and grid identity were validated in
                // safe Rust and are converted at the final C boundary.
                unsafe {
                    ui_call_grid_resize(
                        grid,
                        Integer::from(size.width),
                        Integer::from(size.height),
                    );
                }
            }
            UiCommand::GridCursorGoto(position) => {
                // SAFETY: The grid identity and coordinates are converted at
                // the final C boundary.
                unsafe {
                    ui_grid_cursor_goto(
                        self.target.grid.as_native_handle(),
                        position.row,
                        position.column,
                    );
                }
            }
            UiCommand::GridScroll { region, delta } => {
                // SAFETY: The ordered region and its delta remain valid for
                // this synchronous callback.
                unsafe {
                    ui_call_grid_scroll(
                        grid,
                        Integer::from(region.rows.start),
                        Integer::from(region.rows.end),
                        Integer::from(region.columns.start),
                        Integer::from(region.columns.end),
                        Integer::from(delta.rows),
                        Integer::from(delta.columns),
                    );
                }
            }
            UiCommand::Log(message) => {
                let message = CString::new(message).expect("UI diagnostic contains no NUL");
                // SAFETY: `CString` remains alive for this synchronous call.
                unsafe {
                    log_debug_message(message.as_ptr());
                }
            }
            UiCommand::Flush => {
                // SAFETY: UI output is flushed on Neovim's main thread.
                unsafe {
                    ui_call_flush();
                }
            }
            UiCommand::Delay(milliseconds) => {
                // SAFETY: Sleeping is performed on Neovim's main thread.
                unsafe {
                    os_sleep(milliseconds);
                }
            }
        }
    }

    fn emit_raw_line<Cells: RawLineCells>(&mut self, line: RawLineCommand<Cells>) {
        let grid = Integer::from(self.target.grid.as_native_handle());
        // SAFETY: `RawLineCommand` guarantees equally sized readable payloads
        // matching the changed range. They remain alive for this synchronous
        // callback.
        unsafe {
            ui_may_set_default_colors();
            ui_call_raw_line(
                grid,
                Integer::from(line.row),
                Integer::from(line.columns.start),
                Integer::from(line.columns.end),
                Integer::from(line.clear_column),
                Integer::from(line.clear_attribute),
                line.flags.raw(),
                line.characters().as_ptr(),
                line.attributes().as_ptr(),
            );

            if p_wd != 0 && (rdb_flags & RdbFlags::Line as u32) != 0 {
                ui_call_grid_cursor_goto(
                    grid,
                    Integer::from(line.row),
                    Integer::from(std::cmp::min(line.clear_column, self.target.width - 1)),
                );
                ui_call_flush();
                os_sleep(p_wd.unsigned_abs());
                pending_cursor_update = true;
            }
        }
    }
}

/// Flags controlling one raw-line UI update.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct LineFlags(i32);

impl LineFlags {
    pub(crate) const WRAP: Self = Self(1);
    pub(crate) const INVALID: Self = Self(2);

    /// Creates flags while preserving bits received from the C API.
    pub(crate) const fn from_raw(flags: i32) -> Self {
        Self(flags)
    }

    /// Returns the representation expected by the C UI API.
    pub(crate) const fn raw(self) -> i32 {
        self.0
    }

    /// Adds one flag to this set.
    pub(crate) fn insert(&mut self, flag: Self) {
        self.0 |= flag.0;
    }

    /// Returns this set without the supplied flags.
    pub(crate) const fn without(self, flags: Self) -> Self {
        Self(self.0 & !flags.0)
    }

    /// Reports whether every supplied flag is present.
    pub(crate) const fn contains(self, flags: Self) -> bool {
        self.0 & flags.0 == flags.0
    }
}

static mut PENDING_DEFAULT_COLORS: bool = false;

#[unsafe(no_mangle)]
pub extern "C" fn ui_default_colors_set() {
    unsafe {
        // Throttle setting of default colors at startup, so it only happens once
        // if the user sets the colorscheme in startup.
        PENDING_DEFAULT_COLORS = true;
        if starting == 0 {
            ui_may_set_default_colors();
        }
    }
}

fn ui_may_set_default_colors() {
    unsafe {
        if PENDING_DEFAULT_COLORS {
            PENDING_DEFAULT_COLORS = false;
            ui_call_default_colors_set(
                normal_fg as Integer,
                normal_bg as Integer,
                normal_sp as Integer,
                cterm_normal_fg_color as Integer,
                cterm_normal_bg_color as Integer,
            );
        }
    }
}

/// Builds one owned raw-line command from a stored grid row.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ui_line_r(
    grid: &ScreenGrid,
    row: i32,
    invalid_row: bool,
    start_col: i32,
    end_col: i32,
    clear_col: i32,
    clear_attr: i32,
    wrap: bool,
) -> OwnedRawLineCommand {
    assert!(0 <= row && row < grid.size().height);
    let mut flags = if wrap {
        LineFlags::WRAP
    } else {
        LineFlags::default()
    };
    if start_col == 0 && invalid_row {
        flags.insert(LineFlags::INVALID);
    }

    let offset = grid.line_offset[row as usize] + start_col as usize;
    let payload_len = usize::try_from(end_col - start_col).expect("ordered line columns");
    let payload_end = offset + payload_len;
    OwnedRawLineCommand::try_owned(
        row,
        start_col..end_col,
        clear_col,
        clear_attr,
        flags,
        &grid.chars[offset..payload_end],
        &grid.attrs[offset..payload_end],
        &GRID_OUTPUT_BUFFER_POOL,
    )
    .expect("valid grid line output")
}
