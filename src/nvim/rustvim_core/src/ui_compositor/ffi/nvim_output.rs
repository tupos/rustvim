use std::ffi::CString;

use crate::{
    defines::Integer,
    grid::{ComposedGridSpace, VimSAttrT, VimSCharT},
    ui::output::{RawLineCells, RawLineCommand, UiCommand, UiOutput},
};

unsafe extern "C" {
    fn log_debug_message(message: *const libc::c_char);
    fn os_sleep(ms: u64);
    fn ui_call_flush();
    fn ui_composed_call_grid_cursor_goto(grid: Integer, row: Integer, column: Integer);
    fn ui_composed_call_grid_resize(grid: Integer, width: Integer, height: Integer);
    fn ui_composed_call_grid_scroll(
        grid: Integer,
        top: Integer,
        bottom: Integer,
        left: Integer,
        right: Integer,
        rows: Integer,
        columns: Integer,
    );
    fn ui_composed_call_raw_line(
        grid: Integer,
        row: Integer,
        start_col: Integer,
        end_col: Integer,
        clear_col: Integer,
        clear_attr: Integer,
        flags: libc::c_int,
        characters: *const VimSCharT,
        attributes: *const VimSAttrT,
    );
}

/// Emits safe compositor commands through Neovim's C UI interface.
pub(crate) struct NvimComposedOutput;

impl UiOutput<ComposedGridSpace> for NvimComposedOutput {
    fn emit(&mut self, command: UiCommand<ComposedGridSpace>) {
        match command {
            UiCommand::GridResize(size) => {
                // SAFETY: The validated dimensions are converted at the C
                // boundary, and the callback is synchronous.
                unsafe {
                    ui_composed_call_grid_resize(
                        1,
                        Integer::from(size.width),
                        Integer::from(size.height),
                    );
                }
            }
            UiCommand::GridCursorGoto(position) => {
                // SAFETY: The validated coordinates are converted at the C
                // boundary, and the callback is synchronous.
                unsafe {
                    ui_composed_call_grid_cursor_goto(
                        1,
                        Integer::from(position.row),
                        Integer::from(position.column),
                    );
                }
            }
            UiCommand::GridScroll { region, delta } => {
                // SAFETY: The region and deltas contain validated native
                // values, and the callback is synchronous.
                unsafe {
                    ui_composed_call_grid_scroll(
                        1,
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
                let message = CString::new(message).expect("compositor diagnostic contains no NUL");
                // SAFETY: `CString` supplies a non-null, NUL-terminated
                // string that remains alive for the synchronous logging call.
                unsafe {
                    log_debug_message(message.as_ptr());
                }
            }
            UiCommand::Flush => {
                // SAFETY: UI output is flushed synchronously on Neovim's main
                // thread.
                unsafe {
                    ui_call_flush();
                }
            }
            UiCommand::Delay(milliseconds) => {
                // SAFETY: Sleeping accepts the validated scalar duration and
                // is performed on Neovim's main thread.
                unsafe {
                    os_sleep(milliseconds);
                }
            }
        }
    }

    fn emit_raw_line<Cells: RawLineCells>(&mut self, line: RawLineCommand<Cells>) {
        // SAFETY: `RawLineCommand` guarantees equally sized readable payload
        // slices matching the changed column range. They remain alive for
        // this synchronous callback.
        unsafe {
            ui_composed_call_raw_line(
                1,
                Integer::from(line.row),
                Integer::from(line.columns.start),
                Integer::from(line.columns.end),
                Integer::from(line.clear_column),
                Integer::from(line.clear_attribute),
                line.flags.raw(),
                line.characters().as_ptr(),
                line.attributes().as_ptr(),
            );
        }
    }
}
