//! C ABI entry points and raw-pointer orchestration for the compositor.

use std::{ptr::NonNull, slice};

use crate::{
    defines::{CVimString, Integer},
    grid::{
        CScreenGrid, GridId, ScreenGridGuard, VimSAttrT, VimSCharT, default_grid as c_default_grid,
        msg_grid as c_msg_grid, schar_from_buf,
    },
};

use super::super::*;
use super::{
    NvimOutput, compositor_runtime_snapshot,
    grid_store::{CompositorLayerGuards, clear_layer_registry, initialize_layer_registry},
    initialize_debug_highlights,
};

/// Initializes the compositor with the process-global default grid.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_init() {
    // SAFETY: Initialization runs on Neovim's main thread before compositor
    // operations begin, and the default grid remains process-global.
    let initialized = unsafe { initialize_layer_registry(&raw mut c_default_grid) };
    assert!(initialized);
}

/// Releases all Rust-owned compositor state during process teardown.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_free_all_mem() {
    assert!(clear_layer_registry());
    assert!(lock_compositor_buffer_pool().release());
}

/// Initializes compositor debug highlights.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_syn_init() {
    initialize_debug_highlights();
}

/// Records one newly attached composed UI.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_attach() {
    lock_compositor_draw_state().register_ui();
}

/// Removes one composed UI and releases the buffer after the final detach.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_detach() {
    let detached_last = lock_compositor_draw_state()
        .unregister_ui()
        .expect("cannot unregister a composed UI when none are attached");
    if detached_last {
        assert!(lock_compositor_buffer_pool().release());
    }
}

/// Updates screen validity and returns its previous value.
///
/// Invalidating the composed screen also hides the message separator.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_set_screen_valid(valid: bool) -> bool {
    let previous = lock_compositor_draw_state().set_screen_valid(valid);
    if !valid {
        lock_message_compositor_state().hide_separator();
    }
    previous
}

/// Handles one generated grid-resize event.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_grid_resize(grid: Integer, width: Integer, height: Integer) {
    let (Ok(grid), Ok(width), Ok(height)) = (
        i32::try_from(grid),
        i32::try_from(width),
        i32::try_from(height),
    ) else {
        panic!("grid-resize arguments must fit native compositor coordinates");
    };
    let grid_id = GridId::new(grid);
    let size =
        CompositorSize::try_new(height, width).expect("grid-resize dimensions must be nonnegative");
    if grid_id != GridId::new(1) {
        return;
    }

    let mut output = NvimOutput;
    output.emit(UiCommand::GridResize(size));

    let width = usize::try_from(size.width).expect("validated width must fit usize");
    assert!(lock_compositor_buffer_pool().resize(width));
}

/// Hides the message separator after the composed screen is invalidated.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_hide_message_separator() {
    lock_message_compositor_state().hide_separator();
}

/// Updates message-grid state and executes its scrolling and recomposition.
///
/// Both explicit grids and all registered compositor layers are restored
/// before this function emits a scroll event, visualizes an operation, or
/// recomposes an area.
///
/// # Safety
///
/// `message_grid` must point to a valid, exclusively accessible `CScreenGrid`
/// whose buffers satisfy the ownership and layout requirements of
/// `ScreenGrid`.
///
/// `default_grid` must point to a second valid, exclusively accessible
/// `CScreenGrid` whose buffers satisfy the same requirements.
unsafe fn update_message_position(
    message_grid: *mut CScreenGrid,
    default_grid: *mut CScreenGrid,
    request: CompositorMessagePositionRequest,
    runtime: CompositorRuntime,
    output: &mut impl CompositorOutput,
) -> bool {
    if message_grid.is_null() || default_grid.is_null() || message_grid == default_grid {
        return false;
    }
    let should_draw = lock_compositor_draw_state().should_draw();

    let plan = {
        // SAFETY: The caller transfers exclusive ownership of both grids'
        // buffers into distinct guards for the duration of this operation.
        let mut message_grid = unsafe { ScreenGridGuard::take(message_grid) };
        let default_grid = unsafe { ScreenGridGuard::take(default_grid) };
        lock_message_compositor_state().update_position(
            &mut message_grid,
            &default_grid,
            request,
            should_draw,
        )
    };
    let Some(plan) = plan else {
        return false;
    };

    if let Some(scroll) = plan.scroll {
        output.emit(UiCommand::GridScroll {
            region: scroll.region,
            row_delta: scroll.row_delta,
            column_delta: 0,
        });
    }
    if let Some(area) = plan.recompose {
        // SAFETY: The caller satisfies the helper's message-grid requirements.
        // The earlier guards were dropped above.
        if !unsafe { recompose_area_from_registered_grids(message_grid, area, runtime, output) } {
            return false;
        }
    }
    true
}

/// Handles one generated message-grid position event.
///
/// # Safety
///
/// A non-null `separator_character.data` must point to `separator_character.size`
/// readable bytes for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_msg_set_pos(
    _grid: Integer,
    row: Integer,
    scrolled: bool,
    separator_character: CVimString,
    _z_index: Integer,
    _compositor_index: Integer,
) {
    let row = i32::try_from(row).expect("message-grid row must fit native coordinates");
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let separator_character = (!separator_character.data.is_null())
        .then(|| schar_from_buf(separator_character.data, separator_character.size));
    let request = CompositorMessagePositionRequest::new(
        row,
        separator_character,
        runtime.screen_size,
        scrolled,
    );
    let mut output = NvimOutput;

    // SAFETY: Neovim's global default and message grids are distinct and are
    // exclusively accessed on the main thread during this generated event.
    let updated = unsafe {
        update_message_position(
            &raw mut c_msg_grid,
            &raw mut c_default_grid,
            request,
            runtime,
            &mut output,
        )
    };
    assert!(updated);
}

/// Recomposes the screen area occupied by one grid when drawing is enabled.
///
/// # Safety
///
/// `grid` must point to an exclusively accessible C grid whose buffers satisfy
/// the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_compose_grid(grid: *mut CScreenGrid) {
    let grid = NonNull::new(grid).expect("grid to recompose must not be null");
    let grid_id = {
        // SAFETY: The caller transfers exclusive ownership for this scope.
        let grid = unsafe { ScreenGridGuard::take(grid.as_ptr()) };
        grid.id()
    };

    if !lock_compositor_draw_state().should_draw() {
        return;
    }
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let area =
        recompose_area_for_registered_grid(grid_id).expect("grid to recompose must be registered");
    let mut output = NvimOutput;

    // SAFETY: The grid guard was dropped above. The process-global message
    // grid is exclusively accessed on Neovim's main thread.
    assert!(unsafe {
        recompose_area_from_registered_grids(&raw mut c_msg_grid, area, runtime, &mut output)
    });
}

/// Places a new or existing grid in the registered compositor layer stack.
///
/// Rust determines whether the candidate is already registered and performs
/// either repositioning or insertion. Grid ownership is restored before every
/// recomposition operation.
///
/// # Safety
///
/// If `grid` is not registered, it must satisfy
/// `CompositorLayerGuards::try_take_registered_with_candidate` requirements.
/// `current_window_grid_handle` may be zero or identify a registered grid.
///
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_put_grid(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    column: libc::c_int,
    height: libc::c_int,
    width: libc::c_int,
    valid: bool,
    on_top: bool,
    current_window_grid_handle: Integer,
) -> bool {
    let grid = NonNull::new(grid).expect("grid to place must not be null");
    let request =
        CompositorGridPlacementRequest::try_new(row, column, height, width, valid, on_top)
            .expect("grid placement dimensions must be nonnegative");
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let current_window_grid_id = match i32::try_from(current_window_grid_handle) {
        Ok(0) => None,
        Ok(handle) => Some(GridId::new(handle)),
        Err(_) => panic!("current-window grid handle must fit GridId"),
    };
    let should_draw = lock_compositor_draw_state().should_draw();

    // SAFETY: The caller satisfies the candidate ownership requirements.
    let Some((layer_guards, target)) =
        (unsafe { CompositorLayerGuards::try_take_registered_with_candidate(grid) })
    else {
        return false;
    };
    let current_window_layer =
        current_window_grid_id.and_then(|grid_id| layer_guards.registry.index_of(grid_id));
    let mut output = NvimOutput;
    let recompose_area = |area| {
        // SAFETY: Every grid guard is dropped before this closure. The
        // process-global message grid is accessed on Neovim's main thread.
        unsafe {
            recompose_area_from_registered_grids(&raw mut c_msg_grid, area, runtime, &mut output)
        }
    };
    place_grid_in_registered_layers(
        layer_guards,
        target,
        current_window_layer,
        request,
        should_draw,
        recompose_area,
    )
    .expect("grid placement must preserve compositor invariants")
}

/// Applies one cursor update to the compositor layer stack.
///
/// Any required layer movement is completed before recomposition and cursor
/// output are emitted. Every grid is restored before visualization or UI
/// callbacks.
///
/// # Safety
///
/// `message_grid` must satisfy the message-grid requirements of
/// `recompose_area_from_registered_grids`.
unsafe fn process_grid_cursor_goto(
    message_grid: *mut CScreenGrid,
    request: CompositorCursorRequest,
    output: &mut impl CompositorOutput,
) -> bool {
    let Some(update) = apply_cursor_update_to_registered_grids(request) else {
        return false;
    };
    let CompositorCursorUpdate::Apply {
        plan,
        recompose_areas,
    } = update
    else {
        return true;
    };

    let Some(runtime) = compositor_runtime_snapshot() else {
        return false;
    };
    for area in recompose_areas {
        // SAFETY: The caller satisfies the helper's message-grid requirements.
        // The cursor-update guards were dropped.
        if !unsafe { recompose_area_from_registered_grids(message_grid, area, runtime, output) } {
            return false;
        }
    }

    if plan.emit {
        output.emit(UiCommand::GridCursorGoto(plan.screen_position));
    }

    true
}

/// Handles one generated grid-cursor event.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_grid_cursor_goto(grid_handle: Integer, row: Integer, column: Integer) {
    let (Ok(grid_handle), Ok(row), Ok(column)) = (
        i32::try_from(grid_handle),
        i32::try_from(row),
        i32::try_from(column),
    ) else {
        panic!("grid-cursor arguments must fit native compositor coordinates");
    };
    let request = CompositorCursorRequest {
        grid_id: GridId::new(grid_handle),
        local_position: CompositorPoint { row, column },
    };
    let mut output = NvimOutput;

    // SAFETY: Neovim's message grid is exclusively accessed on the main
    // thread during this generated event.
    let updated = unsafe { process_grid_cursor_goto(&raw mut c_msg_grid, request, &mut output) };
    assert!(updated);
}

/// Adjusts the layer position of one compositor grid.
///
/// # Safety
///
/// `grid` must point to an exclusively accessible C grid whose buffers satisfy
/// the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_layers_adjust(grid: *mut CScreenGrid, raise: bool) {
    let grid = NonNull::new(grid).expect("grid to adjust must not be null");
    let grid_id = {
        // SAFETY: The caller transfers exclusive ownership for this scope.
        let grid = unsafe { ScreenGridGuard::take(grid.as_ptr()) };
        grid.id()
    };
    let adjustment = if raise {
        CompositorLayerAdjustment::Raise
    } else {
        CompositorLayerAdjustment::Lower
    };

    assert!(adjust_registered_layer_order(grid_id, adjustment));
}

/// Removes one grid from the registered layer stack and recomposes its area.
///
/// An unregistered grid is an expected successful no-op.
///
/// # Safety
///
/// `grid` must point to an exclusively accessible non-default C grid whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_remove_grid(grid: *mut CScreenGrid) {
    let grid = NonNull::new(grid).expect("grid to remove must not be null");
    assert_ne!(grid.as_ptr(), &raw mut c_default_grid);
    let grid_id = {
        // SAFETY: The caller transfers exclusive ownership for this scope.
        let grid = unsafe { ScreenGridGuard::take(grid.as_ptr()) };
        grid.id()
    };

    let should_draw = lock_compositor_draw_state().should_draw();
    let layer_guards =
        CompositorLayerGuards::try_take_registered().expect("compositor registry must be valid");
    let Some(layer_index) = layer_guards.registry.index_of(grid_id) else {
        return;
    };
    let removal = remove_registered_layer(layer_guards, layer_index, should_draw)
        .expect("registered layer removal must preserve compositor invariants");

    if let Some(area) = removal.recompose_area {
        let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
        let mut output = NvimOutput;
        // SAFETY: The reduced registry has been published, and the caller
        // accesses the process-global message grid on Neovim's main thread.
        assert!(unsafe {
            recompose_area_from_registered_grids(&raw mut c_msg_grid, area, runtime, &mut output)
        });
    }
}

/// Finds the topmost non-default compositor grid at one screen coordinate.
///
/// Returns null when no matching grid exists or the registered state is
/// invalid.
/// Every temporarily owned grid is restored before this function returns.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_find_grid_at_coord(
    row: libc::c_int,
    column: libc::c_int,
    require_mouse_input: bool,
) -> *mut CScreenGrid {
    let hit_test = if require_mouse_input {
        LayerHitTest::MouseEnabled
    } else {
        LayerHitTest::Any
    };

    let Some(layer_guards) = CompositorLayerGuards::try_take_registered() else {
        return std::ptr::null_mut();
    };
    let grid_id = {
        let Some(layers) = layer_guards.layer_stack() else {
            return std::ptr::null_mut();
        };
        layers.topmost_grid_at(CompositorPoint { row, column }, hit_test)
    };
    let Some(cgrid) = grid_id.and_then(|grid_id| layer_guards.cgrid_by_id(grid_id)) else {
        return std::ptr::null_mut();
    };

    cgrid
}

/// Reports whether one grid contains a composed-screen coordinate.
///
/// The grid is positioned using its stored compositor origin.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid` whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_grid_contains_at(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    column: libc::c_int,
) -> bool {
    if grid.is_null() {
        return false;
    }

    // SAFETY: The caller transfers exclusive ownership of the grid buffers
    // into the guard for the duration of this operation.
    let grid = unsafe { ScreenGridGuard::take(grid) };
    PositionedGrid::try_from_compositor_position(&grid)
        .is_some_and(|grid| grid.contains(CompositorPoint { row, column }))
}

/// Reports whether one window grid accepts mouse input at a screen coordinate.
///
/// Unlike compositor layers, multigrid window fallbacks use the window's
/// screen origin supplied by C.
///
/// # Safety
///
/// `grid` must point to a valid, exclusively accessible `CScreenGrid` whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_grid_accepts_mouse_at(
    grid: *mut CScreenGrid,
    origin_row: libc::c_int,
    origin_column: libc::c_int,
    row: libc::c_int,
    column: libc::c_int,
) -> bool {
    if grid.is_null() {
        return false;
    }

    // SAFETY: The caller transfers exclusive ownership of the grid buffers
    // into the guard for the duration of this operation.
    let grid = unsafe { ScreenGridGuard::take(grid) };
    PositionedGrid::try_new(
        &grid,
        CompositorPoint {
            row: origin_row,
            column: origin_column,
        },
    )
    .is_some_and(|grid| grid.matches(CompositorPoint { row, column }, LayerHitTest::MouseEnabled))
}

/// Reports whether a sufficiently higher compositor layer covers a grid's
/// cursor.
///
/// # Safety
///
/// `current_grid` must point to an exclusively accessible C grid whose buffers
/// satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_cursor_is_covered_by_higher_layer(
    current_grid: *mut CScreenGrid,
    row: libc::c_int,
    column: libc::c_int,
    minimum_z_index_gap: libc::c_int,
) -> bool {
    if !lock_compositor_draw_state().should_draw() {
        return false;
    }
    let Some(current_grid) = NonNull::new(current_grid) else {
        return false;
    };
    let Ok(minimum_z_index_gap) = u32::try_from(minimum_z_index_gap) else {
        return false;
    };
    let current_grid = {
        // SAFETY: The caller transfers exclusive ownership for this scope.
        let grid = unsafe { ScreenGridGuard::take(current_grid.as_ptr()) };
        CompositorCursorGrid::from(&*grid)
    };

    cursor_is_covered_by_registered_layers(current_grid, row, column, minimum_z_index_gap)
        .unwrap_or(false)
}

/// Composes and submits one validated line from registered compositor grids.
///
/// # Safety
///
/// When `request.row` is the message-separator row, `message_grid` must
/// either identify one registered grid or identify another exclusively
/// accessible grid satisfying the `ScreenGrid` ownership and layout
/// requirements.
unsafe fn compose_line_from_registered_grids(
    message_grid: *mut CScreenGrid,
    request: CompositorLineInput,
    output: &mut impl CompositorOutput,
) -> bool {
    let separator_character = lock_message_compositor_state().separator_character_at(request.row);
    let separator_active = separator_character.is_some();
    if separator_active && message_grid.is_null() {
        return false;
    }

    let mut compositor = acquire_compositor_buffer();
    let submission = (|| {
        let layer_guards = CompositorLayerGuards::try_take_registered()?;
        let message_grid_index = separator_active
            .then(|| layer_guards.index_of(message_grid))
            .flatten();
        let separate_message_grid = separator_active && message_grid_index.is_none();

        let message_grid_guard = separate_message_grid.then(|| {
            // SAFETY: The non-null message grid is distinct from every layer
            // pointer and the caller transfers ownership for this operation.
            unsafe { ScreenGridGuard::take(message_grid) }
        });

        let layers = layer_guards.layer_stack()?;
        let message_separator = if separator_active {
            let message_grid = match message_grid_index {
                Some(index) => layer_guards.grid(index)?,
                None => message_grid_guard.as_deref()?,
            };
            Some(MessageSeparator {
                layer: CompositorLayer::try_from_grid(
                    message_grid,
                    std::ptr::eq(message_grid, layer_guards.grid(0)?),
                )?,
                character: separator_character?,
                attribute: request.settings.message_separator_attribute,
            })
        } else {
            None
        };

        compositor.try_compose_line(
            CompositorLineRequest {
                row: request.row,
                start_column: request.columns.start,
                end_column: request.columns.end,
                flags: request.flags,
                message_separator,
                screen_columns: request.settings.screen_columns,
                abort_on_invalid: request.settings.abort_on_invalid,
            },
            &layers,
        )
    })();

    let Some(submission) = submission else {
        return false;
    };
    let output_range = submission.start_col..submission.end_col;
    let Some(characters) = compositor.characters.get(output_range.clone()) else {
        return false;
    };
    let Some(attributes) = compositor.attributes.get(output_range) else {
        return false;
    };
    let (Ok(start_column), Ok(end_column)) = (
        i32::try_from(submission.start_col),
        i32::try_from(submission.end_col),
    ) else {
        return false;
    };
    let Some(command) = RawLineCommand::try_new(
        request.row,
        start_column..end_column,
        end_column,
        0,
        submission.flags,
        characters,
        attributes,
    ) else {
        return false;
    };
    output.emit(UiCommand::RawLine(command));
    true
}

/// Composes and submits a clipped screen area from registered compositor grids.
///
/// # Safety
///
/// When a requested row is the message-separator row, `message_grid` must
/// either identify one registered grid or identify another exclusively
/// accessible grid satisfying the `ScreenGrid` ownership and layout
/// requirements. Grid ownership is restored before each synchronous UI call
/// so compositor callbacks may reenter C safely.
unsafe fn compose_area_from_registered_grids(
    message_grid: *mut CScreenGrid,
    request: CompositorAreaRequest,
    output: &mut impl CompositorOutput,
) -> bool {
    let CompositorAreaRequest { area, settings } = request;
    let Some(area) = clip_recompose_area_to_default_grid(area) else {
        return false;
    };
    if area.columns.end <= area.columns.start {
        return true;
    }

    for row in area.rows {
        // SAFETY: The caller satisfies the documented message-grid
        // requirements. The temporary clipping guards and every preceding
        // line's guards were dropped before this helper takes ownership.
        if !unsafe {
            compose_line_from_registered_grids(
                message_grid,
                CompositorLineInput {
                    row,
                    columns: area.columns.clone(),
                    flags: LINE_FLAG_INVALID,
                    settings,
                },
                output,
            )
        } {
            return false;
        }
    }
    true
}

/// Visualizes and composes one screen area from registered grids.
///
/// # Safety
///
/// When a requested row is the message-separator row, `message_grid` must
/// either identify one registered grid or identify another exclusively
/// accessible grid satisfying the `ScreenGrid` ownership and layout
/// requirements.
unsafe fn recompose_area_from_registered_grids(
    message_grid: *mut CScreenGrid,
    area: CompositorRecomposeArea,
    runtime: CompositorRuntime,
    output: &mut impl CompositorOutput,
) -> bool {
    runtime.visualize(&area, runtime.debug_highlights.recomposed, true, output);

    // SAFETY: The caller satisfies the helper's message-grid requirements.
    // Debug visualization completed without retaining any grid guard.
    unsafe {
        compose_area_from_registered_grids(
            message_grid,
            CompositorAreaRequest {
                area,
                settings: runtime.line_settings,
            },
            output,
        )
    }
}

/// Processes one complete raw-line event against registered compositor grids.
///
/// # Safety
///
/// When the forwarded character count is nonzero, `characters` and
/// `attributes` must point to readable arrays containing at least that many
/// initialized elements.
///
/// A disabled compositor or a grid handle absent from the registered layers
/// produces a successful no-op. `message_grid` is used only for pointer
/// identity and may be null.
unsafe fn process_raw_line(
    message_grid: *mut CScreenGrid,
    grid_id: GridId,
    request: CompositorRawLineRequest,
    clear_attribute: VimSAttrT,
    characters: *const VimSCharT,
    attributes: *const VimSAttrT,
    output: &mut impl CompositorOutput,
) -> bool {
    if !lock_compositor_draw_state().should_draw() {
        return true;
    }
    let Some(runtime) = compositor_runtime_snapshot() else {
        return false;
    };

    let message_was_scrolled = lock_message_compositor_state().was_scrolled;
    let plan = {
        let Some(layer_guards) = CompositorLayerGuards::try_take_registered() else {
            return false;
        };
        let message_layer_index = layer_guards.index_of(message_grid);
        let Some(layers) = layer_guards.layer_stack() else {
            return false;
        };
        let Some(current_layer_index) = layers.layer_index_with_id(grid_id) else {
            return true;
        };

        let Some(plan) = layers.plan_raw_line(
            current_layer_index,
            message_layer_index,
            message_was_scrolled,
            request,
        ) else {
            return false;
        };
        plan
    };

    if plan.clear_column != plan.requested_clear_column {
        log_raw_line_diagnostic(
            RawLineDiagnostic::ClippedColumns(plan.requested_clear_column),
            grid_id,
            output,
        );
    }

    match plan.action {
        CompositorRawLineAction::DiscardRow => {
            log_raw_line_diagnostic(RawLineDiagnostic::InvalidRow(plan.row), grid_id, output);
            true
        }
        CompositorRawLineAction::DiscardColumns => true,
        CompositorRawLineAction::Recompose => {
            runtime.visualize(
                &CompositorRecomposeArea {
                    rows: plan.row..plan.row + 1,
                    columns: plan.columns.start..plan.clear_column,
                },
                runtime.debug_highlights.composed,
                true,
                output,
            );
            // SAFETY: The caller satisfies the documented message-grid
            // requirements. The previous guards were dropped before this
            // helper takes ownership again.
            unsafe {
                compose_line_from_registered_grids(
                    message_grid,
                    CompositorLineInput {
                        row: plan.row,
                        columns: plan.columns.start..plan.clear_column,
                        flags: plan.flags,
                        settings: runtime.line_settings,
                    },
                    output,
                )
            }
        }
        CompositorRawLineAction::Forward => {
            let Some(payload_len) = plan
                .columns
                .end
                .checked_sub(plan.columns.start)
                .and_then(|length| usize::try_from(length).ok())
            else {
                return false;
            };
            if payload_len != 0 && (characters.is_null() || attributes.is_null()) {
                return false;
            }

            let (characters, attributes): (&[VimSCharT], &[VimSAttrT]) = if payload_len == 0 {
                (&[], &[])
            } else {
                // SAFETY: The caller guarantees two readable arrays with
                // at least the forwarded number of initialized elements.
                unsafe {
                    (
                        slice::from_raw_parts(characters, payload_len),
                        slice::from_raw_parts(attributes, payload_len),
                    )
                }
            };

            runtime.visualize(
                &CompositorRecomposeArea {
                    rows: plan.row..plan.row + 1,
                    columns: plan.columns.clone(),
                },
                runtime.debug_highlights.normal,
                plan.columns.end >= plan.clear_column,
                output,
            );
            runtime.visualize(
                &CompositorRecomposeArea {
                    rows: plan.row..plan.row + 1,
                    columns: plan.columns.end..plan.clear_column,
                },
                runtime.debug_highlights.clear,
                true,
                output,
            );
            debug_assert!(attributes.iter().all(|attribute| *attribute >= 0));

            let Some(command) = RawLineCommand::try_new(
                plan.row,
                plan.columns,
                plan.clear_column,
                clear_attribute,
                plan.flags,
                characters,
                attributes,
            ) else {
                return false;
            };
            output.emit(UiCommand::RawLine(command));
            true
        }
    }
}

/// Handles one generated raw-line event.
///
/// # Safety
///
/// When `end_col - start_col` is nonzero, `characters` and `attributes` must
/// point to readable arrays containing at least that many initialized cells.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_raw_line(
    grid: Integer,
    row: Integer,
    start_col: Integer,
    end_col: Integer,
    clear_col: Integer,
    clear_attr: Integer,
    flags: libc::c_int,
    characters: *const VimSCharT,
    attributes: *const VimSAttrT,
) {
    let (Ok(grid), Ok(row), Ok(start_col), Ok(end_col), Ok(clear_col), Ok(clear_attr)) = (
        i32::try_from(grid),
        i32::try_from(row),
        i32::try_from(start_col),
        i32::try_from(end_col),
        i32::try_from(clear_col),
        i32::try_from(clear_attr),
    ) else {
        panic!("raw-line arguments must fit native compositor coordinates");
    };
    let request = CompositorRawLineRequest::try_new(row, start_col, end_col, clear_col, flags)
        .expect("raw-line columns must be ordered");
    let mut output = NvimOutput;

    // SAFETY: The generated caller supplies the documented cell arrays, and
    // Neovim's message grid is exclusively accessed on the main thread.
    let processed = unsafe {
        process_raw_line(
            &raw mut c_msg_grid,
            GridId::new(grid),
            request,
            clear_attr,
            characters,
            attributes,
            &mut output,
        )
    };
    assert!(processed);
}

/// Processes one complete grid scroll against registered compositor grids.
///
/// # Safety
///
/// A disabled compositor or a grid handle absent from the registered layers
/// produces a successful no-op. `message_grid` is used only for pointer
/// identity and may be null.
unsafe fn process_scroll(
    message_grid: *mut CScreenGrid,
    grid_id: GridId,
    request: CompositorScrollRequest,
    output: &mut impl CompositorOutput,
) -> bool {
    if !lock_compositor_draw_state().should_draw() {
        return true;
    }
    let Some(runtime) = compositor_runtime_snapshot() else {
        return false;
    };

    let message_was_scrolled = lock_message_compositor_state().was_scrolled;
    let operation = {
        let Some(layer_guards) = CompositorLayerGuards::try_take_registered() else {
            return false;
        };
        let message_layer_index = layer_guards.index_of(message_grid);
        let Some(layers) = layer_guards.layer_stack() else {
            return false;
        };
        let Some(current_layer_index) = layers.layer_index_with_id(grid_id) else {
            return true;
        };
        let Some(plan) = layers.plan_scroll(
            current_layer_index,
            message_layer_index,
            message_was_scrolled,
            request,
        ) else {
            return false;
        };
        let Some(current_grid) = layer_guards.grid(current_layer_index) else {
            return false;
        };
        plan.prepare(current_grid)
    };

    match operation {
        CompositorScrollOperation::Forward {
            region,
            row_delta,
            column_delta,
        } => {
            output.emit(UiCommand::GridScroll {
                region,
                row_delta,
                column_delta,
            });
            if runtime.debug_enabled {
                runtime.delay(2, output);
            }
        }
        CompositorScrollOperation::Recompose { region, rows } => {
            runtime.visualize(
                &CompositorRecomposeArea {
                    rows: region.rows.clone(),
                    columns: region.columns.clone(),
                },
                runtime.debug_highlights.recomposed,
                true,
                output,
            );
            for row in rows {
                // SAFETY: The caller satisfies the documented message-grid
                // requirements. The planning guards were dropped before this
                // helper takes ownership again.
                if !unsafe {
                    compose_line_from_registered_grids(
                        message_grid,
                        CompositorLineInput {
                            row,
                            columns: region.columns.clone(),
                            flags: 0,
                            settings: runtime.line_settings,
                        },
                        output,
                    )
                } {
                    return false;
                }
            }
        }
    }
    true
}

/// Handles one generated grid-scroll event.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_grid_scroll(
    grid: Integer,
    top: Integer,
    bottom: Integer,
    left: Integer,
    right: Integer,
    rows: Integer,
    columns: Integer,
) {
    let (Ok(grid), Ok(top), Ok(bottom), Ok(left), Ok(right), Ok(rows), Ok(columns)) = (
        i32::try_from(grid),
        i32::try_from(top),
        i32::try_from(bottom),
        i32::try_from(left),
        i32::try_from(right),
        i32::try_from(rows),
        i32::try_from(columns),
    ) else {
        panic!("grid-scroll arguments must fit native compositor coordinates");
    };
    let request = CompositorScrollRequest::try_new(top, bottom, left, right, rows, columns)
        .expect("grid-scroll region must be ordered");
    let mut output = NvimOutput;

    // SAFETY: Neovim's message grid is exclusively accessed on the main
    // thread during this generated event.
    let processed =
        unsafe { process_scroll(&raw mut c_msg_grid, GridId::new(grid), request, &mut output) };
    assert!(processed);
}
