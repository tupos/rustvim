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
    grid_store::{
        LegacyGridStore, clear_grid_bindings, initialize_grid_bindings,
        replace_compositor_grid_bindings,
    },
    initialize_debug_highlights,
};

/// Initializes the compositor with the process-global default grid.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_init() {
    // SAFETY: Initialization runs on Neovim's main thread before compositor
    // operations begin, and the default grid remains process-global.
    let default_grid_id = unsafe { initialize_grid_bindings(&raw mut c_default_grid) }
        .expect("compositor grid bindings must initialize exactly once");
    lock_compositor()
        .initialize_layers(default_grid_id)
        .expect("compositor layers must initialize exactly once");
}

/// Releases all Rust-owned compositor state during process teardown.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_free_all_mem() {
    lock_compositor()
        .clear_layers()
        .expect("compositor layers must be initialized");
    clear_grid_bindings().expect("compositor grid bindings must be initialized");
    lock_compositor_buffer_pool().release();
}

/// Initializes compositor debug highlights.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_syn_init() {
    initialize_debug_highlights();
}

/// Records one newly attached composed UI.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_attach() {
    lock_compositor().attach();
}

/// Removes one composed UI and releases the buffer after the final detach.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_detach() {
    let detached_last = lock_compositor()
        .detach()
        .expect("cannot unregister a composed UI when none are attached");
    if detached_last {
        lock_compositor_buffer_pool().release();
    }
}

/// Updates screen validity and returns its previous value.
///
/// Invalidating the composed screen also hides the message separator.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_set_screen_valid(valid: bool) -> bool {
    lock_compositor().set_screen_valid(valid)
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
    let size =
        GridSize::try_new(height, width).expect("grid-resize dimensions must be nonnegative");
    if GridId::new(grid) != GridId::new(1) {
        return;
    }
    let plan = lock_compositor()
        .resize(size)
        .expect("validated screen width must fit compositor storage");

    let mut output = NvimOutput;
    output.emit(UiCommand::GridResize(plan.size));

    lock_compositor_buffer_pool().resize(plan.buffer_width);
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
) -> CompositorResult<()> {
    if message_grid.is_null() || default_grid.is_null() || message_grid == default_grid {
        return Err(CompositorError::InvalidLayerState);
    }
    let plan = {
        // SAFETY: The caller transfers exclusive ownership of both grids'
        // buffers into distinct guards for the duration of this operation.
        let mut message_grid = unsafe { ScreenGridGuard::take(message_grid) };
        let default_grid = unsafe { ScreenGridGuard::take(default_grid) };
        lock_compositor().set_message_position(&mut message_grid, &default_grid, request)
    }?;

    if let Some(scroll) = plan.scroll {
        output.emit(UiCommand::GridScroll {
            region: scroll.region,
            delta: scroll.delta,
        });
    }
    if let Some(area) = plan.recompose {
        // SAFETY: The caller satisfies the helper's message-grid requirements.
        // The earlier guards were dropped above.
        unsafe { recompose_area_from_registered_grids(message_grid, area, runtime, output) }
            .map_err(|_| CompositorError::RecompositionFailed)?;
    }
    Ok(())
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
    unsafe {
        update_message_position(
            &raw mut c_msg_grid,
            &raw mut c_default_grid,
            request,
            runtime,
            &mut output,
        )
    }
    .expect("message-grid position update must preserve compositor invariants");
}

/// Returns the screen area occupied by one registered grid.
pub(in crate::ui_compositor) fn recompose_area_for_registered_grid(
    grid_id: GridId,
) -> CompositorResult<CompositorRecomposeArea> {
    let grid_store =
        LegacyGridStore::try_take_registered().ok_or(CompositorError::InvalidLayerState)?;
    lock_compositor().recompose_area_for_grid(&grid_store, grid_id)
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

    if !lock_compositor().should_draw() {
        return;
    }
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let area =
        recompose_area_for_registered_grid(grid_id).expect("grid to recompose must be registered");
    let mut output = NvimOutput;

    // SAFETY: The grid guard was dropped above. The process-global message
    // grid is exclusively accessed on Neovim's main thread.
    unsafe {
        recompose_area_from_registered_grids(&raw mut c_msg_grid, area, runtime, &mut output)
    }
    .expect("grid recomposition must preserve compositor invariants");
}

/// Places a new or existing grid in the registered compositor layers.
///
/// A new binding is published before recomposition. An existing grid is
/// restored to its requested placement even if recomposing an exposed area
/// fails. Every grid lease is released before invoking the callback.
pub(in crate::ui_compositor) fn place_grid_in_registered_layers(
    grid_store: LegacyGridStore,
    target: CompositorGridPlacementTarget,
    current_window_layer: Option<usize>,
    request: CompositorGridPlacementRequest,
    mut recompose_area: impl FnMut(CompositorRecomposeArea) -> CompositorResult<()>,
) -> CompositorResult<CompositorGridPlacementOutcome> {
    let preparation = {
        let mut grid_store = grid_store;
        let preparation = lock_compositor().prepare_grid_placement(
            &mut grid_store,
            target,
            current_window_layer,
            request,
        )?;

        if !target.was_registered {
            let replacement_bindings = grid_store
                .ffi_bindings()
                .ok_or(CompositorError::InvalidLayerState)?;
            replace_compositor_grid_bindings(replacement_bindings)?;
        }
        preparation
    };

    let outcome = CompositorGridPlacementOutcome::from_moved(preparation.plan.moved);
    let exposed_areas_result = preparation
        .plan
        .exposed_areas
        .iter()
        .cloned()
        .try_for_each(&mut recompose_area);

    if let Some(completion) = preparation.deferred_completion {
        let mut grid_store =
            LegacyGridStore::try_take_registered().ok_or(CompositorError::InvalidLayerState)?;
        lock_compositor().finish_grid_placement(&mut grid_store, completion)?;
    }

    exposed_areas_result?;
    if let Some(area) = preparation.plan.redraw_area {
        recompose_area(area)?;
    }

    Ok(outcome)
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
/// `LegacyGridStore::try_take_registered_with_candidate` requirements.
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

    // SAFETY: The caller satisfies the candidate ownership requirements.
    let Some((grid_store, grid_id, was_registered)) =
        (unsafe { LegacyGridStore::try_take_registered_with_candidate(grid) })
    else {
        return false;
    };
    let (target, current_window_layer) = {
        let compositor = lock_compositor();
        let target = compositor
            .grid_placement_target(grid_id, was_registered)
            .expect("grid binding and compositor layers must agree");
        let current_window_layer = current_window_grid_id
            .map(|grid_id| compositor.layer_index(grid_id))
            .transpose()
            .expect("compositor layers must be initialized")
            .flatten();
        (target, current_window_layer)
    };
    let mut output = NvimOutput;
    let recompose_area = |area| {
        // SAFETY: Every grid guard is dropped before this closure. The
        // process-global message grid is accessed on Neovim's main thread.
        unsafe {
            recompose_area_from_registered_grids(&raw mut c_msg_grid, area, runtime, &mut output)
        }
        .map_err(|_| CompositorError::RecompositionFailed)
    };
    place_grid_in_registered_layers(
        grid_store,
        target,
        current_window_layer,
        request,
        recompose_area,
    )
    .expect("grid placement must preserve compositor invariants")
    .did_move()
}

/// Applies one cursor update and publishes any resulting layer movement.
pub(in crate::ui_compositor) fn apply_cursor_update_to_registered_grids(
    request: CompositorCursorRequest,
) -> CompositorResult<CompositorCursorUpdate> {
    let mut grid_store =
        LegacyGridStore::try_take_registered().ok_or(CompositorError::InvalidLayerState)?;
    lock_compositor().move_cursor(&mut grid_store, request)
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
) -> CompositorResult<()> {
    let update = apply_cursor_update_to_registered_grids(request)?;
    let CompositorCursorUpdate::Apply {
        plan,
        recompose_areas,
    } = update
    else {
        return Ok(());
    };

    let runtime = compositor_runtime_snapshot().ok_or(CompositorError::InvalidGeometry)?;
    for area in recompose_areas {
        // SAFETY: The caller satisfies the helper's message-grid requirements.
        // The cursor-update guards were dropped.
        unsafe { recompose_area_from_registered_grids(message_grid, area, runtime, output) }?;
    }

    if plan.emit {
        output.emit(UiCommand::GridCursorGoto(plan.screen_position));
    }

    Ok(())
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
        local_position: GridPosition::new(row, column),
    };
    let mut output = NvimOutput;

    // SAFETY: Neovim's message grid is exclusively accessed on the main
    // thread during this generated event.
    unsafe { process_grid_cursor_goto(&raw mut c_msg_grid, request, &mut output) }
        .expect("cursor update must preserve compositor invariants");
}

/// Adjusts a registered grid's layer order.
pub(in crate::ui_compositor) fn adjust_registered_layer_order(
    grid_id: GridId,
    adjustment: CompositorLayerAdjustment,
) -> CompositorResult<()> {
    let mut grid_store =
        LegacyGridStore::try_take_registered().ok_or(CompositorError::InvalidLayerState)?;
    lock_compositor().adjust_layer_order(&mut grid_store, grid_id, adjustment)
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

    adjust_registered_layer_order(grid_id, adjustment)
        .expect("layer adjustment must preserve compositor invariants");
}

/// Removes one registered grid and publishes the reduced C bindings.
pub(in crate::ui_compositor) fn remove_registered_layer(
    mut grid_store: LegacyGridStore,
    layer_index: usize,
) -> CompositorResult<CompositorLayerRemoval> {
    let (grid_id, removal) = {
        let mut compositor = lock_compositor();
        let grid_id = compositor.layer_id(layer_index)?;
        let removal = compositor.remove_layer(&mut grid_store, layer_index)?;
        (grid_id, removal)
    };
    grid_store.remove_binding(grid_id)?;
    let remaining_bindings = grid_store
        .ffi_bindings()
        .ok_or(CompositorError::InvalidLayerState)?;
    replace_compositor_grid_bindings(remaining_bindings)?;
    Ok(removal)
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

    let grid_store =
        LegacyGridStore::try_take_registered().expect("compositor registry must be valid");
    let Some(layer_index) = lock_compositor()
        .layer_index(grid_id)
        .expect("compositor layers must be initialized")
    else {
        return;
    };
    let removal = remove_registered_layer(grid_store, layer_index)
        .expect("registered layer removal must preserve compositor invariants");

    if let Some(area) = removal.recompose_area {
        let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
        let mut output = NvimOutput;
        // SAFETY: The reduced registry has been published, and the caller
        // accesses the process-global message grid on Neovim's main thread.
        unsafe {
            recompose_area_from_registered_grids(&raw mut c_msg_grid, area, runtime, &mut output)
        }
        .expect("removed layer area must be recomposed");
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

    let Some(grid_store) = LegacyGridStore::try_take_registered() else {
        return std::ptr::null_mut();
    };
    let Ok(grid_id) =
        lock_compositor().grid_at(&grid_store, ComposedPosition::new(row, column), hit_test)
    else {
        return std::ptr::null_mut();
    };
    let Some(cgrid) = grid_id.and_then(|grid_id| grid_store.cgrid_by_id(grid_id)) else {
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
        .is_some_and(|grid| grid.contains(ComposedPosition::new(row, column)))
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
    PositionedGrid::try_new(&grid, ComposedPosition::new(origin_row, origin_column)).is_some_and(
        |grid| {
            grid.matches(
                ComposedPosition::new(row, column),
                LayerHitTest::MouseEnabled,
            )
        },
    )
}

/// Checks cursor coverage after acquiring the registered legacy grids.
pub(in crate::ui_compositor) fn cursor_is_covered_by_registered_layers(
    current_grid: CompositorCursorGrid,
    row: i32,
    column: i32,
    minimum_z_index_gap: u32,
) -> CompositorResult<bool> {
    let grid_store =
        LegacyGridStore::try_take_registered().ok_or(CompositorError::InvalidLayerState)?;
    lock_compositor().is_cursor_covered(
        &grid_store,
        current_grid,
        ComposedPosition::new(row, column),
        minimum_z_index_gap,
    )
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
) -> CompositorResult<()> {
    let separator_active = lock_compositor()
        .message_separator_character_at(request.row)
        .is_some();
    if separator_active && message_grid.is_null() {
        return Err(CompositorError::InvalidLayerState);
    }

    let mut line_buffer = acquire_compositor_buffer();
    let submission = (|| -> CompositorResult<CompositorSubmission> {
        let grid_store =
            LegacyGridStore::try_take_registered().ok_or(CompositorError::InvalidLayerState)?;
        let message_grid_id = separator_active
            .then(|| grid_store.grid_id_for_cgrid(message_grid))
            .flatten();
        let separate_message_grid = separator_active && message_grid_id.is_none();

        let message_grid_guard = separate_message_grid.then(|| {
            // SAFETY: The non-null message grid is distinct from every layer
            // pointer and the caller transfers ownership for this operation.
            unsafe { ScreenGridGuard::take(message_grid) }
        });

        let message_grid = if separator_active {
            Some(match message_grid_id {
                Some(grid_id) => grid_store
                    .grid(grid_id)
                    .ok_or(CompositorError::InvalidLayerState)?,
                None => message_grid_guard
                    .as_deref()
                    .ok_or(CompositorError::InvalidLayerState)?,
            })
        } else {
            None
        };

        lock_compositor().compose_line(&mut line_buffer, &grid_store, message_grid, &request)
    })()?;

    let output_range = submission.start_col..submission.end_col;
    let characters = line_buffer
        .characters
        .get(output_range.clone())
        .ok_or(CompositorError::InvalidLineState)?;
    let attributes = line_buffer
        .attributes
        .get(output_range)
        .ok_or(CompositorError::InvalidLineState)?;
    let start_column =
        i32::try_from(submission.start_col).map_err(|_| CompositorError::InvalidGeometry)?;
    let end_column =
        i32::try_from(submission.end_col).map_err(|_| CompositorError::InvalidGeometry)?;
    let command = RawLineCommand::try_new(
        request.row,
        start_column..end_column,
        end_column,
        0,
        submission.flags,
        characters,
        attributes,
    )
    .ok_or(CompositorError::InvalidLineState)?;
    output.emit(UiCommand::RawLine(command));
    Ok(())
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
) -> CompositorResult<()> {
    let plan = {
        let grid_store =
            LegacyGridStore::try_take_registered().ok_or(CompositorError::InvalidLayerState)?;
        lock_compositor().plan_recompose_area(&grid_store, request)?
    };

    for line in plan.into_line_inputs() {
        // SAFETY: The caller satisfies the documented message-grid
        // requirements. The temporary clipping guards and every preceding
        // line's guards were dropped before this helper takes ownership.
        unsafe { compose_line_from_registered_grids(message_grid, line, output) }?;
    }
    Ok(())
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
) -> CompositorResult<()> {
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
) -> CompositorResult<()> {
    let operation = {
        let grid_store =
            LegacyGridStore::try_take_registered().ok_or(CompositorError::InvalidLayerState)?;
        let compositor = lock_compositor();
        let message_layer_index = grid_store
            .grid_id_for_cgrid(message_grid)
            .map(|grid_id| compositor.layer_index(grid_id))
            .transpose()?
            .flatten();
        compositor.process_raw_line(&grid_store, grid_id, message_layer_index, request)?
    };
    let CompositorRawLineOperation::Process(plan) = operation else {
        return Ok(());
    };
    let runtime = compositor_runtime_snapshot().ok_or(CompositorError::InvalidGeometry)?;

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
            Ok(())
        }
        CompositorRawLineAction::DiscardColumns => Ok(()),
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
            let payload_len = plan
                .columns
                .end
                .checked_sub(plan.columns.start)
                .and_then(|length| usize::try_from(length).ok())
                .ok_or(CompositorError::InvalidGeometry)?;
            if payload_len != 0 && (characters.is_null() || attributes.is_null()) {
                return Err(CompositorError::InvalidLineState);
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

            let command = RawLineCommand::try_new(
                plan.row,
                plan.columns,
                plan.clear_column,
                clear_attribute,
                plan.flags,
                characters,
                attributes,
            )
            .ok_or(CompositorError::InvalidLineState)?;
            output.emit(UiCommand::RawLine(command));
            Ok(())
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
    let request = CompositorRawLineRequest::try_new(
        row,
        start_col,
        end_col,
        clear_col,
        LineFlags::from_raw(flags),
    )
    .expect("raw-line columns must be ordered");
    let mut output = NvimOutput;

    // SAFETY: The generated caller supplies the documented cell arrays, and
    // Neovim's message grid is exclusively accessed on the main thread.
    unsafe {
        process_raw_line(
            &raw mut c_msg_grid,
            GridId::new(grid),
            request,
            clear_attr,
            characters,
            attributes,
            &mut output,
        )
    }
    .expect("raw-line processing must preserve compositor invariants");
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
) -> CompositorResult<()> {
    let operation = {
        let grid_store =
            LegacyGridStore::try_take_registered().ok_or(CompositorError::InvalidLayerState)?;
        let compositor = lock_compositor();
        let message_layer_index = grid_store
            .grid_id_for_cgrid(message_grid)
            .map(|grid_id| compositor.layer_index(grid_id))
            .transpose()?
            .flatten();
        compositor.scroll(&grid_store, grid_id, message_layer_index, request)?
    };

    match operation {
        CompositorScrollOperation::Ignore => return Ok(()),
        CompositorScrollOperation::Forward { region, delta } => {
            let runtime = compositor_runtime_snapshot().ok_or(CompositorError::InvalidGeometry)?;
            output.emit(UiCommand::GridScroll { region, delta });
            if runtime.debug_enabled {
                runtime.delay(2, output);
            }
        }
        CompositorScrollOperation::Recompose { region, rows } => {
            let runtime = compositor_runtime_snapshot().ok_or(CompositorError::InvalidGeometry)?;
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
                unsafe {
                    compose_line_from_registered_grids(
                        message_grid,
                        CompositorLineInput {
                            row,
                            columns: region.columns.clone(),
                            flags: LineFlags::default(),
                            settings: runtime.line_settings,
                        },
                        output,
                    )
                }?;
            }
        }
    }
    Ok(())
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
    unsafe { process_scroll(&raw mut c_msg_grid, GridId::new(grid), request, &mut output) }
        .expect("grid-scroll processing must preserve compositor invariants");
}
