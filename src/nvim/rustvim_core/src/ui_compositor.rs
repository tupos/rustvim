mod ffi;
mod output;

use std::{
    ops::Range,
    sync::{Mutex, MutexGuard},
};

#[cfg(test)]
use std::ptr::NonNull;

#[cfg(test)]
use ffi::{
    adjust_registered_layer_order, apply_cursor_update_to_registered_grids,
    cursor_is_covered_by_registered_layers,
    grid_store::{LegacyGridStore, lock_compositor_layer_state},
    place_grid_in_registered_layers, remove_registered_layer, ui_comp_find_grid_at_coord,
};
use output::{CompositorOutput, RawLineCommand, UiCommand};

use crate::{
    generated::enums::RdbFlags,
    grid::{
        ComposedPosition, ComposedRegion, GridDelta, GridId, GridPosition, GridRegion, GridRow,
        GridRowSegment, GridSize, Highlight, ScreenGrid, VimSAttrT, VimSCharT, schar_from_ascii,
        schar_from_char,
    },
    ui::LineFlags,
};

#[cfg(test)]
use crate::grid::CScreenGrid;

/// Result of composing one foreground segment over its background.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SegmentComposition {
    followed_by_continuation: bool,
}

impl SegmentComposition {
    /// Reports whether the source segment ends before a wide-cell continuation.
    pub(crate) fn is_followed_by_continuation(self) -> bool {
        self.followed_by_continuation
    }
}

/// Identity and stacking position of the grid whose cursor is being tested.
///
/// The grid need not be registered as a compositor layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorCursorGrid {
    grid_id: GridId,
    z_index: i32,
}

impl From<&ScreenGrid> for CompositorCursorGrid {
    fn from(grid: &ScreenGrid) -> Self {
        Self {
            grid_id: grid.id(),
            z_index: grid.z_index(),
        }
    }
}

/// A Rust-owned grid positioned at an explicit screen origin.
#[derive(Clone, Copy, Debug)]
struct PositionedGrid<'grid> {
    grid: &'grid ScreenGrid,
    origin: ComposedPosition,
}

impl<'grid> PositionedGrid<'grid> {
    /// Constructs a positioned view of a grid with valid dimensions.
    fn try_new(grid: &'grid ScreenGrid, origin: ComposedPosition) -> Option<Self> {
        (grid.rows >= 0 && grid.cols >= 0).then_some(Self { grid, origin })
    }

    /// Positions a grid at its stored compositor origin.
    fn try_from_compositor_position(grid: &'grid ScreenGrid) -> Option<Self> {
        let (row, column) = grid.compositor_position();
        Self::try_new(grid, ComposedPosition::new(row, column))
    }

    /// Reports whether the allocated grid contains one screen cell.
    fn contains(self, point: ComposedPosition) -> bool {
        point.row >= self.origin.row
            && self
                .origin
                .row
                .checked_add(self.grid.rows)
                .is_some_and(|bottom| point.row < bottom)
            && point.column >= self.origin.column
            && self
                .origin
                .column
                .checked_add(self.grid.cols)
                .is_some_and(|right| point.column < right)
    }

    /// Applies one compositor hit-test policy to this grid.
    fn matches(self, point: ComposedPosition, hit_test: LayerHitTest) -> bool {
        self.contains(point) && hit_test.accepts(self)
    }
}

/// An inclusive rectangle in composed-screen coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorRect {
    top: i32,
    bottom: i32,
    left: i32,
    right: i32,
}

impl CompositorRect {
    /// Constructs a rectangle whose edges form a nonempty area.
    fn try_new(top: i32, bottom: i32, left: i32, right: i32) -> Option<Self> {
        (top <= bottom && left <= right).then_some(Self {
            top,
            bottom,
            left,
            right,
        })
    }

    /// Uses a half-open region's ends as inclusive conservative boundaries.
    ///
    /// This preserves the compositor's historical coverage behavior.
    fn from_region(region: &ComposedRegion) -> Self {
        Self {
            top: region.rows.start,
            bottom: region.rows.end,
            left: region.columns.start,
            right: region.columns.end,
        }
    }

    /// Reports whether this rectangle intersects another at any cell.
    fn intersects(self, other: Self) -> bool {
        self.right >= other.left
            && self.left <= other.right
            && self.bottom >= other.top
            && self.top <= other.bottom
    }
}

/// Half-open bounds for recomposing part of the compositor screen.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorRecomposeArea {
    rows: Range<i32>,
    columns: Range<i32>,
}

impl CompositorRecomposeArea {
    /// Returns the composed-screen area occupied by an allocated grid.
    fn try_from_grid(grid: &ScreenGrid) -> Option<Self> {
        if grid.rows < 0 || grid.cols < 0 {
            return None;
        }

        let (row, column) = grid.compositor_position();
        Some(Self {
            rows: row..row.checked_add(grid.rows)?,
            columns: column..column.checked_add(grid.cols)?,
        })
    }

    /// Clamps this area's exclusive ends to an allocated grid's dimensions.
    ///
    /// Start coordinates remain unchanged to preserve the compositor's
    /// established clipping behavior.
    fn with_ends_clamped_to_grid(self, grid: &ScreenGrid) -> CompositorResult<Self> {
        if grid.rows < 0 || grid.cols < 0 {
            return Err(CompositorError::InvalidGeometry);
        }

        Ok(Self {
            rows: self.rows.start..self.rows.end.min(grid.rows),
            columns: self.columns.start..self.columns.end.min(grid.cols),
        })
    }
}

/// Safe state and follow-up work selected for a composed-grid resize.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorResizePlan {
    size: GridSize,
    buffer_width: usize,
}

/// Failures produced by safe compositor operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum CompositorError {
    /// No composed UI is available to detach.
    #[error("cannot detach a composed UI when none is attached")]
    NoAttachedUi,

    /// The screen width cannot be represented by native buffer indices.
    #[error("screen width cannot address native buffer storage")]
    ScreenWidthOutOfRange,

    /// Compositor coordinates or ranges cannot represent the requested work.
    #[error("compositor geometry is invalid")]
    InvalidGeometry,

    /// Compositor line storage and the requested operation disagree.
    #[error("compositor line state is inconsistent")]
    InvalidLineState,

    /// The layer registry and its corresponding grids disagree.
    #[error("compositor layer state is inconsistent")]
    InvalidLayerState,

    /// The immutable default layer was selected for mutation.
    #[error("the default compositor layer cannot be repositioned or removed")]
    DefaultLayerMutation,

    /// A required screen-area recomposition failed.
    #[error("compositor area recomposition failed")]
    RecompositionFailed,
}

pub(crate) type CompositorResult<T> = Result<T, CompositorError>;

/// Validated native inputs for positioning the composed cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorCursorRequest {
    grid_id: GridId,
    local_position: GridPosition,
}

/// Selected layer and composed-screen cursor action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorCursorPlan {
    layer_index: usize,
    screen_position: ComposedPosition,
    raise_to: Option<usize>,
    emit: bool,
}

/// Action selected for one compositor cursor request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompositorCursorAction {
    Ignore,
    Update(CompositorCursorPlan),
}

/// Applied cursor update and any areas exposed by its layer movement.
#[derive(Clone, Debug, Eq, PartialEq)]
enum CompositorCursorUpdate {
    Ignore,
    Apply {
        plan: CompositorCursorPlan,
        recompose_areas: Vec<CompositorRecomposeArea>,
    },
}

/// Requested position and size of one compositor grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorGridPlacement {
    origin: ComposedPosition,
    size: GridSize,
}

/// Validated native inputs for placing one compositor grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorGridPlacementRequest {
    placement: CompositorGridPlacement,
    valid: bool,
    on_top: bool,
}

impl CompositorGridPlacementRequest {
    /// Constructs a placement request with valid dimensions.
    fn try_new(
        row: i32,
        column: i32,
        height: i32,
        width: i32,
        valid: bool,
        on_top: bool,
    ) -> Option<Self> {
        Some(Self {
            placement: CompositorGridPlacement::try_new(row, column, height, width)?,
            valid,
            on_top,
        })
    }
}

impl CompositorGridPlacement {
    /// Constructs a placement with nonnegative dimensions.
    fn try_new(row: i32, column: i32, height: i32, width: i32) -> Option<Self> {
        Some(Self {
            origin: ComposedPosition::new(row, column),
            size: GridSize::try_new(height, width)?,
        })
    }

    /// Applies this placement to an owned Rust grid.
    fn apply_to(self, grid: &mut ScreenGrid) {
        grid.set_compositor_position(self.origin.row, self.origin.column);
        grid.set_compositor_size(self.size.height, self.size.width);
    }

    /// Returns the exclusive bottom edge of the requested area.
    fn bottom_edge(self) -> CompositorResult<i32> {
        self.origin
            .row
            .checked_add(self.size.height)
            .ok_or(CompositorError::InvalidGeometry)
    }

    /// Returns the exclusive right edge of the requested area.
    fn right_edge(self) -> CompositorResult<i32> {
        self.origin
            .column
            .checked_add(self.size.width)
            .ok_or(CompositorError::InvalidGeometry)
    }

    /// Returns the allocated grid area at this placement.
    fn allocated_area(self, grid: &ScreenGrid) -> CompositorResult<CompositorRecomposeArea> {
        if grid.rows < 0 || grid.cols < 0 {
            return Err(CompositorError::InvalidGeometry);
        }

        Ok(CompositorRecomposeArea {
            rows: self.origin.row
                ..self
                    .origin
                    .row
                    .checked_add(grid.rows)
                    .ok_or(CompositorError::InvalidGeometry)?,
            columns: self.origin.column
                ..self
                    .origin
                    .column
                    .checked_add(grid.cols)
                    .ok_or(CompositorError::InvalidGeometry)?,
        })
    }
}

/// Effects of placing one grid in the compositor.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorGridPlacementPlan {
    placement: CompositorGridPlacement,
    moved: bool,
    exposed_areas: Vec<CompositorRecomposeArea>,
    redraw_area: Option<CompositorRecomposeArea>,
}

/// Observable result of placing one compositor grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompositorGridPlacementOutcome {
    /// The grid retained its existing screen origin.
    Unchanged,

    /// The grid moved to a different screen origin.
    Moved,
}

impl CompositorGridPlacementOutcome {
    /// Constructs an outcome from the placement plan's movement state.
    fn from_moved(moved: bool) -> Self {
        if moved { Self::Moved } else { Self::Unchanged }
    }

    /// Reports whether the grid moved to a different screen origin.
    fn did_move(self) -> bool {
        self == Self::Moved
    }
}

/// Placement work prepared while the compositor and grids are borrowed.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorGridPlacementPreparation {
    plan: CompositorGridPlacementPlan,
    deferred_completion: Option<CompositorGridPlacementCompletion>,
}

/// An existing-grid placement deferred until exposed areas are recomposed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorGridPlacementCompletion {
    grid_id: GridId,
    placement: CompositorGridPlacement,
}

/// One placement target temporarily owned with the registered grids.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorGridPlacementTarget {
    grid_id: GridId,
    layer_index: usize,
    was_registered: bool,
}

impl CompositorGridPlacementPlan {
    /// Plans the initial placement of a newly inserted grid.
    fn try_for_new_grid(
        grid: &ScreenGrid,
        placement: CompositorGridPlacement,
        valid: bool,
        should_draw: bool,
    ) -> CompositorResult<Self> {
        let redraw_area = if valid && should_draw {
            Some(placement.allocated_area(grid)?)
        } else {
            None
        };

        Ok(Self {
            placement,
            moved: true,
            exposed_areas: Vec::new(),
            redraw_area,
        })
    }
}

/// Validated native inputs for positioning the message grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorMessagePositionRequest {
    row: i32,
    separator_character: Option<VimSCharT>,
    screen_size: GridSize,
    scrolled: bool,
}

impl CompositorMessagePositionRequest {
    /// Constructs a message-position request from validated native values.
    fn new(
        row: i32,
        separator_character: Option<VimSCharT>,
        screen_size: GridSize,
        scrolled: bool,
    ) -> Self {
        Self {
            row,
            separator_character,
            screen_size,
            scrolled,
        }
    }
}

/// One message-grid scroll emitted directly to the composed UI.
#[derive(Clone, Debug, Eq, PartialEq)]
struct MessageGridScroll {
    region: ComposedRegion,
    delta: GridDelta,
}

/// Effects required after repositioning the message grid.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CompositorMessagePositionPlan {
    scroll: Option<MessageGridScroll>,
    recompose: Option<CompositorRecomposeArea>,
}

/// Tracks whether the compositor should currently emit composed output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorDrawState {
    attached_ui_count: usize,
    screen_valid: bool,
    screen_size: Option<GridSize>,
}

impl CompositorDrawState {
    /// Creates the initial state before any composed UI is attached.
    const fn new() -> Self {
        Self {
            attached_ui_count: 0,
            screen_valid: true,
            screen_size: None,
        }
    }

    /// Records one newly attached composed UI.
    fn register_ui(&mut self) {
        self.attached_ui_count = self
            .attached_ui_count
            .checked_add(1)
            .expect("composed UI count overflow");
    }

    /// Removes one composed UI and reports whether none remain.
    fn unregister_ui(&mut self) -> CompositorResult<bool> {
        self.attached_ui_count = self
            .attached_ui_count
            .checked_sub(1)
            .ok_or(CompositorError::NoAttachedUi)?;
        Ok(self.attached_ui_count == 0)
    }

    /// Reports whether composed output may currently be emitted.
    fn should_draw(self) -> bool {
        self.attached_ui_count != 0 && self.screen_valid
    }

    /// Updates screen validity and returns its previous value.
    fn set_screen_valid(&mut self, valid: bool) -> bool {
        std::mem::replace(&mut self.screen_valid, valid)
    }
}

/// Message-grid placement and separator state owned by the compositor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MessageCompositorState {
    current_row: i32,
    was_scrolled: bool,
    separator_row: Option<i32>,
    separator_character: VimSCharT,
}

impl MessageCompositorState {
    /// Creates the initial state used before the first message-grid event.
    const fn new() -> Self {
        Self {
            current_row: i32::MAX,
            was_scrolled: false,
            separator_row: None,
            separator_character: VimSCharT::from_ne_bytes([b' ', 0, 0, 0]),
        }
    }

    /// Plans the redraw effects without changing the current state.
    fn plan_redraw(
        &self,
        request: CompositorMessagePositionRequest,
        message_grid: &ScreenGrid,
        default_grid: &ScreenGrid,
        should_draw: bool,
    ) -> CompositorResult<CompositorMessagePositionPlan> {
        if default_grid.cols < 0 {
            return Err(CompositorError::InvalidGeometry);
        }

        if !should_draw {
            return Ok(CompositorMessagePositionPlan::default());
        }

        if request.row > self.current_row {
            return Ok(CompositorMessagePositionPlan {
                scroll: None,
                recompose: Some(CompositorRecomposeArea {
                    rows: self.current_row.saturating_sub(1).max(0)..request.row,
                    columns: 0..default_grid.cols,
                }),
            });
        }

        if request.row >= self.current_row
            || !(self.current_row < request.screen_size.height
                || (request.scrolled && !self.was_scrolled))
        {
            return Ok(CompositorMessagePositionPlan::default());
        }

        let row_delta = self
            .current_row
            .checked_sub(request.row)
            .ok_or(CompositorError::InvalidGeometry)?;
        if message_grid.blends_with_background() {
            let separator_offset = if request.scrolled { 1 } else { 0 };
            return Ok(CompositorMessagePositionPlan {
                scroll: None,
                recompose: Some(CompositorRecomposeArea {
                    rows: request.row.saturating_sub(separator_offset).max(0)
                        ..request
                            .screen_size
                            .height
                            .checked_sub(row_delta)
                            .ok_or(CompositorError::InvalidGeometry)?,
                    columns: 0..request.screen_size.width,
                }),
            });
        }

        let previous_separator_offset = if self.was_scrolled { 1 } else { 0 };
        let scroll = MessageGridScroll {
            region: ComposedRegion::try_new(
                request.row.saturating_sub(previous_separator_offset).max(0)
                    ..request.screen_size.height,
                0..request.screen_size.width,
            )
            .ok_or(CompositorError::InvalidGeometry)?,
            delta: GridDelta::new(row_delta, 0),
        };
        let recompose = if request.scrolled && !self.was_scrolled && request.row > 0 {
            Some(CompositorRecomposeArea {
                rows: request
                    .row
                    .checked_sub(1)
                    .ok_or(CompositorError::InvalidGeometry)?..request.row,
                columns: 0..request.screen_size.width,
            })
        } else {
            None
        };

        Ok(CompositorMessagePositionPlan {
            scroll: Some(scroll),
            recompose,
        })
    }

    /// Updates the message grid and records its new compositor state.
    fn update_position(
        &mut self,
        message_grid: &mut ScreenGrid,
        default_grid: &ScreenGrid,
        request: CompositorMessagePositionRequest,
        should_draw: bool,
    ) -> CompositorResult<CompositorMessagePositionPlan> {
        let separator_row = if request.scrolled && request.row > 0 {
            Some(
                request
                    .row
                    .checked_sub(1)
                    .ok_or(CompositorError::InvalidGeometry)?,
            )
        } else {
            None
        };
        let plan = self.plan_redraw(request, message_grid, default_grid, should_draw)?;

        message_grid.mark_compositor_index_for_update();
        let (_, column) = message_grid.compositor_position();
        message_grid.set_compositor_position(request.row, column);

        if separator_row.is_some()
            && let Some(character) = request.separator_character
        {
            self.separator_character = character;
        }
        self.separator_row = separator_row;
        self.current_row = request.row;
        self.was_scrolled = request.scrolled;

        Ok(plan)
    }

    /// Returns the active separator character for `row`.
    fn separator_character_at(self, row: i32) -> Option<VimSCharT> {
        (self.separator_row == Some(row)).then_some(self.separator_character)
    }

    /// Hides the separator while preserving its last configured character.
    fn hide_separator(&mut self) {
        self.separator_row = None;
    }
}

/// Safe process-global state of the UI compositor.
///
/// Reentrant line-buffer storage and transitional C grid bindings remain
/// outside this state because their ownership must be released independently
/// around synchronous UI callbacks.
struct Compositor {
    display: CompositorDrawState,
    message: MessageCompositorState,
}

impl Compositor {
    /// Creates the compositor state used before any composed UI is attached.
    const fn new() -> Self {
        Self {
            display: CompositorDrawState::new(),
            message: MessageCompositorState::new(),
        }
    }

    /// Records one newly attached composed UI.
    fn attach(&mut self) {
        self.display.register_ui();
    }

    /// Removes one composed UI and reports whether none remain.
    fn detach(&mut self) -> CompositorResult<bool> {
        self.display.unregister_ui()
    }

    /// Updates screen validity and returns its previous value.
    ///
    /// Invalidating the composed screen also hides the message separator.
    fn set_screen_valid(&mut self, valid: bool) -> bool {
        let previous = self.display.set_screen_valid(valid);
        if !valid {
            self.message.hide_separator();
        }
        previous
    }

    /// Reports whether composed output may currently be emitted.
    fn should_draw(&self) -> bool {
        self.display.should_draw()
    }

    /// Hides the message separator while preserving its configured character.
    fn hide_message_separator(&mut self) {
        self.message.hide_separator();
    }

    /// Records a composed-screen resize and plans its external effects.
    fn resize(&mut self, size: GridSize) -> CompositorResult<CompositorResizePlan> {
        let buffer_width =
            usize::try_from(size.width).map_err(|_| CompositorError::ScreenWidthOutOfRange)?;
        self.display.screen_size = Some(size);

        Ok(CompositorResizePlan { size, buffer_width })
    }

    /// Updates message-grid placement and returns its required display work.
    fn set_message_position(
        &mut self,
        message_grid: &mut ScreenGrid,
        default_grid: &ScreenGrid,
        request: CompositorMessagePositionRequest,
    ) -> CompositorResult<CompositorMessagePositionPlan> {
        let should_draw = self.should_draw();
        self.message
            .update_position(message_grid, default_grid, request, should_draw)
    }

    /// Prepares a new or existing grid placement without publishing external
    /// state.
    fn prepare_grid_placement(
        &self,
        registry: &mut CompositorLayerRegistry,
        grids: &mut impl GridAccess,
        target: CompositorGridPlacementTarget,
        current_window_layer: Option<usize>,
        request: CompositorGridPlacementRequest,
    ) -> CompositorResult<CompositorGridPlacementPreparation> {
        let should_draw = self.should_draw();
        if target.was_registered {
            if target.layer_index == 0 {
                return Err(CompositorError::DefaultLayerMutation);
            }
            let plan = {
                let layers = registry.layer_stack(grids)?;
                let layer = layers
                    .layer(target.layer_index)
                    .ok_or(CompositorError::InvalidLayerState)?;
                layer.try_plan_placement(request.placement, request.valid, should_draw)?
            };

            let grid = grids
                .grid_mut(target.grid_id)
                .ok_or(CompositorError::InvalidLayerState)?;
            grid.mark_compositor_index_for_update();
            let deferred_completion = if should_draw {
                grid.set_compositor_disabled(true);
                Some(CompositorGridPlacementCompletion {
                    grid_id: target.grid_id,
                    placement: request.placement,
                })
            } else {
                request.placement.apply_to(grid);
                None
            };
            return Ok(CompositorGridPlacementPreparation {
                plan,
                deferred_completion,
            });
        }

        if target
            .layer_index
            .checked_add(1)
            .ok_or(CompositorError::InvalidLayerState)?
            != registry.len()
        {
            return Err(CompositorError::InvalidLayerState);
        }
        let (insertion_index, plan) = {
            let grid = grids
                .grid(target.grid_id)
                .ok_or(CompositorError::InvalidLayerState)?;
            let layers = registry.layer_stack_prefix(grids, target.layer_index)?;
            let insertion_index = layers.plan_layer_insertion(
                grid.z_index(),
                current_window_layer,
                request.on_top,
            )?;
            let plan = CompositorGridPlacementPlan::try_for_new_grid(
                grid,
                request.placement,
                request.valid,
                should_draw,
            )?;
            (insertion_index, plan)
        };

        registry.place_appended_layer(grids, insertion_index, request.placement)?;
        Ok(CompositorGridPlacementPreparation {
            plan,
            deferred_completion: None,
        })
    }

    /// Completes an existing-grid placement after exposed areas were
    /// recomposed.
    fn finish_grid_placement(
        &self,
        grids: &mut impl GridAccess,
        completion: CompositorGridPlacementCompletion,
    ) -> CompositorResult<()> {
        let grid = grids
            .grid_mut(completion.grid_id)
            .ok_or(CompositorError::InvalidLayerState)?;
        grid.set_compositor_disabled(false);
        completion.placement.apply_to(grid);
        Ok(())
    }

    /// Adjusts a grid's layer position after its z-index changes.
    ///
    /// A grid that is not currently registered is accepted as a no-op,
    /// matching the previous C-side `comp_index == 0` check.
    fn adjust_layer_order(
        &self,
        registry: &mut CompositorLayerRegistry,
        grids: &mut impl GridAccess,
        grid_id: GridId,
        adjustment: CompositorLayerAdjustment,
    ) -> CompositorResult<()> {
        let Some(layer_index) = registry.index_of(grid_id) else {
            return Ok(());
        };
        let movement = {
            let layers = registry.layer_stack(grids)?;
            layers.plan_layer_adjustment(layer_index, adjustment)?
        };

        registry.move_layer(grids, movement)
    }

    /// Removes one non-default grid from the compositor layer stack.
    fn remove_layer(
        &self,
        registry: &mut CompositorLayerRegistry,
        grids: &mut impl GridAccess,
        layer_index: usize,
    ) -> CompositorResult<CompositorLayerRemoval> {
        if layer_index == 0 {
            return Err(CompositorError::DefaultLayerMutation);
        }

        let recompose_area = if self.should_draw() {
            let grid_id = registry
                .layer_id(layer_index)
                .ok_or(CompositorError::InvalidLayerState)?;
            let grid = grids
                .grid(grid_id)
                .ok_or(CompositorError::InvalidLayerState)?;
            Some(
                CompositorRecomposeArea::try_from_grid(grid)
                    .ok_or(CompositorError::InvalidLayerState)?,
            )
        } else {
            None
        };

        registry.remove_layer(grids, layer_index)?;

        Ok(CompositorLayerRemoval { recompose_area })
    }

    /// Finds the topmost non-default compositor grid at one screen point.
    fn grid_at(
        &self,
        registry: &CompositorLayerRegistry,
        grids: &impl GridAccess,
        point: ComposedPosition,
        hit_test: LayerHitTest,
    ) -> CompositorResult<Option<GridId>> {
        let layers = registry.layer_stack(grids)?;
        Ok(layers.topmost_grid_at(point, hit_test))
    }

    /// Reports whether a sufficiently higher layer covers a grid's cursor.
    fn is_cursor_covered(
        &self,
        registry: &CompositorLayerRegistry,
        grids: &impl GridAccess,
        current_grid: CompositorCursorGrid,
        point: ComposedPosition,
        minimum_z_index_gap: u32,
    ) -> CompositorResult<bool> {
        if !self.should_draw() {
            return Ok(false);
        }

        let layers = registry.layer_stack(grids)?;
        Ok(layers.is_cursor_covered_by_higher_layer(point, current_grid, minimum_z_index_gap))
    }

    /// Moves the composed cursor and applies any required layer raise.
    fn move_cursor(
        &self,
        registry: &mut CompositorLayerRegistry,
        grids: &mut impl GridAccess,
        request: CompositorCursorRequest,
    ) -> CompositorResult<CompositorCursorUpdate> {
        let action = {
            let layers = registry.layer_stack(grids)?;
            layers.plan_cursor(request)?
        };
        let CompositorCursorAction::Update(plan) = action else {
            return Ok(CompositorCursorUpdate::Ignore);
        };

        let recompose_areas = if let Some(target_index) = plan.raise_to {
            let raise_plan = {
                let layers = registry.layer_stack(grids)?;
                layers.plan_layer_raise(plan.layer_index, target_index)?
            };
            registry.move_layer(grids, raise_plan.movement)?;
            raise_plan.recompose_areas
        } else {
            Vec::new()
        };

        Ok(CompositorCursorUpdate::Apply {
            plan,
            recompose_areas,
        })
    }

    /// Selects the complete safe operation for one raw-line event.
    fn process_raw_line(
        &self,
        registry: &CompositorLayerRegistry,
        grids: &impl GridAccess,
        grid_id: GridId,
        message_layer_index: Option<usize>,
        request: CompositorRawLineRequest,
    ) -> CompositorResult<CompositorRawLineOperation> {
        if !self.should_draw() {
            return Ok(CompositorRawLineOperation::Ignore);
        }

        let layers = registry.layer_stack(grids)?;
        let Some(current_layer_index) = layers.layer_index_with_id(grid_id) else {
            return Ok(CompositorRawLineOperation::Ignore);
        };
        let plan = layers.plan_raw_line(
            current_layer_index,
            message_layer_index,
            self.message.was_scrolled,
            request,
        )?;

        Ok(CompositorRawLineOperation::Process(plan))
    }

    /// Selects and prepares the complete safe operation for one grid scroll.
    fn scroll(
        &self,
        registry: &CompositorLayerRegistry,
        grids: &impl GridAccess,
        grid_id: GridId,
        message_layer_index: Option<usize>,
        request: CompositorScrollRequest,
    ) -> CompositorResult<CompositorScrollOperation> {
        if !self.should_draw() {
            return Ok(CompositorScrollOperation::Ignore);
        }

        let layers = registry.layer_stack(grids)?;
        let Some(current_layer_index) = layers.layer_index_with_id(grid_id) else {
            return Ok(CompositorScrollOperation::Ignore);
        };
        let plan = layers.plan_scroll(
            current_layer_index,
            message_layer_index,
            self.message.was_scrolled,
            request,
        )?;
        let current_grid = layers
            .layer(current_layer_index)
            .ok_or(CompositorError::InvalidLayerState)?
            .grid;

        Ok(plan.prepare(current_grid))
    }

    /// Composes one complete output line from safely borrowed grids.
    fn compose_line(
        &self,
        buffer: &mut CompositorBuffer,
        layers: &CompositorLayerStack<'_>,
        message_grid: Option<&ScreenGrid>,
        request: &CompositorLineInput,
    ) -> CompositorResult<CompositorSubmission> {
        let message_separator = match self.message.separator_character_at(request.row) {
            Some(character) => {
                let message_grid = message_grid.ok_or(CompositorError::InvalidLayerState)?;
                let default_grid_id = layers
                    .default_layer()
                    .ok_or(CompositorError::InvalidLayerState)?
                    .grid
                    .id();
                Some(MessageSeparator {
                    layer: CompositorLayer::try_from_grid(
                        message_grid,
                        message_grid.id() == default_grid_id,
                    )
                    .ok_or(CompositorError::InvalidLayerState)?,
                    character,
                    attribute: request.settings.message_separator_attribute,
                })
            }
            None => None,
        };

        buffer.try_compose_line(
            CompositorLineRequest {
                row: request.row,
                start_column: request.columns.start,
                end_column: request.columns.end,
                flags: request.flags,
                message_separator,
                screen_columns: request.settings.screen_columns,
                abort_on_invalid: request.settings.abort_on_invalid,
            },
            layers,
        )
    }

    /// Clamps a recomposition area's exclusive ends to the default grid.
    fn clip_recompose_area(
        &self,
        registry: &CompositorLayerRegistry,
        grids: &impl GridAccess,
        area: CompositorRecomposeArea,
    ) -> CompositorResult<CompositorRecomposeArea> {
        let default_grid_id = registry
            .layer_id(0)
            .ok_or(CompositorError::InvalidLayerState)?;
        let default_grid = grids
            .grid(default_grid_id)
            .ok_or(CompositorError::InvalidLayerState)?;
        area.with_ends_clamped_to_grid(default_grid)
    }

    /// Returns the screen area occupied by one registered grid.
    fn recompose_area_for_grid(
        &self,
        registry: &CompositorLayerRegistry,
        grids: &impl GridAccess,
        grid_id: GridId,
    ) -> CompositorResult<CompositorRecomposeArea> {
        registry
            .index_of(grid_id)
            .ok_or(CompositorError::InvalidLayerState)?;
        let grid = grids
            .grid(grid_id)
            .ok_or(CompositorError::InvalidLayerState)?;
        CompositorRecomposeArea::try_from_grid(grid).ok_or(CompositorError::InvalidGeometry)
    }

    /// Plans the line requests required to recompose one clipped screen area.
    fn plan_recompose_area(
        &self,
        registry: &CompositorLayerRegistry,
        grids: &impl GridAccess,
        request: CompositorAreaRequest,
    ) -> CompositorResult<CompositorRecomposePlan> {
        Ok(CompositorRecomposePlan {
            area: self.clip_recompose_area(registry, grids, request.area)?,
            settings: request.settings,
        })
    }

    /// Returns the active message-separator character for one screen row.
    fn message_separator_character_at(&self, row: i32) -> Option<VimSCharT> {
        self.message.separator_character_at(row)
    }
}

/// Validated grid-local inputs for planning one compositor scroll.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorScrollRequest {
    region: GridRegion,
    delta: GridDelta,
}

impl CompositorScrollRequest {
    /// Constructs a scroll request with an ordered grid-local region.
    fn try_new(
        top: i32,
        bottom: i32,
        left: i32,
        right: i32,
        row_delta: i32,
        column_delta: i32,
    ) -> Option<Self> {
        Some(Self {
            region: GridRegion::try_new(top..bottom, left..right)?,
            delta: GridDelta::new(row_delta, column_delta),
        })
    }
}

/// Action selected for one validated compositor scroll.
#[derive(Clone, Debug, Eq, PartialEq)]
enum CompositorScrollPlan {
    /// Forward the translated scroll directly to the composed UI.
    Forward {
        region: ComposedRegion,
        delta: GridDelta,
    },

    /// Recompose valid rows because another layer covers the region.
    Recompose {
        region: ComposedRegion,
        screen_rows: Range<i32>,
        first_grid_row: i32,
        grid_start_column: i32,
    },
}

/// A complete scroll action ready to execute after restoring grid ownership.
#[derive(Clone, Debug, Eq, PartialEq)]
enum CompositorScrollOperation {
    /// Ignore the event because composed output is disabled or absent.
    Ignore,

    /// Forward the translated scroll directly to the composed UI.
    Forward {
        region: ComposedRegion,
        delta: GridDelta,
    },

    /// Recompose only rows whose source cells are valid.
    Recompose {
        region: ComposedRegion,
        rows: Vec<i32>,
    },
}

impl CompositorScrollPlan {
    /// Resolves row validity while the selected grid is safely borrowed.
    fn prepare(self, grid: &ScreenGrid) -> CompositorScrollOperation {
        match self {
            Self::Forward { region, delta } => CompositorScrollOperation::Forward { region, delta },
            Self::Recompose {
                region,
                screen_rows,
                first_grid_row,
                grid_start_column,
            } => {
                let start_column = usize::try_from(grid_start_column).ok();
                let rows = screen_rows
                    .enumerate()
                    .filter_map(|(offset, screen_row)| {
                        let grid_row = i32::try_from(offset)
                            .ok()
                            .and_then(|offset| first_grid_row.checked_add(offset))
                            .and_then(|row| usize::try_from(row).ok())?;
                        let row = GridRow::try_from_grid(grid, grid_row)?;
                        row.is_valid_at(start_column?).then_some(screen_row)
                    })
                    .collect();
                CompositorScrollOperation::Recompose { region, rows }
            }
        }
    }
}

/// Validated grid-local inputs for planning one raw-line event.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorRawLineRequest {
    row: i32,
    columns: Range<i32>,
    clear_column: i32,
    flags: LineFlags,
}

impl CompositorRawLineRequest {
    /// Constructs a raw-line request with ordered grid-local columns.
    fn try_new(
        row: i32,
        start_column: i32,
        end_column: i32,
        clear_column: i32,
        flags: LineFlags,
    ) -> Option<Self> {
        (start_column <= end_column && end_column <= clear_column).then_some(Self {
            row,
            columns: start_column..end_column,
            clear_column,
            flags,
        })
    }
}

/// Action selected for a validated raw-line event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompositorRawLineAction {
    DiscardRow,
    DiscardColumns,
    Recompose,
    Forward,
}

/// Diagnostic emitted after validating or clipping a raw-line request.
enum RawLineDiagnostic {
    InvalidRow(i32),
    ClippedColumns(i32),
}

/// Translated and clipped state for one complete raw-line operation.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorRawLinePlan {
    row: i32,
    columns: Range<i32>,
    clear_column: i32,
    requested_clear_column: i32,
    flags: LineFlags,
    action: CompositorRawLineAction,
}

/// Safe raw-line work selected before any external output is emitted.
#[derive(Clone, Debug, Eq, PartialEq)]
enum CompositorRawLineOperation {
    Ignore,
    Process(CompositorRawLinePlan),
}

/// Output range prepared for one raw-line UI event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorSubmission {
    start_col: usize,
    end_col: usize,
    flags: LineFlags,
}

/// Mutable output slices for one compositor span.
///
/// Characters and attributes are kept in parallel slices because that is the
/// representation consumed by Neovim's raw-line UI event.
///
/// TODO: Replace these parallel slices with a unified Rust cell slice once
/// screen-grid ownership has moved completely from C to Rust.
pub(crate) struct CompositorSegment<'output> {
    characters: &'output mut [VimSCharT],
    attributes: &'output mut [VimSAttrT],
}

impl<'output> CompositorSegment<'output> {
    /// Borrows equally sized character and attribute output slices.
    pub(crate) fn try_from_slices(
        characters: &'output mut [VimSCharT],
        attributes: &'output mut [VimSAttrT],
    ) -> Option<Self> {
        (characters.len() == attributes.len()).then_some(Self {
            characters,
            attributes,
        })
    }

    /// Fills this output segment with one repeated cell.
    fn fill(&mut self, character: VimSCharT, attribute: VimSAttrT) {
        self.characters.fill(character);
        self.attributes.fill(attribute);
    }

    /// Copies a foreground grid segment into the compositor output.
    pub(crate) fn try_copy(
        &mut self,
        foreground: GridRowSegment<'_>,
    ) -> CompositorResult<SegmentComposition> {
        if foreground.len() != self.characters.len() {
            return Err(CompositorError::InvalidLineState);
        }

        self.characters.copy_from_slice(foreground.characters());
        self.attributes.copy_from_slice(foreground.attributes());

        Ok(SegmentComposition {
            followed_by_continuation: foreground.is_followed_by_continuation(),
        })
    }

    /// Composes a foreground grid segment over its default-grid background.
    ///
    /// Without blending, this copies the foreground cells directly. With
    /// blending, spaces and Braille blanks may reveal the background, matching
    /// the compositor's existing negative-space behavior.
    pub(crate) fn try_compose(
        &mut self,
        background: GridRowSegment<'_>,
        foreground: GridRowSegment<'_>,
        blending: bool,
    ) -> CompositorResult<SegmentComposition> {
        let composition = self.try_copy(foreground)?;
        if blending {
            self.try_blend_prefix_over(background, self.characters.len())?;
        }
        Ok(composition)
    }

    /// Blends a prefix of this output over a background grid segment.
    ///
    /// The complete output and background segments remain available while the
    /// prefix is blended because a double-width background character beginning
    /// at the end of the prefix also includes its following continuation cell.
    pub(crate) fn try_blend_prefix_over(
        &mut self,
        background: GridRowSegment<'_>,
        blend_len: usize,
    ) -> CompositorResult<()> {
        self.try_blend_prefix_over_with(background, blend_len, |background, foreground, through| {
            let (attribute, through) =
                Highlight::new(background).blend(Highlight::new(foreground), through);
            (attribute.raw(), through)
        })
    }

    /// Blends a prefix using the supplied attribute-blending operation.
    ///
    /// Keeping the cell traversal independent of the highlight implementation
    /// permits deterministic tests of wide-cell and negative-space behavior.
    fn try_blend_prefix_over_with(
        &mut self,
        background: GridRowSegment<'_>,
        blend_len: usize,
        mut blend_attributes: impl FnMut(VimSAttrT, VimSAttrT, bool) -> (VimSAttrT, bool),
    ) -> CompositorResult<()> {
        let len = self.characters.len();
        if background.len() != len || blend_len > len {
            return Err(CompositorError::InvalidLineState);
        }

        let background_characters = background.characters();
        let background_attributes = background.attributes();
        let mut index = 0;

        while index < blend_len {
            let width = if index + 1 < len && background_characters[index + 1] == 0 {
                2
            } else {
                1
            };
            let mut through =
                is_negative_space(self.characters[index]) && background_characters[index] != 0;

            if width == 2 {
                through &= is_negative_space(self.characters[index + 1]);
            }

            let (attribute, blended_through) = blend_attributes(
                background_attributes[index],
                self.attributes[index],
                through,
            );
            self.attributes[index] = attribute;
            through = blended_through;

            if width == 2 {
                let (attribute, blended_through) = blend_attributes(
                    background_attributes[index + 1],
                    self.attributes[index + 1],
                    through,
                );
                self.attributes[index + 1] = attribute;
                through = blended_through;
            }

            if through {
                self.characters[index..index + width]
                    .copy_from_slice(&background_characters[index..index + width]);
            }

            index += width;
        }

        Ok(())
    }
}

/// State used while composing one output line.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorLine {
    columns: Range<usize>,
    skip_start: usize,
    skip_end: usize,
}

/// One Rust-owned grid as positioned in the compositor layer stack.
#[derive(Clone, Copy, Debug)]
struct CompositorLayer<'grid> {
    grid: &'grid ScreenGrid,
    origin: ComposedPosition,
    requested_size: GridSize,
    visible_size: GridSize,
    z_index: i32,
    layer_index: usize,
    blending: bool,
    disabled: bool,
    is_default: bool,
}

impl<'grid> CompositorLayer<'grid> {
    /// Constructs a validated layer view from a Rust screen grid.
    fn try_from_grid(grid: &'grid ScreenGrid, is_default: bool) -> Option<Self> {
        let (origin_row, origin_column) = grid.compositor_position();
        let (requested_width, requested_height) = grid.compositor_size();
        let allocated_size = GridSize::try_new(grid.rows, grid.cols)?;
        let requested_size = GridSize::try_new(requested_height, requested_width)?;
        let visible_size = GridSize {
            height: allocated_size.height.min(requested_size.height),
            width: allocated_size.width.min(requested_size.width),
        };

        Some(Self {
            grid,
            origin: ComposedPosition::new(origin_row, origin_column),
            requested_size,
            visible_size,
            z_index: grid.z_index(),
            layer_index: grid.compositor_index(),
            blending: grid.blends_with_background(),
            disabled: grid.is_compositor_disabled(),
            is_default,
        })
    }

    /// Returns the allocated dimensions of this layer's grid.
    fn allocated_size(self) -> GridSize {
        GridSize {
            height: self.grid.rows,
            width: self.grid.cols,
        }
    }

    /// Returns the exclusive right edge of this layer.
    fn right_edge(self) -> CompositorResult<i32> {
        self.origin
            .column
            .checked_add(self.visible_size.width)
            .ok_or(CompositorError::InvalidGeometry)
    }

    /// Returns the requested compositor area, optionally including the row
    /// occupied by a scrolled message separator.
    fn coverage_rect(self, include_row_above: bool) -> CompositorResult<Option<CompositorRect>> {
        let top = self
            .origin
            .row
            .checked_sub(i32::from(include_row_above))
            .ok_or(CompositorError::InvalidGeometry)?;
        let bottom = self
            .origin
            .row
            .checked_add(self.requested_size.height)
            .and_then(|bottom| bottom.checked_sub(1))
            .ok_or(CompositorError::InvalidGeometry)?;
        let right = self
            .origin
            .column
            .checked_add(self.requested_size.width)
            .and_then(|right| right.checked_sub(1))
            .ok_or(CompositorError::InvalidGeometry)?;
        Ok(CompositorRect::try_new(
            top,
            bottom,
            self.origin.column,
            right,
        ))
    }

    /// Computes the existing compositor recomposition bounds for two layers.
    ///
    /// Empty or inverted bounds are retained because the C area renderer also
    /// receives them and applies its own clipping and debug behavior.
    fn recomposition_area_with(self, other: Self) -> CompositorResult<CompositorRecomposeArea> {
        let bottom = self
            .origin
            .row
            .checked_add(self.grid.rows)
            .ok_or(CompositorError::InvalidGeometry)?
            .min(
                other
                    .origin
                    .row
                    .checked_add(other.grid.rows)
                    .ok_or(CompositorError::InvalidGeometry)?,
            );
        let right = self
            .origin
            .column
            .checked_add(self.grid.cols)
            .ok_or(CompositorError::InvalidGeometry)?
            .min(
                other
                    .origin
                    .column
                    .checked_add(other.grid.cols)
                    .ok_or(CompositorError::InvalidGeometry)?,
            );

        Ok(CompositorRecomposeArea {
            rows: self.origin.row.max(other.origin.row)..bottom,
            columns: self.origin.column.max(other.origin.column)..right,
        })
    }

    /// Plans repositioning and resizing this existing compositor layer.
    fn try_plan_placement(
        self,
        placement: CompositorGridPlacement,
        valid: bool,
        should_draw: bool,
    ) -> CompositorResult<CompositorGridPlacementPlan> {
        let moved = placement.origin != self.origin;
        let mut exposed_areas = Vec::new();

        if should_draw {
            let old_bottom = self
                .origin
                .row
                .checked_add(self.requested_size.height)
                .ok_or(CompositorError::InvalidGeometry)?;
            let old_right = self
                .origin
                .column
                .checked_add(self.requested_size.width)
                .ok_or(CompositorError::InvalidGeometry)?;
            let new_bottom = placement.bottom_edge()?;
            let new_right = placement.right_edge()?;

            exposed_areas.push(CompositorRecomposeArea {
                rows: self.origin.row..placement.origin.row,
                columns: self.origin.column..old_right,
            });

            if self.origin.column < placement.origin.column {
                exposed_areas.push(CompositorRecomposeArea {
                    rows: placement.origin.row.max(self.origin.row)..new_bottom.min(old_bottom),
                    columns: self.origin.column..placement.origin.column,
                });
            }

            if new_right < old_right {
                exposed_areas.push(CompositorRecomposeArea {
                    rows: placement.origin.row.max(self.origin.row)..new_bottom.min(old_bottom),
                    columns: new_right..old_right,
                });
            }

            exposed_areas.push(CompositorRecomposeArea {
                rows: new_bottom..old_bottom,
                columns: self.origin.column..old_right,
            });
        }

        let redraw_area = if moved && valid && should_draw {
            Some(placement.allocated_area(self.grid)?)
        } else {
            None
        };

        Ok(CompositorGridPlacementPlan {
            placement,
            moved,
            exposed_areas,
            redraw_area,
        })
    }

    /// Reports whether this layer participates in selection on `row`.
    fn is_visible_on_row(self, row: i32) -> bool {
        !self.disabled
            && row >= self.origin.row
            && self
                .origin
                .row
                .checked_add(self.visible_size.height)
                .is_some_and(|bottom| row < bottom)
    }

    /// Reports whether this layer covers one composed-screen cell.
    fn covers(self, row: i32, column: i32) -> bool {
        self.is_visible_on_row(row)
            && column >= self.origin.column
            && self.right_edge().is_ok_and(|right| column < right)
    }

    /// Returns this layer as a positioned allocated grid.
    fn positioned_grid(self) -> PositionedGrid<'grid> {
        PositionedGrid {
            grid: self.grid,
            origin: self.origin,
        }
    }

    /// Borrows the grid cells corresponding to composed-screen columns.
    fn screen_segment(
        self,
        row: i32,
        columns: Range<usize>,
    ) -> CompositorResult<GridRowSegment<'grid>> {
        let start_column =
            i32::try_from(columns.start).map_err(|_| CompositorError::InvalidGeometry)?;
        let end_column =
            i32::try_from(columns.end).map_err(|_| CompositorError::InvalidGeometry)?;
        let grid_row = row
            .checked_sub(self.origin.row)
            .and_then(|row| usize::try_from(row).ok())
            .ok_or(CompositorError::InvalidGeometry)?;
        let grid_start = start_column
            .checked_sub(self.origin.column)
            .and_then(|column| usize::try_from(column).ok())
            .ok_or(CompositorError::InvalidGeometry)?;
        let grid_end = end_column
            .checked_sub(self.origin.column)
            .and_then(|column| usize::try_from(column).ok())
            .ok_or(CompositorError::InvalidGeometry)?;

        GridRow::try_from_grid(self.grid, grid_row)
            .and_then(|row| row.segment(grid_start..grid_end))
            .ok_or(CompositorError::InvalidLineState)
    }

    /// Reports whether a line ending on this layer may retain its wrap flag.
    fn allows_wrap(self, screen_columns: usize) -> bool {
        self.is_default
            || (self.origin.column == 0
                && usize::try_from(self.grid.cols).ok() == Some(screen_columns))
    }
}

/// One contiguous output range selected from the layer stack.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LayerSelection {
    layer_index: usize,
    columns: Range<usize>,
}

/// Selects which compositor layers participate in coordinate hit testing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LayerHitTest {
    Any,
    MouseEnabled,
}

impl LayerHitTest {
    /// Reports whether one positioned grid participates in this hit test.
    fn accepts(self, grid: PositionedGrid<'_>) -> bool {
        match self {
            Self::Any => true,
            Self::MouseEnabled => grid.grid.accepts_mouse_input(),
        }
    }
}

/// Provides safe grid borrowing by stable Rust grid identity.
///
/// Implementations may obtain their storage from C-owned grids, but those
/// ownership details must remain outside compositor algorithms.
trait GridAccess {
    /// Borrows one grid by identity.
    fn grid(&self, grid_id: GridId) -> Option<&ScreenGrid>;

    /// Mutably borrows one grid by identity.
    fn grid_mut(&mut self, grid_id: GridId) -> Option<&mut ScreenGrid>;
}

/// Direction in which a layer's changed z-index may move it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompositorLayerAdjustment {
    Raise,
    Lower,
}

/// One stable relocation within the compositor layer stack.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorLayerMove {
    source_index: usize,
    target_index: usize,
}

/// Rust-owned identities and ordering of compositor layers.
///
/// C grid pointers are deliberately excluded. During the ownership transition,
/// a `GridAccess` implementation supplies the corresponding Rust grid borrows.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorLayerRegistry {
    layer_ids: Vec<GridId>,
}

impl CompositorLayerRegistry {
    /// Creates a nonempty registry containing unique grid identities.
    fn try_new(layer_ids: Vec<GridId>) -> Option<Self> {
        if layer_ids.is_empty()
            || layer_ids
                .iter()
                .enumerate()
                .any(|(index, grid_id)| layer_ids[..index].contains(grid_id))
        {
            return None;
        }

        Some(Self { layer_ids })
    }

    /// Returns the number of registered compositor layers.
    fn len(&self) -> usize {
        self.layer_ids.len()
    }

    /// Returns the ordered registered identities.
    fn layer_ids(&self) -> &[GridId] {
        &self.layer_ids
    }

    /// Returns one identity by its layer index.
    fn layer_id(&self, index: usize) -> Option<GridId> {
        self.layer_ids.get(index).copied()
    }

    /// Returns the layer index of one registered grid.
    fn index_of(&self, grid_id: GridId) -> Option<usize> {
        self.layer_ids
            .iter()
            .position(|candidate| *candidate == grid_id)
    }

    /// Appends one previously unregistered grid identity.
    fn try_append(&mut self, grid_id: GridId) -> CompositorResult<()> {
        if self.index_of(grid_id).is_some() {
            return Err(CompositorError::InvalidLayerState);
        }

        self.layer_ids.push(grid_id);
        Ok(())
    }

    /// Removes one non-default grid identity.
    fn remove_non_default(&mut self, layer_index: usize) -> CompositorResult<GridId> {
        if layer_index == 0 {
            return Err(CompositorError::DefaultLayerMutation);
        }
        if layer_index >= self.layer_ids.len() {
            return Err(CompositorError::InvalidLayerState);
        }

        Ok(self.layer_ids.remove(layer_index))
    }

    /// Applies one validated layer relocation.
    fn apply_layer_move(&mut self, movement: CompositorLayerMove) -> CompositorResult<()> {
        movement.apply_to(&mut self.layer_ids)
    }

    /// Clones the first `layer_count` layer identities into a new registry.
    fn prefix(&self, layer_count: usize) -> CompositorResult<Self> {
        let layer_ids = self
            .layer_ids
            .get(..layer_count)
            .ok_or(CompositorError::InvalidLayerState)?;
        Self::try_new(layer_ids.to_vec()).ok_or(CompositorError::InvalidLayerState)
    }

    /// Borrows every registered grid as a validated compositor layer stack.
    fn layer_stack<'grid>(
        &self,
        grids: &'grid impl GridAccess,
    ) -> CompositorResult<CompositorLayerStack<'grid>> {
        self.layer_stack_prefix(grids, self.len())
    }

    /// Borrows an initial registry prefix as a compositor layer stack.
    fn layer_stack_prefix<'grid>(
        &self,
        grids: &'grid impl GridAccess,
        layer_count: usize,
    ) -> CompositorResult<CompositorLayerStack<'grid>> {
        let grids = self
            .layer_ids
            .get(..layer_count)
            .ok_or(CompositorError::InvalidLayerState)?
            .iter()
            .map(|grid_id| {
                grids
                    .grid(*grid_id)
                    .ok_or(CompositorError::InvalidLayerState)
            })
            .collect::<CompositorResult<Vec<_>>>()?;
        CompositorLayerStack::try_from_grids(&grids).ok_or(CompositorError::InvalidLayerState)
    }

    /// Places the appended grid at its planned compositor-stack index.
    fn place_appended_layer(
        &mut self,
        grids: &mut impl GridAccess,
        insertion_index: usize,
        placement: CompositorGridPlacement,
    ) -> CompositorResult<()> {
        let appended_index = self
            .len()
            .checked_sub(1)
            .ok_or(CompositorError::InvalidLayerState)?;
        if insertion_index == 0 || insertion_index > appended_index {
            return Err(CompositorError::InvalidLayerState);
        }

        self.move_layer(
            grids,
            CompositorLayerMove {
                source_index: appended_index,
                target_index: insertion_index,
            },
        )?;
        let grid_id = self
            .layer_id(insertion_index)
            .ok_or(CompositorError::InvalidLayerState)?;
        let grid = grids
            .grid_mut(grid_id)
            .ok_or(CompositorError::InvalidLayerState)?;
        placement.apply_to(grid);
        Ok(())
    }

    /// Applies a relocation and synchronizes affected grid metadata.
    fn move_layer(
        &mut self,
        grids: &mut impl GridAccess,
        movement: CompositorLayerMove,
    ) -> CompositorResult<()> {
        self.apply_layer_move(movement)?;

        let affected_indices = movement.affected_indices();
        let first_index = *affected_indices.start();
        for (offset, index) in affected_indices.enumerate() {
            let grid_id = self
                .layer_id(index)
                .ok_or(CompositorError::InvalidLayerState)?;
            let grid = grids
                .grid_mut(grid_id)
                .ok_or(CompositorError::InvalidLayerState)?;
            grid.set_compositor_index(first_index + offset);
        }
        Ok(())
    }

    /// Removes one non-default layer and synchronizes shifted grid indices.
    fn remove_layer(
        &mut self,
        grids: &mut impl GridAccess,
        layer_index: usize,
    ) -> CompositorResult<()> {
        let removed_grid_id = self.remove_non_default(layer_index)?;
        grids
            .grid_mut(removed_grid_id)
            .ok_or(CompositorError::InvalidLayerState)?
            .set_compositor_index(0);

        for new_index in layer_index..self.len() {
            let grid_id = self
                .layer_id(new_index)
                .ok_or(CompositorError::InvalidLayerState)?;
            let grid = grids
                .grid_mut(grid_id)
                .ok_or(CompositorError::InvalidLayerState)?;
            grid.set_compositor_index(new_index);
        }
        Ok(())
    }
}

/// Effects of removing one grid from the active compositor layers.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorLayerRemoval {
    recompose_area: Option<CompositorRecomposeArea>,
}

/// One explicit raise and the areas exposed by crossing lower layers.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorLayerRaisePlan {
    movement: CompositorLayerMove,
    recompose_areas: Vec<CompositorRecomposeArea>,
}

impl CompositorLayerMove {
    /// Applies this relocation to any layer representation.
    fn apply_to<T>(self, layers: &mut [T]) -> CompositorResult<()> {
        if self.source_index < self.target_index {
            layers
                .get_mut(self.source_index..=self.target_index)
                .ok_or(CompositorError::InvalidLayerState)?
                .rotate_left(1);
        } else if self.source_index > self.target_index {
            layers
                .get_mut(self.target_index..=self.source_index)
                .ok_or(CompositorError::InvalidLayerState)?
                .rotate_right(1);
        } else {
            layers
                .get(self.source_index)
                .ok_or(CompositorError::InvalidLayerState)?;
        }

        Ok(())
    }

    /// Returns the inclusive range whose compositor indices changed.
    fn affected_indices(self) -> std::ops::RangeInclusive<usize> {
        self.source_index.min(self.target_index)..=self.source_index.max(self.target_index)
    }
}

/// Ordered borrowed grids used to compose one line.
struct CompositorLayerStack<'grid> {
    layers: Vec<CompositorLayer<'grid>>,
}

/// Validated inputs for checking coverage above one compositor layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorCoveredAboveQuery {
    rectangle: CompositorRect,
    current_layer_index: usize,
    message_layer_index: Option<usize>,
    message_was_scrolled: bool,
}

impl<'grid> CompositorLayerStack<'grid> {
    /// Builds a layer stack from grids already owned by Rust guards.
    fn try_from_grids(grids: &[&'grid ScreenGrid]) -> Option<Self> {
        let layers = grids
            .iter()
            .enumerate()
            .map(|(index, grid)| CompositorLayer::try_from_grid(grid, index == 0))
            .collect::<Option<Vec<_>>>()?;

        if layers
            .iter()
            .enumerate()
            .any(|(index, layer)| layer.layer_index != index)
        {
            return None;
        }

        Some(Self { layers })
    }

    /// Returns the default grid at the bottom of the stack.
    fn default_layer(&self) -> Option<CompositorLayer<'grid>> {
        self.layers.first().copied()
    }

    /// Returns one layer by its stack index.
    fn layer(&self, index: usize) -> Option<CompositorLayer<'grid>> {
        self.layers.get(index).copied()
    }

    /// Selects the stable insertion index for one new layer.
    fn plan_layer_insertion(
        &self,
        z_index: i32,
        current_window_layer: Option<usize>,
        on_top: bool,
    ) -> CompositorResult<usize> {
        let mut insertion_index = self.layers.len();
        while insertion_index > 0
            && self
                .layers
                .get(insertion_index - 1)
                .ok_or(CompositorError::InvalidLayerState)?
                .z_index
                > z_index
        {
            insertion_index -= 1;
        }

        // The default grid must remain the first layer.
        if insertion_index == 0 {
            return Err(CompositorError::InvalidLayerState);
        }

        if !on_top && insertion_index > 1 {
            let preceding_index = insertion_index - 1;
            let preceding_layer = self
                .layer(preceding_index)
                .ok_or(CompositorError::InvalidLayerState)?;
            if current_window_layer == Some(preceding_index) && preceding_layer.z_index == z_index {
                insertion_index = preceding_index;
            }
        }

        Ok(insertion_index)
    }

    /// Finds the compositor layer identified by one native grid identity.
    fn layer_index_with_id(&self, grid_id: GridId) -> Option<usize> {
        self.layers
            .iter()
            .position(|layer| layer.grid.id() == grid_id)
    }

    /// Selects a grid and plans its composed-screen cursor update.
    fn plan_cursor(
        &self,
        request: CompositorCursorRequest,
    ) -> CompositorResult<CompositorCursorAction> {
        let screen_size = self
            .default_layer()
            .ok_or(CompositorError::InvalidLayerState)?
            .allocated_size();
        let Some(layer_index) = self.layer_index_with_id(request.grid_id) else {
            return Ok(CompositorCursorAction::Ignore);
        };

        let layer = self
            .layer(layer_index)
            .ok_or(CompositorError::InvalidLayerState)?;
        let screen_position = layer
            .origin
            .checked_add_local(request.local_position)
            .ok_or(CompositorError::InvalidGeometry)?;

        let raise_to = if layer.is_default {
            None
        } else {
            let mut target_index = self
                .layers
                .len()
                .checked_sub(1)
                .ok_or(CompositorError::InvalidLayerState)?;
            while target_index > 1
                && self
                    .layer(target_index)
                    .ok_or(CompositorError::InvalidLayerState)?
                    .z_index
                    > layer.z_index
            {
                target_index -= 1;
            }
            (layer_index < target_index).then_some(target_index)
        };
        let emit =
            screen_position.column < screen_size.width && screen_position.row < screen_size.height;

        Ok(CompositorCursorAction::Update(CompositorCursorPlan {
            layer_index,
            screen_position,
            raise_to,
            emit,
        }))
    }

    /// Finds the topmost non-default grid containing one screen cell.
    fn topmost_grid_at(&self, point: ComposedPosition, hit_test: LayerHitTest) -> Option<GridId> {
        self.layers.get(1..)?.iter().rev().find_map(|layer| {
            let layer = *layer;
            layer
                .positioned_grid()
                .matches(point, hit_test)
                .then_some(layer.grid.id())
        })
    }

    /// Reports whether a sufficiently higher layer covers `current_grid`'s cursor.
    fn is_cursor_covered_by_higher_layer(
        &self,
        point: ComposedPosition,
        current_grid: CompositorCursorGrid,
        minimum_z_index_gap: u32,
    ) -> bool {
        self.topmost_grid_at(point, LayerHitTest::Any)
            .and_then(|grid_id| self.layer_index_with_id(grid_id))
            .and_then(|index| self.layer(index))
            .is_some_and(|top_layer| {
                top_layer.grid.id() != current_grid.grid_id
                    && top_layer
                        .grid
                        .is_at_least_z_index_above(current_grid.z_index, minimum_z_index_gap)
            })
    }

    /// Plans a stable relocation after one layer's z-index changes.
    fn plan_layer_adjustment(
        &self,
        layer_index: usize,
        adjustment: CompositorLayerAdjustment,
    ) -> CompositorResult<CompositorLayerMove> {
        let layer = self
            .layer(layer_index)
            .ok_or(CompositorError::InvalidLayerState)?;
        if layer.is_default {
            return Err(CompositorError::DefaultLayerMutation);
        }

        let mut target_index = layer_index;
        match adjustment {
            CompositorLayerAdjustment::Raise => {
                while target_index + 1 < self.layers.len()
                    && layer.z_index > self.layers[target_index + 1].z_index
                {
                    target_index += 1;
                }
            }
            CompositorLayerAdjustment::Lower => {
                while target_index > 1 && layer.z_index < self.layers[target_index - 1].z_index {
                    target_index -= 1;
                }
            }
        }

        Ok(CompositorLayerMove {
            source_index: layer_index,
            target_index,
        })
    }

    /// Plans an explicit upward move and recomposition of crossed layers.
    fn plan_layer_raise(
        &self,
        layer_index: usize,
        target_index: usize,
    ) -> CompositorResult<CompositorLayerRaisePlan> {
        let layer = self
            .layer(layer_index)
            .ok_or(CompositorError::InvalidLayerState)?;
        if layer.is_default {
            return Err(CompositorError::DefaultLayerMutation);
        }
        if target_index < layer_index || target_index >= self.layers.len() {
            return Err(CompositorError::InvalidLayerState);
        }

        let recompose_areas = if target_index == layer_index {
            Vec::new()
        } else {
            self.layers
                .get(layer_index + 1..=target_index)
                .ok_or(CompositorError::InvalidLayerState)?
                .iter()
                .copied()
                .map(|crossed_layer| layer.recomposition_area_with(crossed_layer))
                .collect::<CompositorResult<Vec<_>>>()?
        };

        Ok(CompositorLayerRaisePlan {
            movement: CompositorLayerMove {
                source_index: layer_index,
                target_index,
            },
            recompose_areas,
        })
    }

    /// Reports whether any layer above the current layer intersects the query.
    fn is_covered_above(&self, query: CompositorCoveredAboveQuery) -> CompositorResult<bool> {
        let first_layer_above = query
            .current_layer_index
            .checked_add(1)
            .ok_or(CompositorError::InvalidLayerState)?;
        let layers_above = self
            .layers
            .get(first_layer_above..)
            .ok_or(CompositorError::InvalidLayerState)?;

        for layer in layers_above {
            let include_row_above =
                query.message_was_scrolled && query.message_layer_index == Some(layer.layer_index);
            if layer
                .coverage_rect(include_row_above)?
                .is_some_and(|rectangle| rectangle.intersects(query.rectangle))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Plans a translated scroll for one layer in this stack.
    fn plan_scroll(
        &self,
        current_layer_index: usize,
        message_layer_index: Option<usize>,
        message_was_scrolled: bool,
        request: CompositorScrollRequest,
    ) -> CompositorResult<CompositorScrollPlan> {
        let current_layer = self
            .layer(current_layer_index)
            .ok_or(CompositorError::InvalidLayerState)?;
        let row_delta = request.delta.rows;
        let region = request
            .region
            .checked_translate(current_layer.origin)
            .ok_or(CompositorError::InvalidGeometry)?;
        let covered = self.is_covered_above(CompositorCoveredAboveQuery {
            rectangle: CompositorRect::from_region(&region),
            current_layer_index,
            message_layer_index,
            message_was_scrolled,
        })?;

        if !covered && !current_layer.blending {
            return Ok(CompositorScrollPlan::Forward {
                region,
                delta: request.delta,
            });
        }

        // TODO(bfredl): calculate non-overlapping subareas that can still be
        // forwarded instead of recomposing the complete covered rectangle.
        let skipped_top = row_delta
            .checked_neg()
            .ok_or(CompositorError::InvalidGeometry)?
            .max(0);
        let skipped_bottom = row_delta.max(0);
        let first_recompose_row = region
            .rows
            .start
            .checked_add(skipped_top)
            .ok_or(CompositorError::InvalidGeometry)?;
        let recompose_end_row = region
            .rows
            .end
            .checked_sub(skipped_bottom)
            .ok_or(CompositorError::InvalidGeometry)?;
        let first_grid_row = first_recompose_row
            .checked_sub(current_layer.origin.row)
            .ok_or(CompositorError::InvalidGeometry)?;
        let grid_start_column = region
            .columns
            .start
            .checked_sub(current_layer.origin.column)
            .ok_or(CompositorError::InvalidGeometry)?;

        Ok(CompositorScrollPlan::Recompose {
            region,
            screen_rows: first_recompose_row..recompose_end_row,
            first_grid_row,
            grid_start_column,
        })
    }

    /// Plans translation, clipping, and composition for one raw-line event.
    fn plan_raw_line(
        &self,
        current_layer_index: usize,
        message_layer_index: Option<usize>,
        message_was_scrolled: bool,
        request: CompositorRawLineRequest,
    ) -> CompositorResult<CompositorRawLinePlan> {
        let current_layer = self
            .layer(current_layer_index)
            .ok_or(CompositorError::InvalidLayerState)?;
        let default_layer = self
            .default_layer()
            .ok_or(CompositorError::InvalidLayerState)?;
        let row = request
            .row
            .checked_add(current_layer.origin.row)
            .ok_or(CompositorError::InvalidGeometry)?;
        let start_column = request
            .columns
            .start
            .checked_add(current_layer.origin.column)
            .ok_or(CompositorError::InvalidGeometry)?;
        let end_column = request
            .columns
            .end
            .checked_add(current_layer.origin.column)
            .ok_or(CompositorError::InvalidGeometry)?;
        let requested_clear_column = request
            .clear_column
            .checked_add(current_layer.origin.column)
            .ok_or(CompositorError::InvalidGeometry)?;
        let flags = if current_layer.is_default {
            request.flags
        } else {
            request.flags.without(LineFlags::WRAP)
        };
        let mut plan = CompositorRawLinePlan {
            row,
            columns: start_column..end_column,
            clear_column: requested_clear_column,
            requested_clear_column,
            flags,
            action: CompositorRawLineAction::Forward,
        };

        if row >= default_layer.grid.rows {
            plan.action = CompositorRawLineAction::DiscardRow;
            return Ok(plan);
        }

        let screen_columns = default_layer.grid.cols;
        if plan.clear_column > screen_columns {
            plan.clear_column = screen_columns;
            if plan.columns.start >= screen_columns {
                plan.action = CompositorRawLineAction::DiscardColumns;
                return Ok(plan);
            }
            plan.columns.end = plan.columns.end.min(screen_columns);
        }

        let covered = self.is_covered_above(CompositorCoveredAboveQuery {
            rectangle: CompositorRect::try_new(
                plan.row,
                plan.row,
                plan.columns.start,
                plan.clear_column,
            )
            .ok_or(CompositorError::InvalidGeometry)?,
            current_layer_index,
            message_layer_index,
            message_was_scrolled,
        })?;
        if plan.flags.contains(LineFlags::INVALID) || covered || current_layer.blending {
            plan.action = CompositorRawLineAction::Recompose;
        }

        Ok(plan)
    }

    /// Selects the topmost layer and its uninterrupted output range.
    fn select_segment(
        &self,
        row: i32,
        start_column: usize,
        end_column: usize,
    ) -> CompositorResult<LayerSelection> {
        let column = i32::try_from(start_column).map_err(|_| CompositorError::InvalidGeometry)?;
        let line_end = i32::try_from(end_column).map_err(|_| CompositorError::InvalidGeometry)?;
        let mut selected = None;
        let mut segment_end = line_end;

        for (index, layer) in self.layers.iter().copied().enumerate() {
            if !layer.is_visible_on_row(row) {
                continue;
            }
            if layer.covers(row, column) {
                selected = Some(index);
                segment_end = layer.right_edge()?;
            } else if layer.origin.column > column {
                segment_end = segment_end.min(layer.origin.column);
            }
        }

        let layer_index = selected.ok_or(CompositorError::InvalidLayerState)?;
        let segment_end = usize::try_from(segment_end.min(line_end))
            .map_err(|_| CompositorError::InvalidGeometry)?;
        if start_column >= segment_end {
            return Err(CompositorError::InvalidLineState);
        }
        Ok(LayerSelection {
            layer_index,
            columns: start_column..segment_end,
        })
    }
}

/// Message-separator replacement applied below its compositor layer.
#[derive(Clone, Copy)]
struct MessageSeparator<'grid> {
    layer: CompositorLayer<'grid>,
    character: VimSCharT,
    attribute: VimSAttrT,
}

/// Highlight groups used to visualize compositor operations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorDebugHighlights {
    normal: i32,
    clear: i32,
    composed: i32,
    recomposed: i32,
}

impl CompositorDebugHighlights {
    const fn empty() -> Self {
        Self {
            normal: 0,
            clear: 0,
            composed: 0,
            recomposed: 0,
        }
    }
}

/// Validated settings shared by compositor-line requests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorLineSettings {
    message_separator_attribute: VimSAttrT,
    screen_columns: usize,
    abort_on_invalid: bool,
}

/// Owned runtime values used while processing one compositor event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorRuntime {
    screen_size: GridSize,
    line_settings: CompositorLineSettings,
    debug_highlights: CompositorDebugHighlights,
    debug_enabled: bool,
    write_delay_ms: u64,
}

impl CompositorRuntime {
    /// Validates dynamic values before they enter safe compositor logic.
    fn try_new(
        screen_rows: i32,
        screen_columns: i32,
        message_separator_attribute: VimSAttrT,
        redraw_debug_flags: u32,
        write_delay: i64,
        debug_highlights: CompositorDebugHighlights,
    ) -> Option<Self> {
        let screen_columns_usize = usize::try_from(screen_columns).ok()?;
        Some(Self {
            screen_size: GridSize::try_new(screen_rows, screen_columns)?,
            line_settings: CompositorLineSettings {
                message_separator_attribute,
                screen_columns: screen_columns_usize,
                abort_on_invalid: redraw_debug_flags & RdbFlags::Invalid as u32 != 0,
            },
            debug_highlights,
            debug_enabled: redraw_debug_flags & RdbFlags::Compositor as u32 != 0,
            write_delay_ms: write_delay.unsigned_abs(),
        })
    }
}

/// Emits one formatted raw-line diagnostic through Neovim's debug logger.
fn log_raw_line_diagnostic(
    diagnostic: RawLineDiagnostic,
    grid_id: GridId,
    output: &mut impl CompositorOutput,
) {
    let message = match diagnostic {
        RawLineDiagnostic::InvalidRow(row) => {
            format!("compositor: invalid row {row} on grid {grid_id}")
        }
        RawLineDiagnostic::ClippedColumns(requested_clear_column) => {
            format!("compositor: invalid last column {requested_clear_column} on grid {grid_id}")
        }
    };
    output.emit(UiCommand::Log(message));
}

/// Validated inputs for composing one screen area.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorAreaRequest {
    area: CompositorRecomposeArea,
    settings: CompositorLineSettings,
}

/// A clipped screen area ready for line-by-line recomposition.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorRecomposePlan {
    area: CompositorRecomposeArea,
    settings: CompositorLineSettings,
}

impl CompositorRecomposePlan {
    /// Produces one owned line request for every row in a nonempty area.
    fn into_line_inputs(self) -> impl Iterator<Item = CompositorLineInput> {
        let has_columns = self.area.columns.start < self.area.columns.end;
        let columns = self.area.columns;
        let settings = self.settings;

        self.area
            .rows
            .filter(move |_| has_columns)
            .map(move |row| CompositorLineInput {
                row,
                columns: columns.clone(),
                flags: LineFlags::INVALID,
                settings,
            })
    }
}

/// Validated inputs for composing one line from temporarily C-owned grids.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorLineInput {
    row: i32,
    columns: Range<i32>,
    flags: LineFlags,
    settings: CompositorLineSettings,
}

/// Validated inputs for composing one complete line in Rust.
#[derive(Clone, Copy)]
struct CompositorLineRequest<'grid> {
    row: i32,
    start_column: i32,
    end_column: i32,
    flags: LineFlags,
    message_separator: Option<MessageSeparator<'grid>>,
    screen_columns: usize,
    abort_on_invalid: bool,
}

/// Rust-owned storage and state for compositor line construction.
///
/// Complete line requests select validated Rust grid segments and copy or
/// blend them through `CompositorSegment`.
struct CompositorBuffer {
    characters: Vec<VimSCharT>,
    attributes: Vec<VimSAttrT>,
}

impl CompositorBuffer {
    /// Creates an empty compositor buffer.
    const fn new() -> Self {
        Self {
            characters: Vec::new(),
            attributes: Vec::new(),
        }
    }

    /// Reallocates the buffer for a screen of `width` columns.
    fn resize(&mut self, width: usize) {
        if self.characters.len() != width {
            self.characters = vec![schar_from_ascii(' '); width];
            self.attributes = vec![0; width];
        }
    }

    /// Releases all compositor storage.
    fn release(&mut self) {
        self.characters = Vec::new();
        self.attributes = Vec::new();
    }

    /// Selects and composes every layer segment for one complete line.
    fn try_compose_line(
        &mut self,
        request: CompositorLineRequest<'_>,
        layers: &CompositorLayerStack<'_>,
    ) -> CompositorResult<CompositorSubmission> {
        let mut line =
            self.prepare_line(request.start_column, request.end_column, request.flags)?;
        let line_end = line.columns.end;
        let mut column = line.columns.start;
        let mut last_layer = None;

        while column < line_end {
            let selection = layers.select_segment(request.row, column, line_end)?;
            let selected_layer = layers
                .layer(selection.layer_index)
                .ok_or(CompositorError::InvalidLayerState)?;
            let separator = request
                .message_separator
                .filter(|separator| selected_layer.layer_index <= separator.layer.layer_index);
            let composed_layer = separator.map_or(selected_layer, |separator| separator.layer);
            let output_columns = selection.columns;

            let background = if composed_layer.blending {
                Some(
                    layers
                        .default_layer()
                        .ok_or(CompositorError::InvalidLayerState)?
                        .screen_segment(request.row, output_columns.start..line_end)?,
                )
            } else {
                None
            };

            if let Some(separator) = separator {
                self.try_compose_separator_segment(
                    &mut line,
                    output_columns.clone(),
                    separator.character,
                    separator.attribute,
                    background,
                )?;
            } else {
                let foreground =
                    selected_layer.screen_segment(request.row, output_columns.clone())?;
                self.try_compose_grid_segment(
                    &mut line,
                    output_columns.clone(),
                    foreground,
                    background,
                )?;
            }

            column = output_columns.end;
            last_layer = Some(composed_layer);
        }

        let allow_wrap = last_layer
            .ok_or(CompositorError::InvalidLineState)?
            .allows_wrap(request.screen_columns);
        self.finish_line(line, request.flags, allow_wrap, request.abort_on_invalid)
    }

    /// Prepares one line and applies invalid-line edge expansion.
    fn prepare_line(
        &self,
        start_col: i32,
        end_col: i32,
        flags: LineFlags,
    ) -> CompositorResult<CompositorLine> {
        let width =
            i32::try_from(self.characters.len()).map_err(|_| CompositorError::InvalidLineState)?;
        let mut start_col = start_col.max(0);
        let mut end_col = end_col;
        let mut skip_start = 0;
        let mut skip_end = 0;

        if flags.contains(LineFlags::INVALID) {
            if start_col > 0 {
                start_col -= 1;
                skip_start = 1;
            }
            if end_col < width {
                end_col += 1;
                skip_end = 1;
            }
        }

        let start_col =
            usize::try_from(start_col).map_err(|_| CompositorError::InvalidLineState)?;
        let end_col = usize::try_from(end_col).map_err(|_| CompositorError::InvalidLineState)?;
        if start_col >= end_col || end_col > self.characters.len() {
            return Err(CompositorError::InvalidLineState);
        }

        Ok(CompositorLine {
            columns: start_col..end_col,
            skip_start,
            skip_end,
        })
    }

    /// Copies and optionally blends one already validated grid segment.
    fn try_compose_grid_segment(
        &mut self,
        line: &mut CompositorLine,
        output_columns: Range<usize>,
        foreground: GridRowSegment<'_>,
        background: Option<GridRowSegment<'_>>,
    ) -> CompositorResult<()> {
        let source_len = output_columns
            .end
            .checked_sub(output_columns.start)
            .ok_or(CompositorError::InvalidLineState)?;
        if source_len == 0 || foreground.len() != source_len {
            return Err(CompositorError::InvalidLineState);
        }

        let line_start = line.columns.start;
        let line_end = line.columns.end;
        if output_columns.start < line_start || output_columns.end > line_end {
            return Err(CompositorError::InvalidLineState);
        }

        let composition = self
            .try_segment_mut(output_columns.clone())?
            .try_copy(foreground)?;

        if composition.is_followed_by_continuation() {
            *self
                .characters
                .get_mut(output_columns.end - 1)
                .ok_or(CompositorError::InvalidLineState)? = schar_from_ascii(' ');
        }

        if let Some(background) = background {
            self.try_segment_mut(output_columns.start..line_end)?
                .try_blend_prefix_over(background, source_len)?;
        }

        self.resolve_segment_edges(
            line,
            output_columns,
            composition.is_followed_by_continuation(),
        )
    }

    /// Fills and optionally blends one message-separator segment.
    fn try_compose_separator_segment(
        &mut self,
        line: &mut CompositorLine,
        output_columns: Range<usize>,
        character: VimSCharT,
        attribute: VimSAttrT,
        background: Option<GridRowSegment<'_>>,
    ) -> CompositorResult<()> {
        let source_len = output_columns
            .end
            .checked_sub(output_columns.start)
            .ok_or(CompositorError::InvalidLineState)?;
        if source_len == 0 {
            return Err(CompositorError::InvalidLineState);
        }

        let line_start = line.columns.start;
        let line_end = line.columns.end;
        if output_columns.start < line_start || output_columns.end > line_end {
            return Err(CompositorError::InvalidLineState);
        }

        self.try_segment_mut(output_columns.clone())?
            .fill(character, attribute);

        if let Some(background) = background {
            self.try_segment_mut(output_columns.start..line_end)?
                .try_blend_prefix_over(background, source_len)?;
        }

        self.resolve_segment_edges(line, output_columns, false)
    }

    /// Completes one line and validates its output attributes.
    fn finish_line(
        &mut self,
        mut line: CompositorLine,
        mut flags: LineFlags,
        allow_wrap: bool,
        abort_on_invalid: bool,
    ) -> CompositorResult<CompositorSubmission> {
        if self.characters.get(
            line.columns
                .end
                .checked_sub(1)
                .ok_or(CompositorError::InvalidLineState)?,
        ) == Some(&0)
        {
            line.skip_end = 0;
        }
        if !allow_wrap {
            flags = flags.without(LineFlags::WRAP);
        }

        let start_col = line
            .columns
            .start
            .checked_add(line.skip_start)
            .ok_or(CompositorError::InvalidLineState)?;
        let end_col = line
            .columns
            .end
            .checked_sub(line.skip_end)
            .ok_or(CompositorError::InvalidLineState)?;
        for attribute in self
            .attributes
            .get_mut(start_col..end_col)
            .ok_or(CompositorError::InvalidLineState)?
        {
            if *attribute < 0 {
                if abort_on_invalid {
                    std::process::abort();
                }
                *attribute = 0;
            }
        }

        Ok(CompositorSubmission {
            start_col,
            end_col,
            flags,
        })
    }

    /// Borrows one output segment through the existing compositor abstraction.
    fn try_segment_mut(
        &mut self,
        columns: Range<usize>,
    ) -> CompositorResult<CompositorSegment<'_>> {
        let characters = self
            .characters
            .get_mut(columns.clone())
            .ok_or(CompositorError::InvalidLineState)?;
        let attributes = self
            .attributes
            .get_mut(columns)
            .ok_or(CompositorError::InvalidLineState)?;
        CompositorSegment::try_from_slices(characters, attributes)
            .ok_or(CompositorError::InvalidLineState)
    }

    /// Resolves wide-cell clipping at one selected segment boundary.
    fn resolve_segment_edges(
        &mut self,
        line: &mut CompositorLine,
        output_columns: Range<usize>,
        followed_by_continuation: bool,
    ) -> CompositorResult<()> {
        let source_len = output_columns
            .end
            .checked_sub(output_columns.start)
            .ok_or(CompositorError::InvalidLineState)?;
        let line_start = line.columns.start;
        let line_end = line.columns.end;
        let mut include_start =
            followed_by_continuation && output_columns.start == line_start && source_len == 1;
        let mut include_end = false;

        let first_character = self
            .characters
            .get_mut(output_columns.start)
            .ok_or(CompositorError::InvalidLineState)?;
        if *first_character == 0 {
            *first_character = schar_from_ascii(' ');
            include_end = output_columns
                .start
                .checked_add(1)
                .ok_or(CompositorError::InvalidLineState)?
                == line_end;
        } else if output_columns.start == line_start
            && source_len > 1
            && self.characters.get(
                output_columns
                    .start
                    .checked_add(1)
                    .ok_or(CompositorError::InvalidLineState)?,
            ) == Some(&0)
        {
            include_start = true;
        }

        if include_start {
            line.skip_start = 0;
        }
        if include_end {
            line.skip_end = 0;
        }
        Ok(())
    }
}

/// Reusable compositor storage that can be checked out without retaining its
/// mutex lock.
///
/// Ordinarily the pool retains one buffer. If composition is reentered while
/// that buffer is leased, another buffer is allocated temporarily.
struct CompositorBufferPool {
    width: usize,
    available_buffer: Option<CompositorBuffer>,
}

impl CompositorBufferPool {
    /// Creates a pool containing one empty reusable buffer.
    const fn new() -> Self {
        Self {
            width: 0,
            available_buffer: Some(CompositorBuffer::new()),
        }
    }

    /// Returns the configured composed-screen width.
    fn width(&self) -> usize {
        self.width
    }

    /// Reconfigures available storage for a new composed-screen width.
    fn resize(&mut self, width: usize) {
        self.width = width;
        if let Some(buffer) = self.available_buffer.as_mut() {
            buffer.resize(width);
        }
    }

    /// Releases all retained compositor storage.
    fn release(&mut self) {
        self.width = 0;
        self.available_buffer = Some(CompositorBuffer::new());
    }

    /// Checks out reusable storage or allocates storage for reentrant work.
    fn take_buffer(&mut self) -> CompositorBuffer {
        let mut buffer = self
            .available_buffer
            .take()
            .unwrap_or_else(CompositorBuffer::new);
        buffer.resize(self.width);
        buffer
    }

    /// Retains one returned buffer when it still has the configured width.
    fn return_buffer(&mut self, buffer: CompositorBuffer) {
        let has_current_width =
            buffer.characters.len() == self.width && buffer.attributes.len() == self.width;
        if self.available_buffer.is_none() && has_current_width {
            self.available_buffer = Some(buffer);
        }
    }
}

/// Exclusive ownership of compositor storage without retaining the pool lock.
///
/// Dropping the lease returns compatible storage to the pool. If a synchronous
/// callback resized the composed screen, the old-width storage is discarded.
#[must_use = "dropping the lease returns its compositor buffer to the pool"]
struct CompositorBufferLease<'pool> {
    pool: &'pool Mutex<CompositorBufferPool>,
    buffer: Option<CompositorBuffer>,
}

impl<'pool> CompositorBufferLease<'pool> {
    /// Checks out one buffer while holding the pool lock only briefly.
    fn acquire(pool: &'pool Mutex<CompositorBufferPool>) -> Self {
        let buffer = pool
            .try_lock()
            .expect("compositor buffer pool is already borrowed")
            .take_buffer();
        Self {
            pool,
            buffer: Some(buffer),
        }
    }
}

impl std::ops::Deref for CompositorBufferLease<'_> {
    type Target = CompositorBuffer;

    fn deref(&self) -> &Self::Target {
        self.buffer
            .as_ref()
            .expect("compositor buffer lease does not own a buffer")
    }
}

impl std::ops::DerefMut for CompositorBufferLease<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.buffer
            .as_mut()
            .expect("compositor buffer lease does not own a buffer")
    }
}

impl Drop for CompositorBufferLease<'_> {
    fn drop(&mut self) {
        let buffer = self
            .buffer
            .take()
            .expect("compositor buffer lease does not own a buffer");
        self.pool
            .try_lock()
            .expect("compositor buffer pool is already borrowed")
            .return_buffer(buffer);
    }
}

static COMPOSITOR: Mutex<Compositor> = Mutex::new(Compositor::new());

/// Acquires exclusive access to the safe process-global compositor state.
fn lock_compositor() -> MutexGuard<'static, Compositor> {
    COMPOSITOR
        .try_lock()
        .expect("compositor state is already borrowed")
}

static COMPOSITOR_BUFFER_POOL: Mutex<CompositorBufferPool> =
    Mutex::new(CompositorBufferPool::new());

/// Acquires exclusive access to the process-global compositor buffer pool.
fn lock_compositor_buffer_pool() -> MutexGuard<'static, CompositorBufferPool> {
    COMPOSITOR_BUFFER_POOL
        .try_lock()
        .expect("compositor buffer pool is already borrowed")
}

/// Checks out compositor storage without retaining the pool mutex.
fn acquire_compositor_buffer() -> CompositorBufferLease<'static> {
    CompositorBufferLease::acquire(&COMPOSITOR_BUFFER_POOL)
}

/// Emits one clear raw-line event used for compositor visualization.
fn emit_compositor_debug_line(
    row: i32,
    columns: Range<i32>,
    attribute: VimSAttrT,
    output: &mut impl CompositorOutput,
) -> CompositorResult<()> {
    let start_column =
        usize::try_from(columns.start).map_err(|_| CompositorError::InvalidGeometry)?;
    let end_column = usize::try_from(columns.end).map_err(|_| CompositorError::InvalidGeometry)?;

    let buffer_width = lock_compositor_buffer_pool().width();
    if start_column > end_column || end_column > buffer_width {
        return Err(CompositorError::InvalidLineState);
    }

    let command = RawLineCommand::try_new(
        row,
        columns.start..columns.start,
        columns.end,
        attribute,
        LineFlags::default(),
        &[],
        &[],
    )
    .ok_or(CompositorError::InvalidLineState)?;
    output.emit(UiCommand::RawLine(command));
    Ok(())
}

/// Reports whether a screen character participates in negative-space blending.
fn is_negative_space(character: VimSCharT) -> bool {
    character == schar_from_ascii(' ') || character == schar_from_char(0x2800)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI32, Ordering};

    use crate::grid::LineBuffer;

    static NEXT_TEST_GRID_ID: AtomicI32 = AtomicI32::new(10_000);

    fn assign_test_grid_id(mut grid: ScreenGrid) -> ScreenGrid {
        grid.handle = NEXT_TEST_GRID_ID.fetch_add(1, Ordering::Relaxed);
        grid
    }

    static COMPOSITOR_LAYER_REGISTRY_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Serializes tests which install a process-global compositor registry.
    struct TestCompositorLayerRegistry {
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for TestCompositorLayerRegistry {
        fn drop(&mut self) {
            *lock_compositor_layer_state() = None;
        }
    }

    /// Installs the supplied C grid bindings and layer order for one test.
    ///
    /// # Safety
    ///
    /// Every pointer in `cgrids` must identify a distinct, exclusively
    /// accessible grid whose buffers satisfy the ownership and layout
    /// requirements of `ScreenGrid`.
    unsafe fn register_test_compositor_layers(
        cgrids: &[*mut CScreenGrid],
    ) -> TestCompositorLayerRegistry {
        let test_lock = COMPOSITOR_LAYER_REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: The caller satisfies the documented pointer and grid
        // ownership requirements.
        let grid_store =
            unsafe { LegacyGridStore::try_take_in_binding_order(cgrids.as_ptr(), cgrids.len()) }
                .expect("test compositor layers must be valid");
        let state = grid_store
            .ffi_state()
            .expect("test compositor layer state must be valid");

        let mut current_state = lock_compositor_layer_state();
        assert!(current_state.is_none());
        *current_state = Some(state);
        drop(current_state);

        TestCompositorLayerRegistry { _lock: test_lock }
    }

    /// Resets and attaches the process-global compositor for a serialized
    /// registry test.
    struct TestAttachedCompositor;

    impl Drop for TestAttachedCompositor {
        fn drop(&mut self) {
            *lock_compositor() = Compositor::new();
        }
    }

    /// Enables drawing for a test already protected by the registry test lock.
    fn attach_test_compositor() -> TestAttachedCompositor {
        {
            let mut compositor = lock_compositor();
            *compositor = Compositor::new();
            compositor.attach();
        }

        TestAttachedCompositor
    }

    fn grid_with_row(characters: &[VimSCharT], attributes: &[VimSAttrT]) -> ScreenGrid {
        assert_eq!(characters.len(), attributes.len());
        let mut line_buffer = LineBuffer::new(characters.len());
        let mut grid = ScreenGrid::new(
            1,
            i32::try_from(characters.len()).unwrap(),
            true,
            &mut line_buffer,
        );
        grid.chars.copy_from_slice(characters);
        grid.attrs.copy_from_slice(attributes);
        assign_test_grid_id(grid)
    }

    fn grid_with_size(rows: i32, columns: i32) -> ScreenGrid {
        let mut line_buffer = LineBuffer::new(columns as usize);
        assign_test_grid_id(ScreenGrid::new(rows, columns, true, &mut line_buffer))
    }

    #[test]
    fn layer_registry_owns_unique_grid_order() {
        let first = GridId::new(1);
        let second = GridId::new(2);
        let third = GridId::new(3);

        assert!(CompositorLayerRegistry::try_new(Vec::new()).is_none());
        assert!(CompositorLayerRegistry::try_new(vec![first, first]).is_none());

        let mut registry = CompositorLayerRegistry::try_new(vec![first, second, third]).unwrap();
        assert_eq!(registry.len(), 3);
        assert_eq!(registry.index_of(second), Some(1));
        assert_eq!(registry.prefix(2).unwrap().layer_ids(), &[first, second]);

        registry
            .apply_layer_move(CompositorLayerMove {
                source_index: 2,
                target_index: 1,
            })
            .unwrap();
        assert_eq!(registry.layer_ids(), &[first, third, second]);
        assert_eq!(
            registry.remove_non_default(0),
            Err(CompositorError::DefaultLayerMutation)
        );
        assert_eq!(registry.remove_non_default(1), Ok(third));
        assert_eq!(registry.layer_ids(), &[first, second]);
    }

    fn compositor_layer(
        grid: &ScreenGrid,
        origin_column: i32,
        visible_width: i32,
        layer_index: usize,
        disabled: bool,
    ) -> CompositorLayer<'_> {
        CompositorLayer {
            grid,
            origin: ComposedPosition::new(0, origin_column),
            requested_size: GridSize {
                height: grid.rows,
                width: visible_width,
            },
            visible_size: GridSize {
                height: grid.rows,
                width: visible_width,
            },
            z_index: 0,
            layer_index,
            blending: false,
            disabled,
            is_default: layer_index == 0,
        }
    }

    fn message_position_request(row: i32, scrolled: bool) -> CompositorMessagePositionRequest {
        CompositorMessagePositionRequest {
            row,
            separator_character: Some(schar_from_ascii('-')),
            screen_size: GridSize {
                height: 10,
                width: 20,
            },
            scrolled,
        }
    }

    #[test]
    fn runtime_validates_and_owns_dynamic_inputs() {
        let debug_highlights = CompositorDebugHighlights {
            normal: 1,
            clear: 2,
            composed: 3,
            recomposed: 4,
        };
        let flags = RdbFlags::Compositor as u32 | RdbFlags::Invalid as u32;

        assert_eq!(
            CompositorRuntime::try_new(20, 10, 9, flags, -7, debug_highlights),
            Some(CompositorRuntime {
                screen_size: GridSize {
                    height: 20,
                    width: 10,
                },
                line_settings: CompositorLineSettings {
                    message_separator_attribute: 9,
                    screen_columns: 10,
                    abort_on_invalid: true,
                },
                debug_highlights,
                debug_enabled: true,
                write_delay_ms: 7,
            })
        );
        assert!(CompositorRuntime::try_new(-1, 10, 9, flags, 0, debug_highlights).is_none());
        assert!(CompositorRuntime::try_new(20, -1, 9, flags, 0, debug_highlights).is_none());
    }

    #[test]
    fn compositor_lifecycle_requires_an_attached_ui_and_valid_screen() {
        let mut compositor = Compositor::new();
        assert!(!compositor.should_draw());

        compositor.attach();
        compositor.attach();
        assert!(compositor.should_draw());

        compositor.message.separator_row = Some(3);
        assert!(compositor.set_screen_valid(false));
        assert!(!compositor.should_draw());
        assert_eq!(compositor.message.separator_row, None);
        assert!(!compositor.set_screen_valid(true));
        assert!(compositor.should_draw());

        assert_eq!(compositor.detach(), Ok(false));
        assert!(compositor.should_draw());
        assert_eq!(compositor.detach(), Ok(true));
        assert!(!compositor.should_draw());
        assert_eq!(compositor.detach(), Err(CompositorError::NoAttachedUi));
    }

    #[test]
    fn compositor_resize_records_screen_size_and_buffer_width() {
        let mut compositor = Compositor::new();
        let size = GridSize {
            height: 24,
            width: 80,
        };

        assert_eq!(
            compositor.resize(size),
            Ok(CompositorResizePlan {
                size,
                buffer_width: 80,
            })
        );
        assert_eq!(compositor.display.screen_size, Some(size));
    }

    #[test]
    fn moving_message_grid_down_recomposes_the_exposed_rows() {
        let mut compositor = Compositor::new();
        let mut message_grid = grid_with_size(10, 20);
        let default_grid = grid_with_size(10, 18);
        assert_eq!(
            compositor.set_message_position(
                &mut message_grid,
                &default_grid,
                message_position_request(5, false),
            ),
            Ok(CompositorMessagePositionPlan::default())
        );

        compositor.attach();
        let plan = compositor
            .set_message_position(
                &mut message_grid,
                &default_grid,
                message_position_request(7, false),
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorMessagePositionPlan {
                scroll: None,
                recompose: Some(CompositorRecomposeArea {
                    rows: 4..7,
                    columns: 0..18,
                }),
            }
        );
        assert_eq!(message_grid.compositor_position(), (7, 0));
    }

    #[test]
    fn moving_message_grid_up_scrolls_text_and_recomposes_a_new_separator() {
        let mut compositor = Compositor::new();
        let mut message_grid = grid_with_size(10, 20);
        let default_grid = grid_with_size(10, 18);
        compositor
            .set_message_position(
                &mut message_grid,
                &default_grid,
                message_position_request(5, false),
            )
            .unwrap();

        compositor.attach();
        let plan = compositor
            .set_message_position(
                &mut message_grid,
                &default_grid,
                message_position_request(2, true),
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorMessagePositionPlan {
                scroll: Some(MessageGridScroll {
                    region: ComposedRegion::try_new(2..10, 0..20).unwrap(),
                    delta: GridDelta::new(3, 0),
                }),
                recompose: Some(CompositorRecomposeArea {
                    rows: 1..2,
                    columns: 0..20,
                }),
            }
        );
        assert_eq!(
            compositor.message_separator_character_at(1),
            Some(schar_from_ascii('-'))
        );
        compositor.hide_message_separator();
        assert_eq!(compositor.message_separator_character_at(1), None);
    }

    #[test]
    fn moving_a_blended_message_grid_up_recomposes_instead_of_scrolling() {
        let mut state = MessageCompositorState::new();
        let mut message_grid = grid_with_size(10, 20);
        message_grid.set_blends_with_background(true);
        let default_grid = grid_with_size(10, 18);
        state
            .update_position(
                &mut message_grid,
                &default_grid,
                message_position_request(5, false),
                false,
            )
            .unwrap();

        let plan = state
            .plan_redraw(
                message_position_request(2, true),
                &message_grid,
                &default_grid,
                true,
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorMessagePositionPlan {
                scroll: None,
                recompose: Some(CompositorRecomposeArea {
                    rows: 1..7,
                    columns: 0..20,
                }),
            }
        );
    }

    #[test]
    fn grid_recomposition_area_uses_position_and_allocated_size() {
        let mut grid = grid_with_size(3, 4);
        grid.set_compositor_position(2, 5);

        assert_eq!(
            CompositorRecomposeArea::try_from_grid(&grid),
            Some(CompositorRecomposeArea {
                rows: 2..5,
                columns: 5..9,
            })
        );
    }

    #[test]
    fn grid_recomposition_area_rejects_overflowing_edges() {
        let mut grid = grid_with_size(1, 1);
        grid.set_compositor_position(i32::MAX, i32::MAX);

        assert_eq!(CompositorRecomposeArea::try_from_grid(&grid), None);
    }

    #[test]
    fn recomposition_area_clamps_ends_to_allocated_grid_size() {
        let grid = grid_with_size(3, 4);

        assert_eq!(
            CompositorRecomposeArea {
                rows: 1..5,
                columns: 2..6,
            }
            .with_ends_clamped_to_grid(&grid),
            Ok(CompositorRecomposeArea {
                rows: 1..3,
                columns: 2..4,
            })
        );
    }

    #[test]
    fn registered_recomposition_areas_use_grid_geometry() {
        let mut grid = grid_with_size(3, 4);
        grid.set_compositor_position(2, 5);
        let grid_id = grid.id();

        // SAFETY: This conversion transfers the grid allocation to its C
        // representation.
        let mut cgrid = unsafe { grid.to_cgrid() };
        let grid_pointer = &mut cgrid as *mut CScreenGrid;
        let cgrids = [grid_pointer];

        {
            // SAFETY: The array contains one valid, exclusively accessible
            // grid for the duration of the operation.
            let _registry = unsafe { register_test_compositor_layers(&cgrids) };
            let grid_store = LegacyGridStore::try_take_registered().unwrap();

            assert_eq!(
                Compositor::new().recompose_area_for_grid(
                    &grid_store.registry,
                    &grid_store,
                    grid_id,
                ),
                Ok(CompositorRecomposeArea {
                    rows: 2..5,
                    columns: 5..9,
                })
            );
            assert_eq!(
                Compositor::new().clip_recompose_area(
                    &grid_store.registry,
                    &grid_store,
                    CompositorRecomposeArea {
                        rows: 1..5,
                        columns: 2..6,
                    },
                ),
                Ok(CompositorRecomposeArea {
                    rows: 1..3,
                    columns: 2..4,
                })
            );
        }

        // SAFETY: The registry guard has restored the grid allocation.
        let grid = unsafe { ScreenGrid::take_from_cgrid(&mut cgrid) };
        assert_eq!(grid.id(), grid_id);
    }

    #[test]
    fn layer_guards_follow_registry_order_and_restore_all_grids() {
        let first = grid_with_row(&[schar_from_ascii('a')], &[1]);
        let second = grid_with_row(&[schar_from_ascii('b')], &[2]);
        let registry = CompositorLayerRegistry::try_new(vec![first.id(), second.id()]).unwrap();

        // SAFETY: Each conversion transfers its grid allocations to a distinct
        // C representation.
        let (mut first_cgrid, mut second_cgrid) = unsafe { (first.to_cgrid(), second.to_cgrid()) };
        let first_pointer = &mut first_cgrid as *mut CScreenGrid;
        let second_pointer = &mut second_cgrid as *mut CScreenGrid;
        let cgrids = [second_pointer, first_pointer];

        {
            // SAFETY: Both pointers identify distinct, exclusively accessible
            // C grids and the pointer array remains readable for this scope.
            let grid_store = unsafe {
                LegacyGridStore::try_take_for_registry(cgrids.as_ptr(), cgrids.len(), registry)
            }
            .unwrap();
            assert_eq!(grid_store.index_of(first_pointer), Some(0));
            assert_eq!(grid_store.index_of(second_pointer), Some(1));
            let first_id = grid_store.registry.layer_id(0).unwrap();
            let second_id = grid_store.registry.layer_id(1).unwrap();
            assert_eq!(
                grid_store.grid(first_id).unwrap().chars.as_slice(),
                &[schar_from_ascii('a')]
            );
            assert_eq!(
                grid_store.grid(second_id).unwrap().chars.as_slice(),
                &[schar_from_ascii('b')]
            );
        }

        // SAFETY: Dropping the collection returned ownership to both C grids.
        let (first, second) = unsafe {
            (
                ScreenGrid::take_from_cgrid(&mut first_cgrid),
                ScreenGrid::take_from_cgrid(&mut second_cgrid),
            )
        };
        assert_eq!(first.chars.as_slice(), &[schar_from_ascii('a')]);
        assert_eq!(second.chars.as_slice(), &[schar_from_ascii('b')]);
    }

    #[test]
    fn layer_adjustment_raises_across_strictly_lower_z_indices() {
        let default_grid = grid_with_size(1, 1);
        let moving_grid = grid_with_size(1, 1);
        let lower_grid = grid_with_size(1, 1);
        let equal_grid = grid_with_size(1, 1);
        let mut moving_layer = compositor_layer(&moving_grid, 0, 1, 1, false);
        moving_layer.z_index = 30;
        let mut lower_layer = compositor_layer(&lower_grid, 0, 1, 2, false);
        lower_layer.z_index = 20;
        let mut equal_layer = compositor_layer(&equal_grid, 0, 1, 3, false);
        equal_layer.z_index = 30;
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 1, 0, false),
                moving_layer,
                lower_layer,
                equal_layer,
            ],
        };

        assert_eq!(
            layers.plan_layer_adjustment(1, CompositorLayerAdjustment::Raise),
            Ok(CompositorLayerMove {
                source_index: 1,
                target_index: 2,
            })
        );
    }

    #[test]
    fn registered_layer_adjustment_updates_rust_order_and_indices() {
        let default_grid = grid_with_size(1, 1);
        let mut moving_grid = grid_with_size(1, 1);
        moving_grid.set_z_index(30);
        moving_grid.set_compositor_index(1);
        let mut lower_grid = grid_with_size(1, 1);
        lower_grid.set_z_index(20);
        lower_grid.set_compositor_index(2);
        let mut equal_grid = grid_with_size(1, 1);
        equal_grid.set_z_index(30);
        equal_grid.set_compositor_index(3);
        let moving_grid_id = moving_grid.id();
        let expected_registry = [
            default_grid.id(),
            lower_grid.id(),
            moving_grid_id,
            equal_grid.id(),
        ];

        // SAFETY: Each conversion transfers its grid allocations to a
        // distinct C representation.
        let (mut default_cgrid, mut moving_cgrid, mut lower_cgrid, mut equal_cgrid) = unsafe {
            (
                default_grid.to_cgrid(),
                moving_grid.to_cgrid(),
                lower_grid.to_cgrid(),
                equal_grid.to_cgrid(),
            )
        };
        let default_pointer = &mut default_cgrid as *mut CScreenGrid;
        let moving_pointer = &mut moving_cgrid as *mut CScreenGrid;
        let lower_pointer = &mut lower_cgrid as *mut CScreenGrid;
        let equal_pointer = &mut equal_cgrid as *mut CScreenGrid;
        let cgrids = [
            default_pointer,
            moving_pointer,
            lower_pointer,
            equal_pointer,
        ];

        {
            // SAFETY: The array contains distinct, exclusively accessible
            // grids for the duration of the adjustment.
            let _registry = unsafe { register_test_compositor_layers(&cgrids) };

            assert_eq!(
                adjust_registered_layer_order(moving_grid_id, CompositorLayerAdjustment::Raise,),
                Ok(())
            );
            assert_eq!(
                lock_compositor_layer_state()
                    .as_ref()
                    .unwrap()
                    .registry
                    .layer_ids(),
                &expected_registry
            );
            assert_eq!(
                cgrids,
                [
                    default_pointer,
                    moving_pointer,
                    lower_pointer,
                    equal_pointer,
                ]
            );
        }

        // SAFETY: The adjustment restored every grid allocation.
        let (default_grid, moving_grid, lower_grid, equal_grid) = unsafe {
            (
                ScreenGrid::take_from_cgrid(&mut default_cgrid),
                ScreenGrid::take_from_cgrid(&mut moving_cgrid),
                ScreenGrid::take_from_cgrid(&mut lower_cgrid),
                ScreenGrid::take_from_cgrid(&mut equal_cgrid),
            )
        };
        assert_eq!(default_grid.compositor_index(), 0);
        assert_eq!(moving_grid.compositor_index(), 2);
        assert_eq!(lower_grid.compositor_index(), 1);
        assert_eq!(equal_grid.compositor_index(), 3);
    }

    #[test]
    fn existing_grid_placement_plans_exposed_and_redrawn_areas() {
        let grid = grid_with_size(3, 4);
        let mut layer = compositor_layer(&grid, 3, 4, 1, false);
        layer.origin.row = 2;
        layer.requested_size.height = 3;
        let placement = CompositorGridPlacement::try_new(3, 5, 2, 2).unwrap();

        assert_eq!(
            layer.try_plan_placement(placement, true, true),
            Ok(CompositorGridPlacementPlan {
                placement,
                moved: true,
                exposed_areas: vec![
                    CompositorRecomposeArea {
                        rows: 2..3,
                        columns: 3..7,
                    },
                    CompositorRecomposeArea {
                        rows: 3..5,
                        columns: 3..5,
                    },
                    CompositorRecomposeArea {
                        rows: 5..5,
                        columns: 3..7,
                    },
                ],
                redraw_area: Some(CompositorRecomposeArea {
                    rows: 3..6,
                    columns: 5..9,
                }),
            })
        );
    }

    #[test]
    fn stationary_grid_resize_only_plans_exposed_areas() {
        let grid = grid_with_size(3, 4);
        let mut layer = compositor_layer(&grid, 3, 4, 1, false);
        layer.origin.row = 2;
        layer.requested_size.height = 3;
        let placement = CompositorGridPlacement::try_new(2, 3, 2, 2).unwrap();

        assert_eq!(
            layer.try_plan_placement(placement, true, true),
            Ok(CompositorGridPlacementPlan {
                placement,
                moved: false,
                exposed_areas: vec![
                    CompositorRecomposeArea {
                        rows: 2..2,
                        columns: 3..7,
                    },
                    CompositorRecomposeArea {
                        rows: 2..4,
                        columns: 5..7,
                    },
                    CompositorRecomposeArea {
                        rows: 4..5,
                        columns: 3..7,
                    },
                ],
                redraw_area: None,
            })
        );
    }

    #[test]
    fn new_grid_placement_plans_redrawing_allocated_size() {
        let grid = grid_with_size(3, 4);
        let placement = CompositorGridPlacement::try_new(2, 5, 2, 2).unwrap();

        assert_eq!(
            CompositorGridPlacementPlan::try_for_new_grid(&grid, placement, true, true),
            Ok(CompositorGridPlacementPlan {
                placement,
                moved: true,
                exposed_areas: Vec::new(),
                redraw_area: Some(CompositorRecomposeArea {
                    rows: 2..5,
                    columns: 5..9,
                }),
            })
        );
    }

    #[test]
    fn layer_insertion_preserves_zindex_and_current_window_ordering() {
        let default_grid = grid_with_size(1, 1);
        let current_grid = grid_with_size(1, 1);
        let upper_grid = grid_with_size(1, 1);
        let mut current_layer = compositor_layer(&current_grid, 0, 1, 1, false);
        current_layer.z_index = 10;
        let mut upper_layer = compositor_layer(&upper_grid, 0, 1, 2, false);
        upper_layer.z_index = 20;
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 1, 0, false),
                current_layer,
                upper_layer,
            ],
        };

        assert_eq!(layers.plan_layer_insertion(10, Some(1), true), Ok(2));
        assert_eq!(layers.plan_layer_insertion(10, Some(1), false), Ok(1));
        assert_eq!(layers.plan_layer_insertion(30, Some(1), true), Ok(3));
        assert_eq!(
            layers.plan_layer_insertion(-1, None, true),
            Err(CompositorError::InvalidLayerState)
        );
    }

    #[test]
    fn cursor_planning_selects_translates_and_raises_a_grid() {
        let default_grid = grid_with_size(10, 12);
        let mut selected_grid = grid_with_size(2, 2);
        selected_grid.handle = 7;
        let upper_grid = grid_with_size(2, 2);
        let mut selected_layer = compositor_layer(&selected_grid, 3, 2, 1, false);
        selected_layer.origin.row = 2;
        selected_layer.z_index = 20;
        let mut upper_layer = compositor_layer(&upper_grid, 0, 2, 2, false);
        upper_layer.z_index = 10;
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 12, 0, false),
                selected_layer,
                upper_layer,
            ],
        };

        assert_eq!(
            layers.plan_cursor(CompositorCursorRequest {
                grid_id: GridId::new(7),
                local_position: GridPosition::new(1, 2),
            }),
            Ok(CompositorCursorAction::Update(CompositorCursorPlan {
                layer_index: 1,
                screen_position: ComposedPosition::new(3, 5),
                raise_to: Some(2),
                emit: true,
            }))
        );
        assert_eq!(
            layers.plan_cursor(CompositorCursorRequest {
                grid_id: GridId::new(7),
                local_position: GridPosition::new(10, 2),
            }),
            Ok(CompositorCursorAction::Update(CompositorCursorPlan {
                layer_index: 1,
                screen_position: ComposedPosition::new(12, 5),
                raise_to: Some(2),
                emit: false,
            }))
        );
        assert_eq!(
            layers.plan_cursor(CompositorCursorRequest {
                grid_id: GridId::new(99),
                local_position: GridPosition::new(0, 0),
            }),
            Ok(CompositorCursorAction::Ignore)
        );
    }

    #[test]
    fn registered_cursor_update_raises_the_layer_and_restores_grid_ownership() {
        let mut default_grid = grid_with_size(10, 12);
        default_grid.set_compositor_size(10, 12);
        let mut selected_grid = grid_with_size(2, 2);
        selected_grid.handle = 7;
        selected_grid.set_compositor_position(2, 3);
        selected_grid.set_compositor_size(2, 2);
        selected_grid.set_compositor_index(1);
        let mut upper_grid = grid_with_size(2, 2);
        upper_grid.set_compositor_position(2, 3);
        upper_grid.set_compositor_size(2, 2);
        upper_grid.set_compositor_index(2);

        // SAFETY: Each conversion transfers its grid allocations to a distinct
        // C representation.
        let (mut default_cgrid, mut selected_cgrid, mut upper_cgrid) = unsafe {
            (
                default_grid.to_cgrid(),
                selected_grid.to_cgrid(),
                upper_grid.to_cgrid(),
            )
        };
        let default_pointer = &mut default_cgrid as *mut CScreenGrid;
        let selected_pointer = &mut selected_cgrid as *mut CScreenGrid;
        let upper_pointer = &mut upper_cgrid as *mut CScreenGrid;
        let cgrids = [default_pointer, selected_pointer, upper_pointer];

        // SAFETY: The array contains distinct, exclusively accessible grids.
        let _registry = unsafe { register_test_compositor_layers(&cgrids) };

        assert_eq!(
            apply_cursor_update_to_registered_grids(CompositorCursorRequest {
                grid_id: GridId::new(99),
                local_position: GridPosition::new(0, 0),
            }),
            Ok(CompositorCursorUpdate::Ignore)
        );
        assert_eq!(cgrids, [default_pointer, selected_pointer, upper_pointer]);

        // The cursor lies outside the default grid, so production code will
        // recompose the exposed area before suppressing cursor output.
        assert_eq!(
            apply_cursor_update_to_registered_grids(CompositorCursorRequest {
                grid_id: GridId::new(7),
                local_position: GridPosition::new(10, 0),
            }),
            Ok(CompositorCursorUpdate::Apply {
                plan: CompositorCursorPlan {
                    layer_index: 1,
                    screen_position: ComposedPosition::new(12, 3),
                    raise_to: Some(2),
                    emit: false,
                },
                recompose_areas: vec![CompositorRecomposeArea {
                    rows: 2..4,
                    columns: 3..5,
                }],
            })
        );
        assert_eq!(cgrids, [default_pointer, selected_pointer, upper_pointer]);

        // SAFETY: Both cursor updates restored every grid allocation.
        let (default_grid, selected_grid, upper_grid) = unsafe {
            (
                ScreenGrid::take_from_cgrid(&mut default_cgrid),
                ScreenGrid::take_from_cgrid(&mut selected_cgrid),
                ScreenGrid::take_from_cgrid(&mut upper_cgrid),
            )
        };
        assert_eq!(default_grid.compositor_index(), 0);
        assert_eq!(selected_grid.compositor_index(), 2);
        assert_eq!(upper_grid.compositor_index(), 1);
    }

    #[test]
    fn layer_insertion_never_displaces_default_layer() {
        let default_grid = grid_with_size(1, 1);
        let layers = CompositorLayerStack {
            layers: vec![compositor_layer(&default_grid, 0, 1, 0, false)],
        };

        assert_eq!(layers.plan_layer_insertion(0, Some(0), false), Ok(1));
    }

    #[test]
    fn registered_layers_place_existing_grid_and_restore_ownership() {
        let default_grid = grid_with_size(4, 4);
        let mut moving_grid = grid_with_size(2, 2);
        moving_grid.set_compositor_index(1);
        moving_grid.set_compositor_size(2, 2);
        // SAFETY: Each conversion transfers its grid allocations to a distinct
        // C representation.
        let (mut default_cgrid, mut moving_cgrid) =
            unsafe { (default_grid.to_cgrid(), moving_grid.to_cgrid()) };
        let default_pointer = &mut default_cgrid as *mut CScreenGrid;
        let moving_pointer = &mut moving_cgrid as *mut CScreenGrid;
        let cgrids = [default_pointer, moving_pointer];
        let mut recomposed_areas = Vec::new();

        // SAFETY: The array contains distinct, exclusively accessible grids.
        let _registry = unsafe { register_test_compositor_layers(&cgrids) };
        let _compositor = attach_test_compositor();

        // SAFETY: The candidate is already registered.
        let (grid_store, target) = unsafe {
            LegacyGridStore::try_take_registered_with_candidate(
                NonNull::new(moving_pointer).unwrap(),
            )
        }
        .unwrap();
        assert_eq!(
            place_grid_in_registered_layers(
                grid_store,
                target,
                None,
                CompositorGridPlacementRequest {
                    placement: CompositorGridPlacement::try_new(1, 2, 2, 2).unwrap(),
                    valid: true,
                    on_top: false,
                },
                |area| {
                    recomposed_areas.push(area);
                    Ok(())
                },
            ),
            Ok(CompositorGridPlacementOutcome::Moved)
        );
        assert_eq!(
            recomposed_areas,
            vec![
                CompositorRecomposeArea {
                    rows: 0..1,
                    columns: 0..2,
                },
                CompositorRecomposeArea {
                    rows: 1..2,
                    columns: 0..2,
                },
                CompositorRecomposeArea {
                    rows: Range { start: 3, end: 2 },
                    columns: 0..2,
                },
                CompositorRecomposeArea {
                    rows: 1..3,
                    columns: 2..4,
                },
            ]
        );
        // SAFETY: The placement restored ownership to both C grids.
        let (default_grid, moving_grid) = unsafe {
            (
                ScreenGrid::take_from_cgrid(&mut default_cgrid),
                ScreenGrid::take_from_cgrid(&mut moving_cgrid),
            )
        };
        assert_eq!(default_grid.compositor_index(), 0);
        assert_eq!(moving_grid.compositor_index(), 1);
        assert_eq!(moving_grid.compositor_position(), (1, 2));
        assert_eq!(moving_grid.compositor_size(), (2, 2));
        assert!(!moving_grid.is_compositor_disabled());
    }

    #[test]
    fn registered_layers_place_appended_grid_and_restore_ownership() {
        let default_grid = grid_with_size(4, 4);
        let mut current_grid = grid_with_size(2, 2);
        current_grid.set_compositor_index(1);
        let appended_grid = grid_with_size(2, 2);
        let current_grid_id = current_grid.id();
        let expected_registry = [default_grid.id(), appended_grid.id(), current_grid_id];
        // SAFETY: Each conversion transfers its grid allocations to a distinct
        // C representation.
        let (mut default_cgrid, mut current_cgrid, mut appended_cgrid) = unsafe {
            (
                default_grid.to_cgrid(),
                current_grid.to_cgrid(),
                appended_grid.to_cgrid(),
            )
        };
        let default_pointer = &mut default_cgrid as *mut CScreenGrid;
        let current_pointer = &mut current_cgrid as *mut CScreenGrid;
        let appended_pointer = &mut appended_cgrid as *mut CScreenGrid;
        let cgrids = [default_pointer, current_pointer];
        let mut recomposed_areas = Vec::new();

        // SAFETY: The array contains distinct, exclusively accessible grids.
        let _registry = unsafe { register_test_compositor_layers(&cgrids) };
        let _compositor = attach_test_compositor();

        // SAFETY: The candidate is distinct from every registered grid and is
        // exclusively accessible.
        let (grid_store, target) = unsafe {
            LegacyGridStore::try_take_registered_with_candidate(
                NonNull::new(appended_pointer).unwrap(),
            )
        }
        .unwrap();
        let current_window_layer = grid_store.registry.index_of(current_grid_id);
        assert_eq!(
            place_grid_in_registered_layers(
                grid_store,
                target,
                current_window_layer,
                CompositorGridPlacementRequest {
                    placement: CompositorGridPlacement::try_new(1, 2, 2, 2).unwrap(),
                    valid: true,
                    on_top: false,
                },
                |area| {
                    recomposed_areas.push(area);
                    Ok(())
                },
            ),
            Ok(CompositorGridPlacementOutcome::Moved)
        );
        assert_eq!(
            recomposed_areas,
            vec![CompositorRecomposeArea {
                rows: 1..3,
                columns: 2..4,
            }]
        );
        assert_eq!(
            lock_compositor_layer_state()
                .as_ref()
                .unwrap()
                .registry
                .layer_ids(),
            &expected_registry
        );

        // SAFETY: The placement restored ownership to every C grid.
        let (default_grid, current_grid, appended_grid) = unsafe {
            (
                ScreenGrid::take_from_cgrid(&mut default_cgrid),
                ScreenGrid::take_from_cgrid(&mut current_cgrid),
                ScreenGrid::take_from_cgrid(&mut appended_cgrid),
            )
        };
        assert_eq!(default_grid.compositor_index(), 0);
        assert_eq!(appended_grid.compositor_index(), 1);
        assert_eq!(current_grid.compositor_index(), 2);
        assert_eq!(appended_grid.compositor_position(), (1, 2));
        assert_eq!(appended_grid.compositor_size(), (2, 2));
    }

    #[test]
    fn layer_adjustment_lowers_across_strictly_higher_z_indices() {
        let default_grid = grid_with_size(1, 1);
        let equal_grid = grid_with_size(1, 1);
        let higher_grid = grid_with_size(1, 1);
        let moving_grid = grid_with_size(1, 1);
        let mut equal_layer = compositor_layer(&equal_grid, 0, 1, 1, false);
        equal_layer.z_index = 10;
        let mut higher_layer = compositor_layer(&higher_grid, 0, 1, 2, false);
        higher_layer.z_index = 20;
        let mut moving_layer = compositor_layer(&moving_grid, 0, 1, 3, false);
        moving_layer.z_index = 10;
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 1, 0, false),
                equal_layer,
                higher_layer,
                moving_layer,
            ],
        };

        assert_eq!(
            layers.plan_layer_adjustment(3, CompositorLayerAdjustment::Lower),
            Ok(CompositorLayerMove {
                source_index: 3,
                target_index: 2,
            })
        );
    }

    #[test]
    fn explicit_layer_raise_plans_every_crossed_layer_area() {
        let default_grid = grid_with_size(6, 8);
        let moving_grid = grid_with_size(3, 3);
        let first_crossed_grid = grid_with_size(2, 2);
        let second_crossed_grid = grid_with_size(2, 2);
        let mut moving_layer = compositor_layer(&moving_grid, 1, 1, 1, false);
        moving_layer.origin.row = 1;
        let mut first_crossed_layer = compositor_layer(&first_crossed_grid, 0, 2, 2, false);
        first_crossed_layer.origin.row = 2;
        let mut second_crossed_layer = compositor_layer(&second_crossed_grid, 3, 2, 3, false);
        second_crossed_layer.origin.row = 0;
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 8, 0, false),
                moving_layer,
                first_crossed_layer,
                second_crossed_layer,
            ],
        };

        assert_eq!(
            layers.plan_layer_raise(1, 3),
            Ok(CompositorLayerRaisePlan {
                movement: CompositorLayerMove {
                    source_index: 1,
                    target_index: 3,
                },
                recompose_areas: vec![
                    CompositorRecomposeArea {
                        rows: 2..4,
                        columns: 1..2,
                    },
                    CompositorRecomposeArea {
                        rows: 1..2,
                        columns: 3..4,
                    },
                ],
            })
        );
    }

    #[test]
    fn explicit_layer_raise_rejects_invalid_movements() {
        let default_grid = grid_with_size(1, 1);
        let first_grid = grid_with_size(1, 1);
        let second_grid = grid_with_size(1, 1);
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 1, 0, false),
                compositor_layer(&first_grid, 0, 1, 1, false),
                compositor_layer(&second_grid, 0, 1, 2, false),
            ],
        };

        assert_eq!(
            layers.plan_layer_raise(0, 1),
            Err(CompositorError::DefaultLayerMutation)
        );
        assert_eq!(
            layers.plan_layer_raise(2, 1),
            Err(CompositorError::InvalidLayerState)
        );
        assert_eq!(
            layers.plan_layer_raise(1, 3),
            Err(CompositorError::InvalidLayerState)
        );
    }

    #[test]
    fn guarded_layer_move_updates_registry_and_grid_indices() {
        let default_grid = grid_with_row(&[schar_from_ascii('d')], &[1]);
        let lower_grid = grid_with_row(&[schar_from_ascii('l')], &[2]);
        let upper_grid = grid_with_row(&[schar_from_ascii('u')], &[3]);
        let expected_registry = [default_grid.id(), upper_grid.id(), lower_grid.id()];
        // SAFETY: Each conversion transfers its grid allocations to a distinct
        // C representation.
        let (mut default_cgrid, mut lower_cgrid, mut upper_cgrid) = unsafe {
            (
                default_grid.to_cgrid(),
                lower_grid.to_cgrid(),
                upper_grid.to_cgrid(),
            )
        };
        let default_pointer = &mut default_cgrid as *mut CScreenGrid;
        let lower_pointer = &mut lower_cgrid as *mut CScreenGrid;
        let upper_pointer = &mut upper_cgrid as *mut CScreenGrid;
        let cgrids = [default_pointer, lower_pointer, upper_pointer];

        {
            // SAFETY: The pointers identify distinct, exclusively accessible
            // C grids and the pointer array remains readable for this scope.
            let mut grid_store = unsafe {
                LegacyGridStore::try_take_in_binding_order(cgrids.as_ptr(), cgrids.len())
            }
            .unwrap();
            let mut registry = grid_store.registry.clone();
            registry
                .move_layer(
                    &mut grid_store,
                    CompositorLayerMove {
                        source_index: 2,
                        target_index: 1,
                    },
                )
                .unwrap();
            assert_eq!(registry.layer_ids(), &expected_registry);
        }

        assert_eq!(cgrids, [default_pointer, lower_pointer, upper_pointer]);

        // SAFETY: Dropping the collection returned ownership to every C grid.
        let (default_grid, lower_grid, upper_grid) = unsafe {
            (
                ScreenGrid::take_from_cgrid(&mut default_cgrid),
                ScreenGrid::take_from_cgrid(&mut lower_cgrid),
                ScreenGrid::take_from_cgrid(&mut upper_cgrid),
            )
        };
        assert_eq!(default_grid.compositor_index(), 0);
        assert_eq!(lower_grid.compositor_index(), 2);
        assert_eq!(upper_grid.compositor_index(), 1);
    }

    #[test]
    fn guarded_layer_removal_updates_registry_and_indices() {
        let default_grid = grid_with_row(&[schar_from_ascii('d')], &[1]);
        let removed_grid = grid_with_row(&[schar_from_ascii('r')], &[2]);
        let remaining_grid = grid_with_row(&[schar_from_ascii('k')], &[3]);
        let expected_registry = [default_grid.id(), remaining_grid.id()];
        // SAFETY: Each conversion transfers its grid allocations to a distinct
        // C representation.
        let (mut default_cgrid, mut removed_cgrid, mut remaining_cgrid) = unsafe {
            (
                default_grid.to_cgrid(),
                removed_grid.to_cgrid(),
                remaining_grid.to_cgrid(),
            )
        };
        let default_pointer = &mut default_cgrid as *mut CScreenGrid;
        let removed_pointer = &mut removed_cgrid as *mut CScreenGrid;
        let remaining_pointer = &mut remaining_cgrid as *mut CScreenGrid;
        let cgrids = [default_pointer, removed_pointer, remaining_pointer];

        {
            // SAFETY: The pointers identify distinct, exclusively accessible
            // C grids and the pointer array remains readable for this scope.
            let mut grid_store = unsafe {
                LegacyGridStore::try_take_in_binding_order(cgrids.as_ptr(), cgrids.len())
            }
            .unwrap();
            let mut registry = grid_store.registry.clone();
            assert_eq!(
                registry.remove_layer(&mut grid_store, 0),
                Err(CompositorError::DefaultLayerMutation)
            );
            assert_eq!(registry.remove_layer(&mut grid_store, 1), Ok(()));
            assert_eq!(registry.layer_ids(), &expected_registry);
        }

        assert_eq!(
            cgrids,
            [default_pointer, removed_pointer, remaining_pointer]
        );

        // SAFETY: Dropping the collection returned ownership to every C grid.
        let (default_grid, removed_grid, remaining_grid) = unsafe {
            (
                ScreenGrid::take_from_cgrid(&mut default_cgrid),
                ScreenGrid::take_from_cgrid(&mut removed_cgrid),
                ScreenGrid::take_from_cgrid(&mut remaining_cgrid),
            )
        };
        assert_eq!(default_grid.compositor_index(), 0);
        assert_eq!(removed_grid.compositor_index(), 0);
        assert_eq!(remaining_grid.compositor_index(), 1);
    }

    #[test]
    fn registered_layer_removal_returns_area_and_restores_ownership() {
        let default_grid = grid_with_size(4, 4);
        let mut removed_grid = grid_with_size(2, 2);
        removed_grid.set_compositor_position(1, 1);
        removed_grid.set_compositor_index(1);
        let mut remaining_grid = grid_with_size(2, 2);
        remaining_grid.set_compositor_index(2);
        let removed_grid_id = removed_grid.id();
        let expected_registry = [default_grid.id(), remaining_grid.id()];

        // SAFETY: Each conversion transfers its grid allocations to a distinct
        // C representation.
        let (mut default_cgrid, mut removed_cgrid, mut remaining_cgrid) = unsafe {
            (
                default_grid.to_cgrid(),
                removed_grid.to_cgrid(),
                remaining_grid.to_cgrid(),
            )
        };
        let default_pointer = &mut default_cgrid as *mut CScreenGrid;
        let removed_pointer = &mut removed_cgrid as *mut CScreenGrid;
        let remaining_pointer = &mut remaining_cgrid as *mut CScreenGrid;
        let registry_cgrids = [default_pointer, removed_pointer, remaining_pointer];

        // SAFETY: The array contains distinct, exclusively accessible grids.
        let _registry = unsafe { register_test_compositor_layers(&registry_cgrids) };
        let _compositor = attach_test_compositor();

        let grid_store = LegacyGridStore::try_take_registered().unwrap();
        let layer_index = grid_store.registry.index_of(removed_grid_id).unwrap();
        assert_eq!(
            remove_registered_layer(grid_store, layer_index),
            Ok(CompositorLayerRemoval {
                recompose_area: Some(CompositorRecomposeArea {
                    rows: 1..3,
                    columns: 1..3,
                }),
            })
        );
        assert_eq!(
            lock_compositor_layer_state()
                .as_ref()
                .unwrap()
                .registry
                .layer_ids(),
            &expected_registry
        );

        {
            let grid_store = LegacyGridStore::try_take_registered().unwrap();
            assert_eq!(grid_store.registry.index_of(removed_grid_id), None);
            assert_eq!(grid_store.index_of(removed_pointer), None);
        }

        // SAFETY: Removal restored every grid allocation.
        let (default_grid, removed_grid, remaining_grid) = unsafe {
            (
                ScreenGrid::take_from_cgrid(&mut default_cgrid),
                ScreenGrid::take_from_cgrid(&mut removed_cgrid),
                ScreenGrid::take_from_cgrid(&mut remaining_cgrid),
            )
        };
        assert_eq!(default_grid.compositor_index(), 0);
        assert_eq!(removed_grid.compositor_index(), 0);
        assert_eq!(remaining_grid.compositor_index(), 1);
    }

    #[test]
    fn hit_testing_selects_the_topmost_overlapping_layer() {
        let default_grid = grid_with_size(4, 4);
        let lower_grid = grid_with_size(3, 3);
        let upper_grid = grid_with_size(3, 3);
        let mut lower_layer = compositor_layer(&lower_grid, 1, 1, 1, false);
        lower_layer.origin.row = 1;
        let mut upper_layer = compositor_layer(&upper_grid, 1, 1, 2, false);
        upper_layer.origin.row = 1;
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 4, 0, false),
                lower_layer,
                upper_layer,
            ],
        };

        assert_eq!(
            layers.topmost_grid_at(ComposedPosition::new(2, 2), LayerHitTest::Any,),
            Some(upper_grid.id())
        );
    }

    #[test]
    fn registered_hit_testing_returns_the_bound_c_grid() {
        let default_grid = grid_with_size(4, 4);
        let mut overlay_grid = grid_with_size(2, 2);
        overlay_grid.set_compositor_position(1, 1);
        overlay_grid.set_compositor_size(2, 2);
        overlay_grid.set_compositor_index(1);
        overlay_grid.set_accepts_mouse_input(false);

        // SAFETY: Each conversion transfers its grid allocations to a
        // distinct C representation.
        let (mut default_cgrid, mut overlay_cgrid) =
            unsafe { (default_grid.to_cgrid(), overlay_grid.to_cgrid()) };
        let default_pointer = &mut default_cgrid as *mut CScreenGrid;
        let overlay_pointer = &mut overlay_cgrid as *mut CScreenGrid;
        let cgrids = [default_pointer, overlay_pointer];

        {
            // SAFETY: The array contains distinct, exclusively accessible
            // grids for the duration of the registered operations.
            let _registry = unsafe { register_test_compositor_layers(&cgrids) };

            assert_eq!(ui_comp_find_grid_at_coord(1, 1, false), overlay_pointer);
            assert!(ui_comp_find_grid_at_coord(1, 1, true).is_null());
            assert!(ui_comp_find_grid_at_coord(0, 0, false).is_null());
        }

        // SAFETY: Hit-testing restored both grid allocations.
        let (_default_grid, _overlay_grid) = unsafe {
            (
                ScreenGrid::take_from_cgrid(&mut default_cgrid),
                ScreenGrid::take_from_cgrid(&mut overlay_cgrid),
            )
        };
    }

    #[test]
    fn hit_testing_excludes_the_default_layer() {
        let default_grid = grid_with_size(3, 3);
        let layers = CompositorLayerStack {
            layers: vec![compositor_layer(&default_grid, 0, 3, 0, false)],
        };

        assert!(
            layers
                .topmost_grid_at(ComposedPosition::new(1, 1), LayerHitTest::Any,)
                .is_none()
        );
    }

    #[test]
    fn positioned_grid_uses_stored_compositor_position() {
        let mut grid = grid_with_size(2, 3);
        grid.set_compositor_position(4, 5);
        let positioned = PositionedGrid::try_from_compositor_position(&grid).unwrap();

        assert!(positioned.contains(ComposedPosition::new(4, 5)));
        assert!(positioned.contains(ComposedPosition::new(5, 7)));
        assert!(!positioned.contains(ComposedPosition::new(3, 5)));
        assert!(!positioned.contains(ComposedPosition::new(6, 5)));
        assert!(!positioned.contains(ComposedPosition::new(5, 8)));
    }

    #[test]
    fn positioned_grid_hit_testing_uses_external_half_open_bounds() {
        let grid = grid_with_size(2, 3);
        let positioned = PositionedGrid::try_new(&grid, ComposedPosition::new(4, 5)).unwrap();

        assert!(positioned.matches(ComposedPosition::new(4, 5), LayerHitTest::MouseEnabled,));
        assert!(positioned.matches(ComposedPosition::new(5, 7), LayerHitTest::MouseEnabled,));
        assert!(!positioned.matches(ComposedPosition::new(6, 5), LayerHitTest::MouseEnabled,));
        assert!(!positioned.matches(ComposedPosition::new(5, 8), LayerHitTest::MouseEnabled,));
    }

    #[test]
    fn mouse_hit_testing_skips_layers_that_reject_mouse_input() {
        let default_grid = grid_with_size(3, 3);
        let lower_grid = grid_with_size(2, 2);
        let mut upper_grid = grid_with_size(2, 2);
        upper_grid.set_accepts_mouse_input(false);
        let lower_layer = compositor_layer(&lower_grid, 1, 2, 1, false);
        let upper_layer = compositor_layer(&upper_grid, 1, 2, 2, false);
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 3, 0, false),
                lower_layer,
                upper_layer,
            ],
        };
        let point = ComposedPosition::new(0, 1);

        assert_eq!(
            layers.topmost_grid_at(point, LayerHitTest::Any),
            Some(upper_grid.id())
        );
        assert_eq!(
            layers.topmost_grid_at(point, LayerHitTest::MouseEnabled),
            Some(lower_grid.id())
        );
    }

    #[test]
    fn hit_testing_reports_when_no_non_default_layer_contains_the_cell() {
        let default_grid = grid_with_size(3, 3);
        let overlay_grid = grid_with_size(1, 1);
        let mut overlay_layer = compositor_layer(&overlay_grid, 2, 1, 1, false);
        overlay_layer.origin.row = 2;
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 3, 0, false),
                overlay_layer,
            ],
        };

        assert!(
            layers
                .topmost_grid_at(ComposedPosition::new(0, 0), LayerHitTest::Any,)
                .is_none()
        );
    }

    #[test]
    fn cursor_coverage_requires_a_distinct_layer_with_a_sufficient_z_index() {
        let default_grid = grid_with_size(3, 3);
        let current_grid = grid_with_size(3, 3);
        let upper_grid = grid_with_size(2, 2);
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 3, 0, false),
                compositor_layer(&upper_grid, 1, 2, 1, false),
            ],
        };
        let covered_point = ComposedPosition::new(0, 1);

        let current_cursor_grid = CompositorCursorGrid::from(&current_grid);
        let upper_cursor_grid = CompositorCursorGrid::from(&upper_grid);

        assert!(layers.is_cursor_covered_by_higher_layer(covered_point, current_cursor_grid, 0));
        assert!(!layers.is_cursor_covered_by_higher_layer(covered_point, current_cursor_grid, 1));
        assert!(!layers.is_cursor_covered_by_higher_layer(covered_point, upper_cursor_grid, 0));
        assert!(!layers.is_cursor_covered_by_higher_layer(
            ComposedPosition::new(2, 2),
            current_cursor_grid,
            0,
        ));
    }

    #[test]
    fn registered_bindings_support_cursor_coverage_queries() {
        let mut default_grid = grid_with_size(4, 4);
        default_grid.set_compositor_size(4, 4);
        let mut current_grid = grid_with_size(2, 2);
        current_grid.set_compositor_position(0, 0);
        current_grid.set_compositor_size(2, 2);
        current_grid.set_compositor_index(1);
        let mut upper_grid = grid_with_size(2, 2);
        upper_grid.set_compositor_position(0, 0);
        upper_grid.set_compositor_size(2, 2);
        upper_grid.set_compositor_index(2);
        let external_grid = grid_with_size(2, 2);
        let current_cursor_grid = CompositorCursorGrid::from(&current_grid);
        let external_cursor_grid = CompositorCursorGrid::from(&external_grid);

        // SAFETY: Each conversion transfers its grid allocations to a
        // distinct C representation.
        let (mut default_cgrid, mut current_cgrid, mut upper_cgrid) = unsafe {
            (
                default_grid.to_cgrid(),
                current_grid.to_cgrid(),
                upper_grid.to_cgrid(),
            )
        };
        let default_pointer = &mut default_cgrid as *mut CScreenGrid;
        let current_pointer = &mut current_cgrid as *mut CScreenGrid;
        let upper_pointer = &mut upper_cgrid as *mut CScreenGrid;
        let cgrids = [default_pointer, current_pointer, upper_pointer];

        {
            // SAFETY: The array contains distinct, exclusively accessible
            // grids for the duration of the coverage queries.
            let _registry = unsafe { register_test_compositor_layers(&cgrids) };
            let _compositor = attach_test_compositor();

            assert_eq!(
                cursor_is_covered_by_registered_layers(current_cursor_grid, 0, 0, 0),
                Ok(true)
            );
            assert_eq!(
                cursor_is_covered_by_registered_layers(external_cursor_grid, 0, 0, 0),
                Ok(true)
            );
            assert_eq!(
                cursor_is_covered_by_registered_layers(current_cursor_grid, 0, 0, 1),
                Ok(false)
            );
        }

        // SAFETY: Every coverage query restored all grid allocations.
        let (_default_grid, _current_grid, _upper_grid) = unsafe {
            (
                ScreenGrid::take_from_cgrid(&mut default_cgrid),
                ScreenGrid::take_from_cgrid(&mut current_cgrid),
                ScreenGrid::take_from_cgrid(&mut upper_cgrid),
            )
        };
    }

    #[test]
    fn copies_foreground_segment() {
        let foreground = grid_with_row(
            &[schar_from_ascii('x'), schar_from_ascii('y'), 0],
            &[1, 2, 3],
        );
        let mut characters = [0; 2];
        let mut attributes = [0; 2];
        let mut output =
            CompositorSegment::try_from_slices(&mut characters, &mut attributes).unwrap();

        let result = output
            .try_copy(
                GridRow::try_from_grid(&foreground, 0)
                    .unwrap()
                    .segment(0..2)
                    .unwrap(),
            )
            .unwrap();

        assert_eq!(characters, [schar_from_ascii('x'), schar_from_ascii('y')]);
        assert_eq!(attributes, [1, 2]);
        assert!(result.is_followed_by_continuation());

        let mut short_characters = [0; 1];
        let mut short_attributes = [0; 1];
        let mut short_output =
            CompositorSegment::try_from_slices(&mut short_characters, &mut short_attributes)
                .unwrap();
        assert_eq!(
            short_output.try_copy(
                GridRow::try_from_grid(&foreground, 0)
                    .unwrap()
                    .segment(0..2)
                    .unwrap()
            ),
            Err(CompositorError::InvalidLineState)
        );
    }

    #[test]
    fn copies_foreground_cells_without_blending() {
        let background = grid_with_row(
            &[
                schar_from_ascii('a'),
                schar_from_ascii('b'),
                schar_from_ascii('c'),
            ],
            &[10, 20, 30],
        );
        let foreground = grid_with_row(
            &[schar_from_ascii('x'), schar_from_ascii('y'), 0],
            &[1, 2, 3],
        );
        let mut characters = [0; 2];
        let mut attributes = [0; 2];
        let mut output =
            CompositorSegment::try_from_slices(&mut characters, &mut attributes).unwrap();

        let result = output
            .try_compose(
                GridRow::try_from_grid(&background, 0)
                    .unwrap()
                    .segment(0..2)
                    .unwrap(),
                GridRow::try_from_grid(&foreground, 0)
                    .unwrap()
                    .segment(0..2)
                    .unwrap(),
                false,
            )
            .unwrap();

        assert_eq!(characters, [schar_from_ascii('x'), schar_from_ascii('y')]);
        assert_eq!(attributes, [1, 2]);
        assert!(result.is_followed_by_continuation());
    }

    #[test]
    fn scroll_preparation_keeps_only_rows_with_valid_source_cells() {
        let grid = grid_with_row(&[schar_from_ascii('x'), schar_from_ascii('y')], &[1, -1]);
        let region = ComposedRegion::try_new(4..7, 0..2).unwrap();
        let valid_column_plan = CompositorScrollPlan::Recompose {
            region: region.clone(),
            screen_rows: 4..7,
            first_grid_row: 0,
            grid_start_column: 0,
        };
        let invalid_column_plan = CompositorScrollPlan::Recompose {
            region: region.clone(),
            screen_rows: 4..7,
            first_grid_row: 0,
            grid_start_column: 1,
        };

        assert_eq!(
            valid_column_plan.prepare(&grid),
            CompositorScrollOperation::Recompose {
                region: region.clone(),
                rows: vec![4],
            }
        );
        assert_eq!(
            invalid_column_plan.prepare(&grid),
            CompositorScrollOperation::Recompose {
                region,
                rows: Vec::new(),
            }
        );
    }

    #[test]
    fn compositor_rect_intersections_include_shared_edges() {
        let rectangle = CompositorRect::try_new(2, 4, 3, 6).unwrap();

        assert!(rectangle.intersects(CompositorRect::try_new(4, 5, 6, 7).unwrap()));
        assert!(!rectangle.intersects(CompositorRect::try_new(5, 6, 3, 6).unwrap()));
        assert!(!rectangle.intersects(CompositorRect::try_new(2, 4, 7, 8).unwrap()));
    }

    #[test]
    fn coverage_uses_requested_size_and_only_layers_above() {
        let default_grid = grid_with_row(&[schar_from_ascii('a')], &[1]);
        let overlay_grid = grid_with_row(&[schar_from_ascii('b')], &[2]);
        let mut overlay = compositor_layer(&overlay_grid, 4, 1, 1, false);
        overlay.origin.row = 2;
        overlay.requested_size = GridSize {
            height: 2,
            width: 3,
        };
        let layers = CompositorLayerStack {
            layers: vec![compositor_layer(&default_grid, 0, 1, 0, false), overlay],
        };
        let rectangle = CompositorRect::try_new(3, 3, 6, 6).unwrap();

        assert_eq!(
            layers.is_covered_above(CompositorCoveredAboveQuery {
                rectangle,
                current_layer_index: 0,
                message_layer_index: None,
                message_was_scrolled: false,
            }),
            Ok(true)
        );
        assert_eq!(
            layers.is_covered_above(CompositorCoveredAboveQuery {
                rectangle,
                current_layer_index: 1,
                message_layer_index: None,
                message_was_scrolled: false,
            }),
            Ok(false)
        );
    }

    #[test]
    fn scrolled_message_layer_covers_separator_row() {
        let default_grid = grid_with_row(&[schar_from_ascii('a')], &[1]);
        let message_grid = grid_with_row(&[schar_from_ascii('m')], &[2]);
        let mut message = compositor_layer(&message_grid, 1, 1, 1, false);
        message.origin.row = 2;
        message.requested_size = GridSize {
            height: 1,
            width: 2,
        };
        let layers = CompositorLayerStack {
            layers: vec![compositor_layer(&default_grid, 0, 1, 0, false), message],
        };
        let rectangle = CompositorRect::try_new(1, 1, 1, 1).unwrap();
        let query = CompositorCoveredAboveQuery {
            rectangle,
            current_layer_index: 0,
            message_layer_index: Some(1),
            message_was_scrolled: false,
        };

        assert_eq!(layers.is_covered_above(query), Ok(false));
        assert_eq!(
            layers.is_covered_above(CompositorCoveredAboveQuery {
                message_was_scrolled: true,
                ..query
            }),
            Ok(true)
        );
    }

    #[test]
    fn plans_forwarded_raw_line_in_composed_screen_coordinates() {
        let default_grid = grid_with_size(5, 10);
        let current_grid = grid_with_size(2, 5);
        let mut current_layer = compositor_layer(&current_grid, 3, 5, 1, false);
        current_layer.origin.row = 1;
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 10, 0, false),
                current_layer,
            ],
        };

        let plan = layers
            .plan_raw_line(
                1,
                None,
                false,
                CompositorRawLineRequest {
                    row: 1,
                    columns: 0..3,
                    clear_column: 5,
                    flags: LineFlags::WRAP,
                },
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorRawLinePlan {
                row: 2,
                columns: 3..6,
                clear_column: 8,
                requested_clear_column: 8,
                flags: LineFlags::default(),
                action: CompositorRawLineAction::Forward,
            }
        );
    }

    #[test]
    fn rejects_unordered_raw_line_columns() {
        assert!(CompositorRawLineRequest::try_new(0, 2, 1, 3, LineFlags::default()).is_none());
        assert!(CompositorRawLineRequest::try_new(0, 1, 3, 2, LineFlags::default()).is_none());
    }

    #[test]
    fn plans_clipped_raw_line_recomposition() {
        let default_grid = grid_with_size(5, 8);
        let overlay_grid = grid_with_size(1, 1);
        let mut overlay = compositor_layer(&overlay_grid, 6, 1, 1, false);
        overlay.origin.row = 1;
        let layers = CompositorLayerStack {
            layers: vec![compositor_layer(&default_grid, 0, 8, 0, false), overlay],
        };

        let plan = layers
            .plan_raw_line(
                0,
                None,
                false,
                CompositorRawLineRequest {
                    row: 1,
                    columns: 4..7,
                    clear_column: 10,
                    flags: LineFlags::WRAP,
                },
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorRawLinePlan {
                row: 1,
                columns: 4..7,
                clear_column: 8,
                requested_clear_column: 10,
                flags: LineFlags::WRAP,
                action: CompositorRawLineAction::Recompose,
            }
        );
    }

    #[test]
    fn discards_raw_lines_outside_the_default_grid() {
        let default_grid = grid_with_size(5, 8);
        let layers = CompositorLayerStack {
            layers: vec![compositor_layer(&default_grid, 0, 8, 0, false)],
        };

        let row_plan = layers
            .plan_raw_line(
                0,
                None,
                false,
                CompositorRawLineRequest {
                    row: 5,
                    columns: 0..3,
                    clear_column: 4,
                    flags: LineFlags::default(),
                },
            )
            .unwrap();
        let column_plan = layers
            .plan_raw_line(
                0,
                None,
                false,
                CompositorRawLineRequest {
                    row: 1,
                    columns: 8..9,
                    clear_column: 10,
                    flags: LineFlags::default(),
                },
            )
            .unwrap();

        assert_eq!(row_plan.action, CompositorRawLineAction::DiscardRow);
        assert_eq!(column_plan.action, CompositorRawLineAction::DiscardColumns);
        assert_eq!(column_plan.clear_column, 8);
        assert_eq!(column_plan.requested_clear_column, 10);
    }

    #[test]
    fn plans_uncovered_scroll_in_composed_screen_coordinates() {
        let grid = grid_with_row(&[schar_from_ascii('a')], &[1]);
        let mut layer = compositor_layer(&grid, 4, 1, 0, false);
        layer.origin.row = 3;
        let layers = CompositorLayerStack {
            layers: vec![layer],
        };

        let plan = layers
            .plan_scroll(
                0,
                None,
                false,
                CompositorScrollRequest {
                    region: GridRegion::try_new(2..7, 1..5).unwrap(),
                    delta: GridDelta::new(2, -1),
                },
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorScrollPlan::Forward {
                region: ComposedRegion::try_new(5..10, 5..9).unwrap(),
                delta: GridDelta::new(2, -1),
            }
        );
    }

    #[test]
    fn rejects_unordered_scroll_regions() {
        assert!(CompositorScrollRequest::try_new(2, 1, 0, 1, 0, 0).is_none());
        assert!(CompositorScrollRequest::try_new(0, 1, 2, 1, 0, 0).is_none());
    }

    #[test]
    fn plans_covered_scroll_recomposition_rows() {
        let default_grid = grid_with_row(&[schar_from_ascii('a')], &[1]);
        let overlay_grid = grid_with_row(&[schar_from_ascii('b')], &[2]);
        let mut default_layer = compositor_layer(&default_grid, 4, 1, 0, false);
        default_layer.origin.row = 3;
        let mut overlay = compositor_layer(&overlay_grid, 6, 1, 1, false);
        overlay.origin.row = 6;
        overlay.requested_size = GridSize {
            height: 1,
            width: 1,
        };
        let layers = CompositorLayerStack {
            layers: vec![default_layer, overlay],
        };

        let plan = layers
            .plan_scroll(
                0,
                None,
                false,
                CompositorScrollRequest {
                    region: GridRegion::try_new(2..7, 1..5).unwrap(),
                    delta: GridDelta::new(2, 0),
                },
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorScrollPlan::Recompose {
                region: ComposedRegion::try_new(5..10, 5..9).unwrap(),
                screen_rows: 5..8,
                first_grid_row: 2,
                grid_start_column: 1,
            }
        );
    }

    #[test]
    fn plans_upward_scroll_recomposition_rows() {
        let default_grid = grid_with_row(&[schar_from_ascii('a')], &[1]);
        let mut layer = compositor_layer(&default_grid, 4, 1, 0, false);
        layer.origin.row = 3;
        layer.blending = true;
        let layers = CompositorLayerStack {
            layers: vec![layer],
        };

        let plan = layers
            .plan_scroll(
                0,
                None,
                false,
                CompositorScrollRequest {
                    region: GridRegion::try_new(2..7, 1..5).unwrap(),
                    delta: GridDelta::new(-2, 0),
                },
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorScrollPlan::Recompose {
                region: ComposedRegion::try_new(5..10, 5..9).unwrap(),
                screen_rows: 7..10,
                first_grid_row: 4,
                grid_start_column: 1,
            }
        );
    }

    #[test]
    fn blends_negative_space_over_background_cells() {
        let background = grid_with_row(
            &[
                schar_from_ascii('W'),
                0,
                schar_from_ascii('b'),
                schar_from_ascii('c'),
            ],
            &[10, 20, 30, 40],
        );
        let foreground = grid_with_row(
            &[
                schar_from_ascii(' '),
                schar_from_ascii(' '),
                schar_from_char(0x2800),
                schar_from_ascii('x'),
            ],
            &[1, 2, 3, 4],
        );
        let mut characters = [0; 4];
        let mut attributes = [0; 4];
        let mut output =
            CompositorSegment::try_from_slices(&mut characters, &mut attributes).unwrap();

        output
            .try_copy(
                GridRow::try_from_grid(&foreground, 0)
                    .unwrap()
                    .segment(0..4)
                    .unwrap(),
            )
            .unwrap();
        output
            .try_blend_prefix_over_with(
                GridRow::try_from_grid(&background, 0)
                    .unwrap()
                    .segment(0..4)
                    .unwrap(),
                4,
                |background, foreground, through| (background + foreground, through),
            )
            .unwrap();

        assert_eq!(
            characters,
            [
                schar_from_ascii('W'),
                0,
                schar_from_ascii('b'),
                schar_from_ascii('x'),
            ]
        );
        assert_eq!(attributes, [11, 22, 33, 44]);
    }

    #[test]
    fn blends_wide_background_continuation_past_prefix() {
        let background = grid_with_row(&[schar_from_ascii('W'), 0], &[10, 20]);
        let mut characters = [schar_from_ascii(' '), schar_from_ascii(' ')];
        let mut attributes = [1, 2];
        let mut output =
            CompositorSegment::try_from_slices(&mut characters, &mut attributes).unwrap();

        output
            .try_blend_prefix_over_with(
                GridRow::try_from_grid(&background, 0)
                    .unwrap()
                    .segment(0..2)
                    .unwrap(),
                1,
                |background, foreground, through| (background + foreground, through),
            )
            .unwrap();

        assert_eq!(characters, [schar_from_ascii('W'), 0]);
        assert_eq!(attributes, [11, 22]);
    }

    #[test]
    fn rejects_mismatched_lengths() {
        let mut characters = [0; 2];
        let mut one_attribute = [0; 1];
        assert!(CompositorSegment::try_from_slices(&mut characters, &mut one_attribute).is_none());

        let foreground = grid_with_row(&[schar_from_ascii('x')], &[1]);
        let background = grid_with_row(&[schar_from_ascii('a'), schar_from_ascii('b')], &[2, 3]);
        let mut attributes = [0; 2];
        let mut output =
            CompositorSegment::try_from_slices(&mut characters, &mut attributes).unwrap();

        assert_eq!(
            output.try_compose(
                GridRow::try_from_grid(&background, 0)
                    .unwrap()
                    .segment(0..2)
                    .unwrap(),
                GridRow::try_from_grid(&foreground, 0)
                    .unwrap()
                    .segment(0..1)
                    .unwrap(),
                false,
            ),
            Err(CompositorError::InvalidLineState)
        );
    }

    #[test]
    fn compositor_buffer_pool_allows_reentrant_leases() {
        let pool = Mutex::new(CompositorBufferPool::new());
        pool.try_lock().unwrap().resize(3);

        {
            let first = CompositorBufferLease::acquire(&pool);
            assert_eq!(first.characters.len(), 3);
            assert_eq!(first.attributes.len(), 3);

            {
                let second = CompositorBufferLease::acquire(&pool);
                assert_eq!(second.characters.len(), 3);
                assert_eq!(second.attributes.len(), 3);
            }

            assert!(pool.try_lock().unwrap().available_buffer.is_some());
        }

        assert!(pool.try_lock().unwrap().available_buffer.is_some());
    }

    #[test]
    fn compositor_buffer_pool_discards_old_width_storage() {
        let pool = Mutex::new(CompositorBufferPool::new());
        pool.try_lock().unwrap().resize(2);

        {
            let leased = CompositorBufferLease::acquire(&pool);
            assert_eq!(leased.characters.len(), 2);
            assert_eq!(leased.attributes.len(), 2);
            pool.try_lock().unwrap().resize(4);
        }

        assert!(pool.try_lock().unwrap().available_buffer.is_none());

        let resized = CompositorBufferLease::acquire(&pool);
        assert_eq!(resized.characters.len(), 4);
        assert_eq!(resized.attributes.len(), 4);
    }

    #[test]
    fn owned_buffer_composes_grid_segments() {
        let foreground = grid_with_row(&[schar_from_ascii('x'), schar_from_ascii('y')], &[1, -1]);
        let foreground = GridRow::try_from_grid(&foreground, 0)
            .unwrap()
            .segment(0..2)
            .unwrap();
        let mut compositor = CompositorBuffer::new();

        compositor.resize(2);
        let mut line = compositor.prepare_line(0, 2, LineFlags::default()).unwrap();
        assert_eq!(line.columns, 0..2);
        compositor
            .try_compose_grid_segment(&mut line, 0..2, foreground, None)
            .unwrap();
        let submission = compositor
            .finish_line(line, LineFlags::default(), true, false)
            .unwrap();

        assert_eq!(
            compositor.characters,
            [schar_from_ascii('x'), schar_from_ascii('y')]
        );
        assert_eq!(compositor.attributes, [1, 0]);
        assert_eq!(
            submission,
            CompositorSubmission {
                start_col: 0,
                end_col: 2,
                flags: LineFlags::default(),
            }
        );
    }

    #[test]
    fn owned_buffer_selects_topmost_visible_segments() {
        let default_grid = grid_with_row(
            &[
                schar_from_ascii('a'),
                schar_from_ascii('b'),
                schar_from_ascii('c'),
                schar_from_ascii('d'),
                schar_from_ascii('e'),
                schar_from_ascii('f'),
            ],
            &[1, 2, 3, 4, 5, 6],
        );
        let overlay = grid_with_row(
            &[
                schar_from_ascii('X'),
                schar_from_ascii('Y'),
                schar_from_ascii('Z'),
            ],
            &[7, 8, 9],
        );
        let disabled = grid_with_row(&[schar_from_ascii('Q')], &[10]);
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 6, 0, false),
                compositor_layer(&overlay, 2, 2, 1, false),
                compositor_layer(&disabled, 0, 1, 2, true),
            ],
        };
        let mut compositor = CompositorBuffer::new();

        compositor.resize(6);
        let submission = compositor
            .try_compose_line(
                CompositorLineRequest {
                    row: 0,
                    start_column: 0,
                    end_column: 6,
                    flags: LineFlags::default(),
                    message_separator: None,
                    screen_columns: 6,
                    abort_on_invalid: false,
                },
                &layers,
            )
            .unwrap();

        assert_eq!(
            compositor.characters,
            [
                schar_from_ascii('a'),
                schar_from_ascii('b'),
                schar_from_ascii('X'),
                schar_from_ascii('Y'),
                schar_from_ascii('e'),
                schar_from_ascii('f'),
            ]
        );
        assert_eq!(compositor.attributes, [1, 2, 7, 8, 5, 6]);
        assert_eq!(submission.start_col, 0);
        assert_eq!(submission.end_col, 6);
    }

    #[test]
    fn owned_buffer_composes_complete_message_separator_lines() {
        let default_grid = grid_with_row(
            &[
                schar_from_ascii('a'),
                schar_from_ascii('b'),
                schar_from_ascii('c'),
            ],
            &[1, 2, 3],
        );
        let message_grid = grid_with_row(&[schar_from_ascii('m'), schar_from_ascii('m')], &[4, 4]);
        let layers = CompositorLayerStack {
            layers: vec![compositor_layer(&default_grid, 0, 3, 0, false)],
        };
        let mut compositor = CompositorBuffer::new();

        compositor.resize(3);
        let submission = compositor
            .try_compose_line(
                CompositorLineRequest {
                    row: 0,
                    start_column: 0,
                    end_column: 3,
                    flags: LineFlags::WRAP,
                    message_separator: Some(MessageSeparator {
                        layer: compositor_layer(&message_grid, 0, 2, 1, false),
                        character: schar_from_ascii('-'),
                        attribute: 7,
                    }),
                    screen_columns: 3,
                    abort_on_invalid: false,
                },
                &layers,
            )
            .unwrap();

        assert_eq!(
            compositor.characters,
            [
                schar_from_ascii('-'),
                schar_from_ascii('-'),
                schar_from_ascii('-')
            ]
        );
        assert_eq!(compositor.attributes, [7, 7, 7]);
        assert_eq!(submission.flags, LineFlags::default());
        compositor.release();
        assert!(compositor.characters.is_empty());
        assert!(compositor.attributes.is_empty());
    }
}
