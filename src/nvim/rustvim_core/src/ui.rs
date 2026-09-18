use crate::{
    defines::{Integer, OptInt},
    generated::enums::RdbFlags,
    grid::{CScreenGrid, ScreenGrid, VimHandleT, VimSAttrT, VimSCharT},
};

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
    fn ui_call_raw_line(
        grid: Integer,
        row: Integer,
        startcol: Integer,
        endcol: Integer,
        clearcol: Integer,
        clearattr: Integer,
        flags: VimLineFlags,
        chunk: *const VimSCharT,
        attrs: *const VimSAttrT,
    );
    fn ui_call_grid_cursor_goto(grid: Integer, row: Integer, col: Integer);
    fn ui_call_flush();
    pub fn ui_grid_cursor_goto(grid_handle: VimHandleT, new_row: libc::c_int, new_col: libc::c_int);
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

enum LineFlags {
    Wrap = 1,
    Invalid = 2,
}

type VimLineFlags = libc::c_int;

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

/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid` whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_line(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    invalid_row: bool,
    start_col: libc::c_int,
    end_col: libc::c_int,
    clear_col: libc::c_int,
    clear_attr: libc::c_int,
    wrap: bool,
) {
    let g: ScreenGrid = unsafe { (&*grid).into() };
    ui_line_r(
        &g,
        row,
        invalid_row,
        start_col,
        end_col,
        clear_col,
        clear_attr,
        wrap,
    );
    unsafe {
        *grid = g.to_cgrid();
    }
}

// Keep the arguments aligned with ui_line() in ui.c.
#[allow(clippy::too_many_arguments)]
pub fn ui_line_r(
    grid: &ScreenGrid,
    row: i32,
    invalid_row: bool,
    start_col: i32,
    end_col: i32,
    clear_col: i32,
    clear_attr: i32,
    wrap: bool,
) {
    assert!(0 <= row && row < grid.rows);
    let mut flags: VimLineFlags = if wrap {
        LineFlags::Wrap as VimLineFlags
    } else {
        0
    };
    if start_col == 0 && invalid_row {
        flags |= LineFlags::Invalid as VimLineFlags;
    }

    unsafe {
        // set default colors now so that that text won't have to be repainted later
        ui_may_set_default_colors();

        let offset = grid.line_offset[row as usize] + start_col as usize;
        ui_call_raw_line(
            grid.handle as i64,
            row as i64,
            start_col as i64,
            end_col as i64,
            clear_col as i64,
            clear_attr as i64,
            flags as i32,
            grid.chars.as_ptr().add(offset),
            grid.attrs.as_ptr().add(offset),
        );

        if p_wd != 0 && (rdb_flags & RdbFlags::Line as u32) != 0 {
            // If 'writedelay' is active, set the cursor to indicate what was drawn.
            ui_call_grid_cursor_goto(
                grid.handle as Integer,
                row as Integer,
                std::cmp::min(clear_col, grid.cols - 1) as Integer,
            );
            ui_call_flush();
            let wd = p_wd.unsigned_abs();
            os_sleep(wd);
            // restore the cursor later
            pending_cursor_update = true;
        }
    }
}
