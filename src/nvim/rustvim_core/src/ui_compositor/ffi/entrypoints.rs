//! C ABI entry points and raw-pointer orchestration for the compositor.

use std::{ptr::NonNull, slice};

use crate::{
    defines::{CVimString, Integer},
    grid::{
        CGridCell, CScreenGrid, GridId, ScreenGridGuard, VimHandleT, VimSAttrT, VimSCharT,
        default_grid as c_default_grid, lock_grid_store, schar_from_buf,
    },
};

use super::super::*;
use super::{NvimOutput, compositor_runtime_snapshot, initialize_debug_highlights};

unsafe extern "C" {
    static mut msg_grid_handle: VimHandleT;
}

/// C representation of a compositor grid hit.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CCompositorGridHit {
    grid_handle: VimHandleT,
    row: libc::c_int,
    column: libc::c_int,
}

impl From<CompositorGridHit> for CCompositorGridHit {
    fn from(hit: CompositorGridHit) -> Self {
        Self {
            grid_handle: hit.grid_id.as_native_handle(),
            row: hit.local_position.row,
            column: hit.local_position.column,
        }
    }
}

/// Returns the current message-grid identity when one has been assigned.
fn current_message_grid_id() -> Option<GridId> {
    // SAFETY: Compositor events read Neovim globals synchronously on the main
    // thread.
    let grid_handle = unsafe { msg_grid_handle };
    (grid_handle > 0).then_some(GridId::new(grid_handle))
}

/// Moves the default grid into permanent Rust storage.
///
/// # Safety
///
/// `default_grid` must point to a valid, exclusively accessible C grid whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
unsafe fn initialize_default_grid(default_grid: *mut CScreenGrid) -> CompositorResult<GridId> {
    unsafe {
        // SAFETY: The caller transfers exclusive ownership for this scope.
        ScreenGridGuard::take(default_grid)
    }
    .store_permanently()
    .map_err(|_| CompositorError::InvalidLayerState)
}

/// Initializes the compositor with the process-global default grid.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_init() {
    // SAFETY: Initialization runs on Neovim's main thread before compositor
    // operations begin, and the default grid remains process-global.
    let default_grid_id = unsafe { initialize_default_grid(&raw mut c_default_grid) }
        .expect("default grid must move into permanent storage exactly once");

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
fn update_message_position(
    message_grid_id: GridId,
    request: CompositorMessagePositionRequest,
    runtime: CompositorRuntime,
    output: &mut impl UiOutput<ComposedGridSpace>,
) -> CompositorResult<()> {
    let default_grid_id = GridId::new(1);
    if message_grid_id == default_grid_id {
        return Err(CompositorError::InvalidLayerState);
    }

    let plan = {
        let grid_store = lock_grid_store();
        let default_grid = grid_store
            .grid(default_grid_id)
            .ok_or(CompositorError::InvalidLayerState)?;
        lock_compositor().set_message_position(message_grid_id, default_grid, request)
    }?;

    if let Some(scroll) = plan.scroll {
        output.emit(UiCommand::GridScroll {
            region: scroll.region,
            delta: scroll.delta,
        });
    }
    if let Some(area) = plan.recompose {
        recompose_area_from_registered_grids(Some(message_grid_id), area, runtime, output)
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
    grid: Integer,
    row: Integer,
    scrolled: bool,
    separator_character: CVimString,
    z_index: Integer,
    _compositor_index: Integer,
) {
    let (Ok(grid), Ok(row), Ok(z_index)) = (
        i32::try_from(grid),
        i32::try_from(row),
        i32::try_from(z_index),
    ) else {
        panic!("message-grid position must fit native compositor values");
    };
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let separator_character = (!separator_character.data.is_null())
        .then(|| schar_from_buf(separator_character.data, separator_character.size));
    let request = CompositorMessagePositionRequest::new(
        row,
        z_index,
        separator_character,
        runtime.screen_size,
        scrolled,
    );
    let mut output = NvimOutput;

    update_message_position(GridId::new(grid), request, runtime, &mut output)
        .expect("message-grid position update must preserve compositor invariants");
}

/// Returns the screen area occupied by one registered grid.
pub(in crate::ui_compositor) fn recompose_area_for_registered_grid(
    grid_id: GridId,
) -> CompositorResult<CompositorRecomposeArea> {
    let grid_store = lock_grid_store();
    lock_compositor().recompose_area_for_grid(&grid_store, grid_id)
}

/// Recomposes the screen area occupied by one grid when drawing is enabled.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_compose_grid(grid_handle: Integer) {
    let grid_handle =
        i32::try_from(grid_handle).expect("grid handle must fit the compositor grid identity");
    let grid_id = GridId::new(grid_handle);

    if !lock_compositor().should_draw() {
        return;
    }
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
    let area =
        recompose_area_for_registered_grid(grid_id).expect("grid to recompose must be registered");
    let mut output = NvimOutput;

    recompose_area_from_registered_grids(current_message_grid_id(), area, runtime, &mut output)
        .expect("grid recomposition must preserve compositor invariants");
}

/// Places a new or existing grid in the registered compositor layers.
///
/// An existing grid is restored to its requested placement even if recomposing
/// an exposed area fails. The grid-store borrow is released before invoking
/// the callback.
pub(in crate::ui_compositor) fn place_grid_in_registered_layers(
    grid_store: impl GridAccess,
    target: CompositorGridPlacementTarget,
    current_window_layer: Option<usize>,
    request: CompositorGridPlacementRequest,
    mut recompose_area: impl FnMut(CompositorRecomposeArea) -> CompositorResult<()>,
) -> CompositorResult<CompositorGridPlacementOutcome> {
    let preparation = {
        let grid_store = grid_store;
        lock_compositor().prepare_grid_placement(
            &grid_store,
            target,
            current_window_layer,
            request,
        )?
    };

    let outcome = CompositorGridPlacementOutcome::from_moved(preparation.plan.moved);
    let exposed_areas_result = preparation
        .plan
        .exposed_areas
        .iter()
        .cloned()
        .try_for_each(&mut recompose_area);

    if let Some(completion) = preparation.deferred_completion {
        lock_compositor().finish_grid_placement(completion)?;
    }

    exposed_areas_result?;
    if let Some(area) = preparation.plan.redraw_area {
        recompose_area(area)?;
    }

    Ok(outcome)
}

/// Places one validated grid in the registered compositor layer stack.
fn place_registered_grid(
    grid_id: GridId,
    current_window_grid_id: Option<GridId>,
    message_grid_id: Option<GridId>,
    request: CompositorGridPlacementRequest,
    runtime: CompositorRuntime,
) -> bool {
    let grid_store = lock_grid_store();
    if grid_store.grid(grid_id).is_none() {
        return false;
    }
    let (target, current_window_layer) = {
        let compositor = lock_compositor();
        let was_registered = compositor
            .layer_index(grid_id)
            .expect("compositor layers must be initialized")
            .is_some();
        let target = compositor
            .grid_placement_target(grid_id, was_registered)
            .expect("grid identity and compositor layers must agree");
        let current_window_layer = current_window_grid_id
            .map(|grid_id| compositor.layer_index(grid_id))
            .transpose()
            .expect("compositor layers must be initialized")
            .flatten();
        (target, current_window_layer)
    };
    let mut output = NvimOutput;
    let recompose_area = |area| {
        recompose_area_from_registered_grids(message_grid_id, area, runtime, &mut output)
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

/// Places a new or existing regular grid in the compositor layer stack.
///
/// Rust determines whether the identity is already registered and performs
/// either repositioning or insertion. The grid-store borrow is released
/// before every recomposition operation.
///
/// `current_window_grid_handle` may be zero or identify a registered grid.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_put_grid(
    grid_handle: Integer,
    row: libc::c_int,
    column: libc::c_int,
    height: libc::c_int,
    width: libc::c_int,
    z_index: libc::c_int,
    blending: bool,
    valid: bool,
    on_top: bool,
    accepts_mouse_input: bool,
    current_window_grid_handle: Integer,
) -> bool {
    let grid_handle =
        i32::try_from(grid_handle).expect("grid handle must fit the compositor grid identity");
    let current_window_grid_id = match i32::try_from(current_window_grid_handle) {
        Ok(0) => None,
        Ok(handle) => Some(GridId::new(handle)),
        Err(_) => panic!("current-window grid handle must fit GridId"),
    };
    let placement = CompositorGridPlacement::try_new(row, column, height, width)
        .expect("grid placement dimensions must be nonnegative");
    let request = CompositorGridPlacementRequest::new(
        placement,
        z_index,
        blending,
        valid,
        on_top,
        accepts_mouse_input,
    );
    let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");

    place_registered_grid(
        GridId::new(grid_handle),
        current_window_grid_id,
        current_message_grid_id(),
        request,
        runtime,
    )
}

/// Applies one cursor update and publishes any resulting layer movement.
pub(in crate::ui_compositor) fn apply_cursor_update_to_registered_grids(
    request: CompositorCursorRequest,
) -> CompositorResult<CompositorCursorUpdate> {
    let grid_store = lock_grid_store();
    lock_compositor().move_cursor(&grid_store, request)
}

/// Applies one cursor update to the compositor layer stack.
///
/// Any required layer movement is completed before recomposition and cursor
/// output are emitted. Grid-store borrows are released before visualization
/// or UI callbacks.
fn process_grid_cursor_goto(
    message_grid_id: Option<GridId>,
    request: CompositorCursorRequest,
    output: &mut impl UiOutput<ComposedGridSpace>,
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
        recompose_area_from_registered_grids(message_grid_id, area, runtime, output)?;
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

    process_grid_cursor_goto(current_message_grid_id(), request, &mut output)
        .expect("cursor update must preserve compositor invariants");
}

/// Updates a registered grid's compositor policy and layer order.
pub(in crate::ui_compositor) fn update_registered_layer_policy(
    grid_id: GridId,
    z_index: i32,
    blending: bool,
    accepts_mouse_input: bool,
) -> CompositorResult<()> {
    lock_compositor().update_layer_policy(grid_id, z_index, blending, accepts_mouse_input)
}

/// Updates one compositor grid's complete layer policy.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_update_grid_layer(
    grid_handle: Integer,
    z_index: libc::c_int,
    blending: bool,
    accepts_mouse_input: bool,
) {
    let grid_handle =
        i32::try_from(grid_handle).expect("grid handle must fit the compositor grid identity");
    update_registered_layer_policy(
        GridId::new(grid_handle),
        z_index,
        blending,
        accepts_mouse_input,
    )
    .expect("layer policy update must preserve compositor invariants");
}

/// Removes one registered grid from the compositor layer stack.
pub(in crate::ui_compositor) fn remove_registered_layer(
    grid_store: impl GridAccess,
    layer_index: usize,
) -> CompositorResult<CompositorLayerRemoval> {
    lock_compositor().remove_layer(&grid_store, layer_index)
}

/// Removes one grid from the registered layer stack and recomposes its area.
///
/// An unregistered grid is an expected successful no-op.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_remove_grid(grid_handle: Integer) {
    let grid_handle =
        i32::try_from(grid_handle).expect("grid handle must fit the compositor grid identity");
    assert_ne!(grid_handle, 1);
    let grid_id = GridId::new(grid_handle);
    let Some(layer_index) = lock_compositor()
        .layer_index(grid_id)
        .expect("compositor layers must be initialized")
    else {
        return;
    };
    let grid_store = lock_grid_store();
    let removal = remove_registered_layer(grid_store, layer_index)
        .expect("registered layer removal must preserve compositor invariants");

    if let Some(area) = removal.recompose_area {
        let runtime = compositor_runtime_snapshot().expect("compositor runtime must be valid");
        let mut output = NvimOutput;
        recompose_area_from_registered_grids(current_message_grid_id(), area, runtime, &mut output)
            .expect("removed layer area must be recomposed");
    }
}

/// Reads the cell from the topmost non-default compositor grid at one screen
/// coordinate.
///
/// Returns `false` when no overlay contains the coordinate or compositor state
/// is unavailable.
///
/// # Safety
///
/// `output` must point to valid, properly aligned writable `CGridCell` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_get_cell_at_coord(
    row: libc::c_int,
    column: libc::c_int,
    output: *mut CGridCell,
) -> bool {
    let Some(output) = NonNull::new(output) else {
        return false;
    };
    let grid_store = lock_grid_store();
    let Ok(Some(cell)) = lock_compositor().cell_at(&grid_store, ComposedPosition::new(row, column))
    else {
        return false;
    };

    // SAFETY: The output pointer was checked for null above and the caller
    // guarantees valid writable storage.
    unsafe {
        output.as_ptr().write(CGridCell::from(cell));
    }
    true
}

/// Finds the topmost mouse-enabled non-default grid at one screen coordinate.
///
/// Returns `false` when no matching grid exists or registered state is invalid.
///
/// # Safety
///
/// `output` must point to valid, properly aligned writable
/// `CCompositorGridHit` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_find_mouse_grid_at_coord(
    row: libc::c_int,
    column: libc::c_int,
    output: *mut CCompositorGridHit,
) -> bool {
    let Some(output) = NonNull::new(output) else {
        return false;
    };
    let grid_store = lock_grid_store();
    let Ok(Some(hit)) = lock_compositor().grid_at(
        &grid_store,
        ComposedPosition::new(row, column),
        LayerHitTest::MouseEnabled,
    ) else {
        return false;
    };

    // SAFETY: The output pointer was checked for null above and the caller
    // guarantees valid writable storage.
    unsafe {
        output.as_ptr().write(CCompositorGridHit::from(hit));
    }
    true
}

/// Reads one positioned grid's cell at a composed-screen coordinate.
///
/// Returns `false` when the grid does not contain the coordinate or cell
/// storage is unavailable.
///
/// # Safety
///
/// `output` must point to valid, properly aligned writable `CGridCell` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_grid_get_cell_at(
    grid_handle: Integer,
    origin_row: libc::c_int,
    origin_column: libc::c_int,
    row: libc::c_int,
    column: libc::c_int,
    output: *mut CGridCell,
) -> bool {
    let (Ok(grid_handle), Some(output)) = (i32::try_from(grid_handle), NonNull::new(output)) else {
        return false;
    };

    let grid_store = lock_grid_store();
    let Some(grid) = grid_store.grid(GridId::new(grid_handle)) else {
        return false;
    };
    let Some(cell) =
        PositionedGrid::try_new(grid, ComposedPosition::new(origin_row, origin_column))
            .and_then(|grid| grid.cell(ComposedPosition::new(row, column)))
    else {
        return false;
    };

    // SAFETY: The output pointer was checked for null above and the caller
    // guarantees valid writable storage.
    unsafe {
        output.as_ptr().write(CGridCell::from(cell));
    }
    true
}

/// Returns one positioned grid's identity and local coordinates for a
/// composed-screen coordinate.
///
/// Returns `false` when the grid does not contain the coordinate.
///
/// # Safety
///
/// `output` must point to valid, properly aligned writable
/// `CCompositorGridHit` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_grid_hit_at_coord(
    grid_handle: Integer,
    origin_row: libc::c_int,
    origin_column: libc::c_int,
    row: libc::c_int,
    column: libc::c_int,
    output: *mut CCompositorGridHit,
) -> bool {
    let (Ok(grid_handle), Some(output)) = (i32::try_from(grid_handle), NonNull::new(output)) else {
        return false;
    };

    let grid_store = lock_grid_store();
    let Some(grid) = grid_store.grid(GridId::new(grid_handle)) else {
        return false;
    };
    let Some(hit) = PositionedGrid::try_new(grid, ComposedPosition::new(origin_row, origin_column))
        .and_then(|grid| grid.hit(ComposedPosition::new(row, column)))
    else {
        return false;
    };

    // SAFETY: The output pointer was checked for null above and the caller
    // guarantees valid writable storage.
    unsafe {
        output.as_ptr().write(CCompositorGridHit::from(hit));
    }
    true
}

/// Returns the composed-screen origin of one registered grid.
///
/// # Safety
///
/// `row` and `column` must point to writable integers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_grid_origin(
    grid_handle: Integer,
    row: *mut libc::c_int,
    column: *mut libc::c_int,
) -> bool {
    let (Ok(grid_handle), Some(mut row), Some(mut column)) = (
        i32::try_from(grid_handle),
        NonNull::new(row),
        NonNull::new(column),
    ) else {
        return false;
    };
    let Ok(Some(origin)) = lock_compositor().grid_origin(GridId::new(grid_handle)) else {
        return false;
    };

    // SAFETY: Both output pointers were checked for null above. The caller
    // guarantees that they point to writable integers.
    unsafe {
        *row.as_mut() = origin.row;
        *column.as_mut() = origin.column;
    }
    true
}

/// Reports whether one registered grid must publish its current layer index.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_grid_publication_pending(grid_handle: Integer) -> bool {
    let Ok(grid_handle) = i32::try_from(grid_handle) else {
        return false;
    };

    lock_compositor()
        .grid_publication_pending(GridId::new(grid_handle))
        .unwrap_or(false)
}

/// Takes one registered grid's current layer-index publication.
///
/// Returns zero when the grid is not registered. Taking a registered
/// publication clears its pending state.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_take_grid_publication(grid_handle: Integer) -> libc::c_int {
    let Ok(grid_handle) = i32::try_from(grid_handle) else {
        return 0;
    };
    let Ok(Some(publication)) = lock_compositor().take_grid_publication(GridId::new(grid_handle))
    else {
        return 0;
    };

    i32::try_from(publication.layer_index)
        .expect("compositor layer index must fit the C UI event representation")
}

/// Checks cursor coverage while borrowing the permanent grid store.
pub(in crate::ui_compositor) fn cursor_is_covered_by_registered_layers(
    current_grid: CompositorCursorGrid,
    row: i32,
    column: i32,
    minimum_z_index_gap: u32,
) -> CompositorResult<bool> {
    let grid_store = lock_grid_store();
    lock_compositor().is_cursor_covered(
        &grid_store,
        current_grid,
        ComposedPosition::new(row, column),
        minimum_z_index_gap,
    )
}

/// Reports whether a sufficiently higher compositor layer covers a grid's
/// cursor.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_cursor_is_covered_by_higher_layer(
    current_grid_handle: Integer,
    current_grid_z_index: libc::c_int,
    row: libc::c_int,
    column: libc::c_int,
    minimum_z_index_gap: libc::c_int,
) -> bool {
    let Ok(current_grid_handle) = i32::try_from(current_grid_handle) else {
        return false;
    };
    let Ok(minimum_z_index_gap) = u32::try_from(minimum_z_index_gap) else {
        return false;
    };
    let current_grid =
        CompositorCursorGrid::new(GridId::new(current_grid_handle), current_grid_z_index);

    cursor_is_covered_by_registered_layers(current_grid, row, column, minimum_z_index_gap)
        .unwrap_or(false)
}

/// Composes and submits one validated line from registered compositor grids.
fn compose_line_from_registered_grids(
    message_grid_id: Option<GridId>,
    request: CompositorLineInput,
    output: &mut impl UiOutput<ComposedGridSpace>,
) -> CompositorResult<()> {
    let separator_active = lock_compositor()
        .message_separator_character_at(request.row)
        .is_some();
    let mut line_buffer = acquire_compositor_buffer();
    let submission = {
        let grid_store = lock_grid_store();
        let message_grid = if separator_active {
            let message_grid_id = message_grid_id.ok_or(CompositorError::InvalidLayerState)?;
            Some(
                grid_store
                    .grid(message_grid_id)
                    .ok_or(CompositorError::InvalidLayerState)?,
            )
        } else {
            None
        };
        let compositor = lock_compositor();
        compositor.compose_line(&mut line_buffer, &grid_store, message_grid, &request)
    }?;

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
    let command = BorrowedRawLineCommand::try_new(
        request.row,
        start_column..end_column,
        end_column,
        0,
        submission.flags,
        characters,
        attributes,
    )
    .ok_or(CompositorError::InvalidLineState)?;
    output.emit_raw_line(command);
    Ok(())
}

/// Composes and submits a clipped screen area from registered compositor grids.
fn compose_area_from_registered_grids(
    message_grid_id: Option<GridId>,
    request: CompositorAreaRequest,
    output: &mut impl UiOutput<ComposedGridSpace>,
) -> CompositorResult<()> {
    let plan = {
        let grid_store = lock_grid_store();
        lock_compositor().plan_recompose_area(&grid_store, request)?
    };

    for line in plan.into_line_inputs() {
        compose_line_from_registered_grids(message_grid_id, line, output)?;
    }
    Ok(())
}

/// Visualizes and composes one screen area from registered grids.
fn recompose_area_from_registered_grids(
    message_grid_id: Option<GridId>,
    area: CompositorRecomposeArea,
    runtime: CompositorRuntime,
    output: &mut impl UiOutput<ComposedGridSpace>,
) -> CompositorResult<()> {
    runtime.visualize(&area, runtime.debug_highlights.recomposed, true, output);

    compose_area_from_registered_grids(
        message_grid_id,
        CompositorAreaRequest {
            area,
            settings: runtime.line_settings,
        },
        output,
    )
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
/// produces a successful no-op.
unsafe fn process_raw_line(
    message_grid_id: Option<GridId>,
    grid_id: GridId,
    request: CompositorRawLineRequest,
    clear_attribute: VimSAttrT,
    characters: *const VimSCharT,
    attributes: *const VimSAttrT,
    output: &mut impl UiOutput<ComposedGridSpace>,
) -> CompositorResult<()> {
    let operation = {
        let grid_store = lock_grid_store();
        let compositor = lock_compositor();
        let message_layer_index = message_grid_id
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
            compose_line_from_registered_grids(
                message_grid_id,
                CompositorLineInput {
                    row: plan.row,
                    columns: plan.columns.start..plan.clear_column,
                    flags: plan.flags,
                    settings: runtime.line_settings,
                },
                output,
            )
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

            let command = BorrowedRawLineCommand::try_new(
                plan.row,
                plan.columns,
                plan.clear_column,
                clear_attribute,
                plan.flags,
                characters,
                attributes,
            )
            .ok_or(CompositorError::InvalidLineState)?;
            output.emit_raw_line(command);
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

    // SAFETY: The generated caller supplies the documented cell arrays.
    unsafe {
        process_raw_line(
            current_message_grid_id(),
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
/// A disabled compositor or a grid handle absent from the registered layers
/// produces a successful no-op.
fn process_scroll(
    message_grid_id: Option<GridId>,
    grid_id: GridId,
    request: CompositorScrollRequest,
    output: &mut impl UiOutput<ComposedGridSpace>,
) -> CompositorResult<()> {
    let operation = {
        let grid_store = lock_grid_store();
        let compositor = lock_compositor();
        let message_layer_index = message_grid_id
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
                compose_line_from_registered_grids(
                    message_grid_id,
                    CompositorLineInput {
                        row,
                        columns: region.columns.clone(),
                        flags: LineFlags::default(),
                        settings: runtime.line_settings,
                    },
                    output,
                )?;
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

    process_scroll(
        current_message_grid_id(),
        GridId::new(grid),
        request,
        &mut output,
    )
    .expect("grid-scroll processing must preserve compositor invariants");
}
