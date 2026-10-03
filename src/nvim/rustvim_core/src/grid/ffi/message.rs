//! C boundary adapters for message-grid operations.

use crate::{
    context::registered_context,
    defines::Integer,
    ui::{
        GridOutputTarget,
        output::{OwnedRawLineCommand, RawLineCells, RawLineCommand, UiCommand, UiOutput},
    },
    ui_compositor::{NvimComposedOutput, compositor_runtime_snapshot},
};

use super::super::{
    CMessageGridPositionConfig, CMessageGridPositionEmitter, CMessageGridScrollFlushRequest,
    CMessageGridTransition, CMessageSurfaceRequest, CMessageSurfaceScrollRequest,
    CMessageSurfaceUpdate, ComposedGridSpace, GridId, GridSize, LineBufferGuard, LocalGridSpace,
    MessageGridMode, MessageGridScrollFlushRequest, MessageGridTransition, MessagePositionPolicy,
    MessagePositionRequest, MessageSurfaceOutput, StoredGridGuard, VimHandleT, VimSCharT,
    lock_grid_store, lock_message_grid_state,
};

unsafe extern "C" {
    fn ui_uncomposed_call_grid_resize(grid: Integer, width: Integer, height: Integer);
    fn ui_call_grid_destroy(grid: Integer);
}

/// Final C UI adapter for one complete message-surface transition.
struct NvimMessageSurfaceOutput {
    composed: NvimComposedOutput,
    emit_position: CMessageGridPositionEmitter,
}

impl UiOutput<ComposedGridSpace> for NvimMessageSurfaceOutput {
    fn emit(&mut self, command: UiCommand<ComposedGridSpace>) {
        self.composed.emit(command);
    }

    fn emit_raw_line<Cells: RawLineCells>(&mut self, command: RawLineCommand<Cells>) {
        self.composed.emit_raw_line(command);
    }
}

impl MessageSurfaceOutput for NvimMessageSurfaceOutput {
    fn emit_message_position(
        &mut self,
        grid_id: GridId,
        row: i32,
        scrolled: bool,
        separator_character: VimSCharT,
        z_index: i32,
        compositor_index: usize,
    ) {
        let compositor_index = i32::try_from(compositor_index)
            .expect("compositor layer index must fit the C UI event representation");
        // SAFETY: The FFI caller guarantees the callback remains valid for the
        // complete synchronous lifecycle operation.
        unsafe {
            (self.emit_position)(
                grid_id.as_native_handle(),
                row,
                scrolled,
                separator_character,
                z_index,
                compositor_index,
            );
        }
    }

    fn emit_grid_resize(&mut self, grid_id: GridId, size: GridSize) {
        // SAFETY: The identity and dimensions were validated by safe Rust, and
        // the generated callback is synchronous.
        unsafe {
            ui_uncomposed_call_grid_resize(
                Integer::from(grid_id.as_native_handle()),
                Integer::from(size.width),
                Integer::from(size.height),
            );
        }
    }

    fn emit_grid_destroy(&mut self, grid_id: GridId) {
        // SAFETY: The stable identity is delivered synchronously at the final
        // C UI boundary.
        unsafe {
            ui_call_grid_destroy(Integer::from(grid_id.as_native_handle()));
        }
    }

    fn emit_grid_command(&mut self, target: GridOutputTarget, command: UiCommand<LocalGridSpace>) {
        target.emit(command);
    }

    fn emit_grid_lines(&mut self, target: GridOutputTarget, lines: Vec<OwnedRawLineCommand>) {
        target.emit_lines(lines);
    }
}

/// Applies one complete message-surface lifecycle transition.
///
/// # Safety
///
/// `emit_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_update_message_surface(
    request: CMessageSurfaceRequest,
    emit_position: CMessageGridPositionEmitter,
) -> CMessageSurfaceUpdate {
    let request = request
        .try_into_native()
        .expect("message-surface request must contain valid editor state");
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let mut output = NvimMessageSurfaceOutput {
        composed: NvimComposedOutput,
        emit_position,
    };

    registered_context()
        .message_surface()
        .update(request, runtime, &mut output)
        .expect("message-surface update must preserve rendering invariants")
        .into()
}

/// Converts one FFI position update and delegates it to `MessageSurface`.
///
/// # Safety
///
/// `emit_position` must remain callable for the duration of this operation.
unsafe fn update_message_grid_position(
    grid_handle: VimHandleT,
    row: libc::c_int,
    scrolled: bool,
    config: CMessageGridPositionConfig,
    emit_position: CMessageGridPositionEmitter,
) {
    assert!(grid_handle > 0, "message grid handle must be positive");
    assert!(row >= 0, "message grid position must be nonnegative");
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let request = MessagePositionRequest::new(
        GridId::new(grid_handle),
        row,
        scrolled,
        runtime.screen_size(),
        MessagePositionPolicy::new(config.separator_character, config.z_index),
    );
    let mut output = NvimMessageSurfaceOutput {
        composed: NvimComposedOutput,
        emit_position,
    };
    registered_context()
        .message_surface()
        .set_position(request, runtime, &mut output)
        .expect("message-grid position update must preserve rendering invariants");
}

/// Converts one FFI position publication and delegates it to `MessageSurface`.
///
/// # Safety
///
/// `emit_position` must remain callable for the duration of this operation.
pub(super) unsafe fn publish_message_grid_position(
    grid_handle: VimHandleT,
    row: libc::c_int,
    scrolled: bool,
    config: CMessageGridPositionConfig,
    emit_position: CMessageGridPositionEmitter,
) {
    assert!(grid_handle > 0, "message grid handle must be positive");
    assert!(row >= 0, "message grid position must be nonnegative");
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let request = MessagePositionRequest::new(
        GridId::new(grid_handle),
        row,
        scrolled,
        runtime.screen_size(),
        MessagePositionPolicy::new(config.separator_character, config.z_index),
    );
    let mut output = NvimMessageSurfaceOutput {
        composed: NvimComposedOutput,
        emit_position,
    };
    registered_context()
        .message_surface()
        .publish_position(request, runtime, &mut output)
        .expect("message-grid position publication must preserve rendering invariants");
}

/// Prepares the message grid for its requested visibility and size.
///
/// A `ReleaseRequired` result leaves storage intact so C can remove the
/// compositor layer before calling `grid_release_message`.
#[unsafe(no_mangle)]
pub extern "C" fn grid_prepare_for_messages(
    grid_handle: VimHandleT,
    rows: libc::c_int,
    columns: libc::c_int,
    should_allocate: bool,
    scrolled_lines: libc::c_int,
) -> CMessageGridTransition {
    let mode = if should_allocate {
        let Some(size) = GridSize::try_new(rows, columns) else {
            return CMessageGridTransition::Unchanged;
        };
        MessageGridMode::Visible(size)
    } else {
        MessageGridMode::Hidden
    };

    let transition = {
        let mut line_buffer = LineBufferGuard::acquire();
        let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
            return CMessageGridTransition::Unchanged;
        };
        grid.prepare_for_messages(mode, &mut line_buffer)
    };

    if transition == MessageGridTransition::Resized {
        lock_message_grid_state()
            .scroll
            .record_resize(scrolled_lines);
    }
    transition.into()
}

/// Releases message-grid storage after compositor removal.
#[unsafe(no_mangle)]
pub extern "C" fn grid_release_message(grid_handle: VimHandleT) {
    if let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) {
        grid.release_message();
    }
}

/// Selects the allocated message grid for the adjusted message view.
#[unsafe(no_mangle)]
pub extern "C" fn grid_activate_message_view(grid_handle: VimHandleT) {
    let grid_id = GridId::new(grid_handle);
    {
        let store = lock_grid_store();
        store
            .grid(grid_id)
            .expect("message grid identity must resolve to permanent storage");
    }
    lock_message_grid_state().activate_view(grid_id);
}

/// Restores an adjusted message view to the shared default grid.
#[unsafe(no_mangle)]
pub extern "C" fn grid_reset_message_view() {
    lock_message_grid_state().reset_view();
}

/// Updates the message-grid position and its adjusted view.
///
/// The position callback and view mutation occur only after the grid-store
/// borrow has been released.
///
/// Returns whether the position was published immediately.
///
/// # Safety
///
/// `emit_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_set_message_position(
    grid_handle: VimHandleT,
    row: libc::c_int,
    scrolled: bool,
    config: CMessageGridPositionConfig,
    emit_position: CMessageGridPositionEmitter,
) {
    // SAFETY: The caller guarantees that the callback remains valid.
    unsafe {
        update_message_grid_position(grid_handle, row, scrolled, config, emit_position);
    }
}

/// Scrolls the message grid upward by one row.
///
/// Position callbacks and scroll events are emitted only after the grid-store
/// borrow has been released.
///
/// # Safety
///
/// `set_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_scroll_message_surface_up(
    request: CMessageSurfaceScrollRequest,
    emit_position: CMessageGridPositionEmitter,
) -> libc::c_int {
    let request = request
        .try_into_native()
        .expect("message scroll request must contain valid editor state");
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let mut output = NvimMessageSurfaceOutput {
        composed: NvimComposedOutput,
        emit_position,
    };
    registered_context()
        .message_surface()
        .scroll_up(request, runtime, &mut output)
        .expect("message scroll must preserve rendering invariants")
}

/// Clears message-grid rows in preparation for a screen redraw.
///
#[unsafe(no_mangle)]
pub extern "C" fn grid_prepare_message_redraw(
    grid_handle: VimHandleT,
    rows_to_clear: libc::c_int,
    valid_rows: libc::c_int,
) {
    let (Ok(rows_to_clear), Ok(valid_rows)) =
        (usize::try_from(rows_to_clear), usize::try_from(valid_rows))
    else {
        return;
    };

    {
        let Ok(mut grid) = StoredGridGuard::acquire(GridId::new(grid_handle)) else {
            return;
        };
        grid.prepare_message_redraw(rows_to_clear, valid_rows);
    }
    lock_message_grid_state().scroll.reset_progress();
}

/// Ends message scrolling, publishes its resting position, and clears hidden
/// message rows.
///
/// The position callback runs after throttling is disabled and the store
/// borrow is released, but before hidden rows are cleared. This preserves the
/// original event order.
///
/// # Safety
///
/// `set_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_reset_message_scroll(
    grid_handle: VimHandleT,
    position: libc::c_int,
    rows_to_clear: libc::c_int,
    config: CMessageGridPositionConfig,
    set_position: CMessageGridPositionEmitter,
) -> libc::c_int {
    let Ok(rows_to_clear) = usize::try_from(rows_to_clear) else {
        return position;
    };
    let grid_id = GridId::new(grid_handle);

    {
        let Ok(mut grid) = StoredGridGuard::acquire(grid_id) else {
            return position;
        };
        grid.disable_message_throttling();
    }
    // SAFETY: The caller guarantees that the callback is valid.
    unsafe {
        update_message_grid_position(grid_handle, position, false, config, set_position);
    }
    {
        if let Ok(mut grid) = StoredGridGuard::acquire(grid_id) {
            grid.clear_message_rows(rows_to_clear, 0);
        }
    }
    lock_message_grid_state().scroll.reset_progress();
    position
}

/// Flushes pending throttled message-grid scrolling and row updates.
///
/// The message-position callback and UI commands are emitted only after the
/// grid-store borrow has been released.
///
/// # Safety
///
/// `emit_position` must remain callable for the duration of this operation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_flush_message_scroll(
    grid_handle: VimHandleT,
    request: CMessageGridScrollFlushRequest,
    config: CMessageGridPositionConfig,
    emit_position: CMessageGridPositionEmitter,
) {
    if grid_handle <= 0 {
        return;
    }
    let Some(screen_size) = GridSize::try_new(request.screen_rows, request.screen_columns) else {
        return;
    };
    let request = MessageGridScrollFlushRequest {
        position: request.position,
        scrolled_lines: request.scrolled_lines,
        screen_size,
        clear_attribute: request.clear_attribute,
    };
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let position = MessagePositionPolicy::new(config.separator_character, config.z_index);
    let mut output = NvimMessageSurfaceOutput {
        composed: NvimComposedOutput,
        emit_position,
    };
    registered_context()
        .message_surface()
        .flush_scroll(
            GridId::new(grid_handle),
            request,
            position,
            runtime,
            &mut output,
        )
        .expect("message scroll flush must preserve rendering invariants");
}
