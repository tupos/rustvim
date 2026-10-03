pub(crate) mod output;

use std::{
    ffi::CString,
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    context::registered_context,
    defines::{Integer, OptInt},
    generated::enums::RdbFlags,
    grid::{
        DEFAULT_GRID_ID, GridId, GridPosition, GridRow, LocalGridSpace, ScreenGrid, VimHandleT,
        VimSAttrT, VimSCharT,
    },
    ui_compositor::{
        CompositorEventOutputs, CompositorScrollRequest, NvimComposedOutput,
        compositor_runtime_snapshot, current_message_grid_id,
    },
};

use output::{
    CellBufferPool, OwnedRawLineCommand, RawLineCells, RawLineCommand, UiCommand, UiOutput,
};

// TODO: move to highlight
pub type RgbValue = i32;

/// One grid-local cursor selected for eventual UI publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GridCursor {
    grid_id: GridId,
    position: GridPosition,
}

impl GridCursor {
    /// Combines one grid identity with its local cursor position.
    const fn new(grid_id: GridId, position: GridPosition) -> Self {
        Self { grid_id, position }
    }
}

/// Current cursor and whether it must be republished to UI clients.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingGridCursorState {
    current: GridCursor,
    publication_pending: bool,
}

impl PendingGridCursorState {
    /// Constructs the initial default-grid cursor without pending output.
    const fn new() -> Self {
        Self {
            current: GridCursor::new(DEFAULT_GRID_ID, GridPosition::new(0, 0)),
            publication_pending: false,
        }
    }

    /// Selects a changed cursor and schedules it for publication.
    fn schedule(&mut self, cursor: GridCursor) {
        if cursor != self.current {
            self.current = cursor;
            self.publication_pending = true;
        }
    }

    /// Schedules republication when the current cursor's grid moved.
    fn mark_grid_moved(&mut self, grid_id: GridId) {
        if self.current.grid_id == grid_id {
            self.publication_pending = true;
        }
    }

    /// Resets local coordinates while retaining the current grid identity.
    fn reset_position_for_refresh(&mut self) {
        self.current.position = GridPosition::new(0, 0);
        self.publication_pending = true;
    }

    /// Schedules the current cursor for unconditional republication.
    fn force_republication(&mut self) {
        self.publication_pending = true;
    }

    /// Returns the current cursor without changing pending state.
    const fn current(self) -> GridCursor {
        self.current
    }

    /// Takes the current cursor when publication is pending.
    fn take_pending(&mut self) -> Option<GridCursor> {
        std::mem::replace(&mut self.publication_pending, false).then_some(self.current)
    }
}

/// State owned by the UI output subsystem for the lifetime of the editor.
pub(crate) struct UiContext {
    grid_output_buffer_pool: Mutex<CellBufferPool>,
    pending_grid_cursor: Mutex<PendingGridCursorState>,
    pending_default_colors: AtomicBool,
}

impl UiContext {
    /// Constructs empty UI output state.
    pub(crate) const fn new() -> Self {
        Self {
            grid_output_buffer_pool: Mutex::new(CellBufferPool::new()),
            pending_grid_cursor: Mutex::new(PendingGridCursorState::new()),
            pending_default_colors: AtomicBool::new(false),
        }
    }

    /// Acquires the pending grid-cursor state.
    fn lock_pending_grid_cursor(&self) -> MutexGuard<'_, PendingGridCursorState> {
        self.pending_grid_cursor
            .try_lock()
            .expect("pending grid cursor is already borrowed")
    }

    /// Selects a changed cursor and schedules it for publication.
    fn schedule_grid_cursor(&self, cursor: GridCursor) {
        self.lock_pending_grid_cursor().schedule(cursor);
    }

    /// Schedules republication when the current cursor's grid moved.
    fn mark_grid_cursor_moved(&self, grid_id: GridId) {
        self.lock_pending_grid_cursor().mark_grid_moved(grid_id);
    }

    /// Resets local coordinates while retaining the current grid identity.
    fn reset_grid_cursor_position(&self) {
        self.lock_pending_grid_cursor().reset_position_for_refresh();
    }

    /// Schedules the current cursor for unconditional republication.
    fn force_grid_cursor_republication(&self) {
        self.lock_pending_grid_cursor().force_republication();
    }

    /// Returns the current cursor without changing pending state.
    fn current_grid_cursor(&self) -> GridCursor {
        self.lock_pending_grid_cursor().current()
    }

    /// Takes the current cursor when publication is pending.
    fn take_pending_grid_cursor(&self) -> Option<GridCursor> {
        self.lock_pending_grid_cursor().take_pending()
    }

    /// Returns the reusable grid-output buffer pool.
    fn grid_output_buffer_pool(&self) -> &Mutex<CellBufferPool> {
        &self.grid_output_buffer_pool
    }

    /// Records that current default colors must be published.
    fn schedule_default_colors(&self) {
        self.pending_default_colors.store(true, Ordering::Relaxed);
    }

    /// Takes the pending default-color publication.
    fn take_pending_default_colors(&self) -> bool {
        self.pending_default_colors.swap(false, Ordering::Relaxed)
    }
}

unsafe extern "C" {
    static mut p_wd: OptInt;
    static mut rdb_flags: libc::c_uint;

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
    fn ui_uncomposed_call_grid_resize(grid: Integer, width: Integer, height: Integer);
    fn ui_uncomposed_call_grid_cursor_goto(grid: Integer, row: Integer, col: Integer);
    fn ui_uncomposed_call_raw_line(
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
    fn ui_call_flush();
    fn log_debug_message(message: *const libc::c_char);
    fn os_sleep(ms: u64);
    fn ui_uncomposed_call_grid_scroll(
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

/// Delivers original grid events only to non-composed Neovim UIs.
struct NvimUncomposedOutput {
    grid_id: GridId,
}

impl UiOutput<LocalGridSpace> for NvimUncomposedOutput {
    fn emit(&mut self, command: UiCommand<LocalGridSpace>) {
        let grid = Integer::from(self.grid_id.as_native_handle());
        match command {
            UiCommand::GridResize(size) => {
                // SAFETY: The identity and dimensions were validated in safe
                // Rust and the generated callback is synchronous.
                unsafe {
                    ui_uncomposed_call_grid_resize(
                        grid,
                        Integer::from(size.width),
                        Integer::from(size.height),
                    );
                }
            }
            UiCommand::GridCursorGoto(position) => {
                // SAFETY: The validated cursor is delivered synchronously at
                // the final C boundary.
                unsafe {
                    ui_uncomposed_call_grid_cursor_goto(
                        grid,
                        Integer::from(position.row),
                        Integer::from(position.column),
                    );
                }
            }
            UiCommand::GridScroll { region, delta } => {
                // SAFETY: The region and delta were validated in safe Rust and
                // the generated callback is synchronous.
                unsafe {
                    ui_uncomposed_call_grid_scroll(
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
        let grid = Integer::from(self.grid_id.as_native_handle());
        // SAFETY: `RawLineCommand` owns or borrows equally sized readable
        // payloads for the synchronous generated callback.
        unsafe {
            ui_uncomposed_call_raw_line(
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
        }
    }
}

/// Routes Rust-originated grid events through the compositor before delivery.
struct NvimGridOutput {
    target: GridOutputTarget,
}

impl NvimGridOutput {
    /// Publishes one pending cursor through composed and uncomposed outputs.
    fn publish_cursor(cursor: GridCursor) {
        let context = registered_context();
        let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
        let mut composed = NvimComposedOutput;
        let mut uncomposed = NvimUncomposedOutput {
            grid_id: cursor.grid_id,
        };
        let mut outputs = CompositorEventOutputs::new(&mut composed, &mut uncomposed);

        context
            .compositor()
            .route_grid_cursor(
                context.grid(),
                current_message_grid_id(),
                cursor.grid_id,
                cursor.position,
                runtime,
                &mut outputs,
            )
            .expect("grid-cursor routing must preserve compositor invariants");
    }
}

impl UiOutput<LocalGridSpace> for NvimGridOutput {
    fn emit(&mut self, command: UiCommand<LocalGridSpace>) {
        match command {
            UiCommand::GridResize(size) => {
                let context = registered_context();
                let mut composed = NvimComposedOutput;
                let mut uncomposed = NvimUncomposedOutput {
                    grid_id: self.target.grid,
                };
                let mut outputs = CompositorEventOutputs::new(&mut composed, &mut uncomposed);

                context
                    .compositor()
                    .route_grid_resize(self.target.grid, size, &mut outputs)
                    .expect("grid-resize routing must preserve compositor invariants");
            }
            UiCommand::GridCursorGoto(position) => registered_context()
                .ui()
                .schedule_grid_cursor(GridCursor::new(self.target.grid, position)),
            UiCommand::GridScroll { region, delta } => {
                let context = registered_context();
                let runtime =
                    compositor_runtime_snapshot().expect("compositor runtime must be valid");
                let request = CompositorScrollRequest::new(region, delta);
                let mut composed = NvimComposedOutput;
                let mut uncomposed = NvimUncomposedOutput {
                    grid_id: self.target.grid,
                };
                let mut outputs = CompositorEventOutputs::new(&mut composed, &mut uncomposed);

                context
                    .compositor()
                    .route_grid_scroll(
                        context.grid(),
                        current_message_grid_id(),
                        self.target.grid,
                        request,
                        runtime,
                        &mut outputs,
                    )
                    .expect("grid-scroll routing must preserve compositor invariants");
            }
            command => NvimUncomposedOutput {
                grid_id: self.target.grid,
            }
            .emit(command),
        }
    }

    fn emit_raw_line<Cells: RawLineCells>(&mut self, line: RawLineCommand<Cells>) {
        ui_may_set_default_colors();

        let row = line.row;
        let clear_column = line.clear_column;
        let context = registered_context();
        let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
        let mut composed = NvimComposedOutput;
        let mut uncomposed = NvimUncomposedOutput {
            grid_id: self.target.grid,
        };
        let mut outputs = CompositorEventOutputs::new(&mut composed, &mut uncomposed);

        context
            .compositor()
            .route_grid_raw_line(
                context.grid(),
                current_message_grid_id(),
                self.target.grid,
                line,
                runtime,
                &mut outputs,
            )
            .expect("raw-line routing must preserve compositor invariants");

        // SAFETY: Redraw options are read synchronously on Neovim's main
        // thread.
        let (write_delay, visualize_line) =
            unsafe { (p_wd, (rdb_flags & RdbFlags::Line as u32) != 0) };
        if write_delay != 0 && visualize_line {
            Self::publish_cursor(GridCursor::new(
                self.target.grid,
                GridPosition::new(row, std::cmp::min(clear_column, self.target.width - 1)),
            ));
            // SAFETY: Redraw-delay callbacks run synchronously on Neovim's
            // main thread after cursor delivery.
            unsafe {
                ui_call_flush();
                os_sleep(write_delay.unsigned_abs());
            }
            context.ui().force_grid_cursor_republication();
        }
    }
}

/// Schedules one grid-local cursor received through the C adapter.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_ui_schedule_grid_cursor(
    grid_handle: VimHandleT,
    row: libc::c_int,
    column: libc::c_int,
) {
    registered_context()
        .ui()
        .schedule_grid_cursor(GridCursor::new(
            GridId::new(grid_handle),
            GridPosition::new(row, column),
        ));
}

/// Schedules cursor republication when its grid moved.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_ui_mark_grid_cursor_moved(grid_handle: VimHandleT) {
    registered_context()
        .ui()
        .mark_grid_cursor_moved(GridId::new(grid_handle));
}

/// Resets cursor coordinates for a UI refresh while retaining its grid.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_ui_reset_grid_cursor_position() {
    registered_context().ui().reset_grid_cursor_position();
}

/// Returns the current grid-local cursor row.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_ui_current_grid_cursor_row() -> libc::c_int {
    registered_context().ui().current_grid_cursor().position.row
}

/// Returns the current grid-local cursor column.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_ui_current_grid_cursor_column() -> libc::c_int {
    registered_context()
        .ui()
        .current_grid_cursor()
        .position
        .column
}

/// Publishes and clears the pending grid cursor.
///
/// Returns whether a cursor was published and window output must be flushed
/// again after possible compositor layer movement.
#[unsafe(no_mangle)]
pub extern "C" fn rustvim_ui_flush_pending_grid_cursor() -> bool {
    let Some(cursor) = registered_context().ui().take_pending_grid_cursor() else {
        return false;
    };

    NvimGridOutput::publish_cursor(cursor);
    true
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

#[unsafe(no_mangle)]
pub extern "C" fn ui_default_colors_set() {
    // Throttle setting of default colors at startup, so it only happens once
    // if the user sets the colorscheme in startup.
    registered_context().ui().schedule_default_colors();

    // SAFETY: UI events and startup-state transitions run on Neovim's main
    // thread.
    if unsafe { starting == 0 } {
        ui_may_set_default_colors();
    }
}

fn ui_may_set_default_colors() {
    if registered_context().ui().take_pending_default_colors() {
        // SAFETY: UI output runs on Neovim's main thread. The color globals
        // are copied by value into this synchronous UI event.
        unsafe {
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

    let row_index = usize::try_from(row).expect("grid row must be nonnegative");
    let start = usize::try_from(start_col).expect("line start column must be nonnegative");
    let end = usize::try_from(end_col).expect("line end column must be nonnegative");
    let segment = GridRow::try_from_grid(grid, row_index)
        .and_then(|row| row.segment(start..end))
        .expect("valid grid line output");

    OwnedRawLineCommand::try_owned(
        row,
        start_col..end_col,
        clear_col,
        clear_attr,
        flags,
        segment.characters(),
        segment.attributes(),
        registered_context().ui().grid_output_buffer_pool(),
    )
    .expect("valid grid line output")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_grid_cursor_preserves_current_state_and_publication_rules() {
        let mut state = PendingGridCursorState::new();
        let initial = GridCursor::new(DEFAULT_GRID_ID, GridPosition::new(0, 0));

        assert_eq!(state.current(), initial);
        assert_eq!(state.take_pending(), None);

        state.schedule(initial);
        assert_eq!(state.take_pending(), None);

        let moved = GridCursor::new(GridId::new(2), GridPosition::new(3, 4));
        state.schedule(moved);
        assert_eq!(state.current(), moved);
        assert_eq!(state.take_pending(), Some(moved));
        assert_eq!(state.take_pending(), None);

        state.mark_grid_moved(DEFAULT_GRID_ID);
        assert_eq!(state.take_pending(), None);

        state.mark_grid_moved(moved.grid_id);
        assert_eq!(state.take_pending(), Some(moved));

        state.reset_position_for_refresh();
        let refreshed = GridCursor::new(moved.grid_id, GridPosition::new(0, 0));
        assert_eq!(state.current(), refreshed);
        assert_eq!(state.take_pending(), Some(refreshed));

        state.force_republication();
        assert_eq!(state.take_pending(), Some(refreshed));
    }
}
