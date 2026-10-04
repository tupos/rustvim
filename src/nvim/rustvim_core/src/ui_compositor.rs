mod context;
mod ffi;

pub(crate) use context::{CompositorContext, CompositorEventOutputs};
pub(crate) use ffi::{NvimComposedOutput, compositor_runtime_snapshot, current_message_grid_id};

use std::{ops::Range, sync::MutexGuard};

use crate::{
    context::registered_context,
    generated::enums::RdbFlags,
    grid::{
        ComposedGridSpace, ComposedPosition, ComposedRegion, GridCell, GridDelta, GridId,
        GridPosition, GridRegion, GridRow, GridRowSegment, GridSize, GridStore, Highlight,
        ScreenGrid, VimSAttrT, VimSCharT, schar_from_ascii, schar_from_char,
    },
    ui::{
        LineFlags,
        output::{BorrowedRawLineCommand, CellBuffer, CellBufferPool, UiCommand, UiOutput},
    },
};
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

/// Identity and fallback stacking position for a cursor coverage query.
///
/// Registered grids use their compositor-owned z-index. The fallback supports
/// grids that are not registered as compositor layers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorCursorGrid {
    grid_id: GridId,
    z_index: i32,
}

impl CompositorCursorGrid {
    /// Creates one cursor-grid identity with an unregistered-grid fallback.
    const fn new(grid_id: GridId, z_index: i32) -> Self {
        Self { grid_id, z_index }
    }
}

/// A Rust-owned grid positioned at an explicit screen origin.
#[derive(Clone, Copy, Debug)]
struct PositionedGrid<'grid> {
    grid: &'grid ScreenGrid,
    origin: ComposedPosition,
}

impl<'grid> PositionedGrid<'grid> {
    /// Constructs a positioned view of a grid.
    fn new(grid: &'grid ScreenGrid, origin: ComposedPosition) -> Self {
        Self { grid, origin }
    }

    /// Reports whether the allocated grid contains one screen cell.
    fn contains(self, point: ComposedPosition) -> bool {
        let size = self.grid.size();
        point.row >= self.origin.row
            && self
                .origin
                .row
                .checked_add(size.height)
                .is_some_and(|bottom| point.row < bottom)
            && point.column >= self.origin.column
            && self
                .origin
                .column
                .checked_add(size.width)
                .is_some_and(|right| point.column < right)
    }

    /// Translates one contained screen position into grid-local coordinates.
    fn local_position(self, point: ComposedPosition) -> Option<GridPosition> {
        self.contains(point).then(|| {
            GridPosition::new(
                point.row - self.origin.row,
                point.column - self.origin.column,
            )
        })
    }

    /// Returns this grid's identity and local coordinates for one contained
    /// composed-screen position.
    fn hit(self, point: ComposedPosition) -> Option<CompositorGridHit> {
        Some(CompositorGridHit {
            grid_id: self.grid.id(),
            local_position: self.local_position(point)?,
        })
    }

    /// Reads the cell at one contained composed-screen position.
    fn cell(self, point: ComposedPosition) -> Option<GridCell> {
        let position = self.hit(point)?.local_position;
        let row = usize::try_from(position.row).ok()?;
        let column = usize::try_from(position.column).ok()?;
        self.grid.cell(row, column)
    }
}

/// One compositor hit with coordinates relative to the selected grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorGridHit {
    grid_id: GridId,
    local_position: GridPosition,
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
    fn try_from_grid(grid: &ScreenGrid, origin: ComposedPosition) -> Option<Self> {
        let size = grid.size();

        Some(Self {
            rows: origin.row..origin.row.checked_add(size.height)?,
            columns: origin.column..origin.column.checked_add(size.width)?,
        })
    }

    /// Clamps this area's exclusive ends to an allocated grid's dimensions.
    ///
    /// Start coordinates remain unchanged to preserve the compositor's
    /// established clipping behavior.
    fn with_ends_clamped_to_grid(self, grid: &ScreenGrid) -> CompositorResult<Self> {
        let size = grid.size();

        Ok(Self {
            rows: self.rows.start..self.rows.end.min(size.height),
            columns: self.columns.start..self.columns.end.min(size.width),
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
pub(crate) struct CompositorGridPlacement {
    origin: ComposedPosition,
    size: GridSize,
}

/// Validated native inputs for placing one compositor grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CompositorGridPlacementRequest {
    placement: CompositorGridPlacement,
    z_index: i32,
    blending: bool,
    valid: bool,
    on_top: bool,
    accepts_mouse_input: bool,
}

impl CompositorGridPlacementRequest {
    /// Constructs a request from validated placement geometry.
    pub(crate) const fn new(
        placement: CompositorGridPlacement,
        z_index: i32,
        blending: bool,
        valid: bool,
        on_top: bool,
        accepts_mouse_input: bool,
    ) -> Self {
        Self {
            placement,
            z_index,
            blending,
            valid,
            on_top,
            accepts_mouse_input,
        }
    }
}

/// Complete inputs for placing one registered compositor grid.
pub(crate) struct RegisteredGridPlacement {
    grid_id: GridId,
    current_window_grid_id: Option<GridId>,
    message_grid_id: Option<GridId>,
    request: CompositorGridPlacementRequest,
    runtime: CompositorRuntime,
}

impl RegisteredGridPlacement {
    /// Combines grid identities, placement policy, and one runtime snapshot.
    pub(crate) const fn new(
        grid_id: GridId,
        current_window_grid_id: Option<GridId>,
        message_grid_id: Option<GridId>,
        request: CompositorGridPlacementRequest,
        runtime: CompositorRuntime,
    ) -> Self {
        Self {
            grid_id,
            current_window_grid_id,
            message_grid_id,
            request,
            runtime,
        }
    }
}

impl CompositorGridPlacement {
    /// Constructs a placement with nonnegative dimensions.
    pub(crate) fn try_new(row: i32, column: i32, height: i32, width: i32) -> Option<Self> {
        Some(Self {
            origin: ComposedPosition::new(row, column),
            size: GridSize::try_new(height, width)?,
        })
    }

    /// Returns the requested origin in composed-screen coordinates.
    pub(crate) const fn origin(self) -> ComposedPosition {
        self.origin
    }

    /// Returns the requested grid dimensions.
    pub(crate) const fn size(self) -> GridSize {
        self.size
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
        let size = grid.size();

        Ok(CompositorRecomposeArea {
            rows: self.origin.row
                ..self
                    .origin
                    .row
                    .checked_add(size.height)
                    .ok_or(CompositorError::InvalidGeometry)?,
            columns: self.origin.column
                ..self
                    .origin
                    .column
                    .checked_add(size.width)
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

/// One placement target resolved against the registered grids.
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
pub(crate) struct CompositorMessagePositionRequest {
    row: i32,
    z_index: i32,
    separator_character: Option<VimSCharT>,
    screen_size: GridSize,
    scrolled: bool,
}

impl CompositorMessagePositionRequest {
    /// Constructs a message-position request from validated native values.
    pub(crate) fn new(
        row: i32,
        z_index: i32,
        separator_character: Option<VimSCharT>,
        screen_size: GridSize,
        scrolled: bool,
    ) -> Self {
        Self {
            row,
            z_index,
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
    z_index: i32,
    blending: bool,
    was_scrolled: bool,
    separator_row: Option<i32>,
    separator_character: VimSCharT,
}

impl MessageCompositorState {
    /// Creates the initial state used before the first message-grid event.
    const fn new() -> Self {
        Self {
            current_row: i32::MAX,
            z_index: 0,
            blending: false,
            was_scrolled: false,
            separator_row: None,
            separator_character: VimSCharT::from_ne_bytes([b' ', 0, 0, 0]),
        }
    }

    /// Plans the redraw effects without changing the current state.
    fn plan_redraw(
        &self,
        request: CompositorMessagePositionRequest,
        default_grid: &ScreenGrid,
        should_draw: bool,
    ) -> CompositorResult<CompositorMessagePositionPlan> {
        let screen_width = default_grid.size().width;
        if !should_draw {
            return Ok(CompositorMessagePositionPlan::default());
        }

        if request.row > self.current_row {
            return Ok(CompositorMessagePositionPlan {
                scroll: None,
                recompose: Some(CompositorRecomposeArea {
                    rows: self.current_row.saturating_sub(1).max(0)..request.row,
                    columns: 0..screen_width,
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
        if self.blending {
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
        let plan = self.plan_redraw(request, default_grid, should_draw)?;

        if separator_row.is_some()
            && let Some(character) = request.separator_character
        {
            self.separator_character = character;
        }
        self.separator_row = separator_row;
        self.current_row = request.row;
        self.z_index = request.z_index;
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

/// Compositor-owned state for one registered grid layer.
///
/// Additional placement and policy metadata will move here as their legacy C
/// callers are migrated.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LayerState {
    grid_id: GridId,
    placement: CompositorGridPlacement,
    z_index: i32,
    blending: bool,
    disabled: bool,
    accepts_mouse_input: bool,
    publication_pending: bool,
}

impl LayerState {
    /// Creates enabled state for one grid with its requested dimensions.
    const fn new(
        grid_id: GridId,
        placement: CompositorGridPlacement,
        z_index: i32,
        blending: bool,
        accepts_mouse_input: bool,
    ) -> Self {
        Self {
            grid_id,
            placement,
            z_index,
            blending,
            disabled: false,
            accepts_mouse_input,
            publication_pending: true,
        }
    }

    /// Creates state before a grid receives its first placement or resize.
    const fn unplaced(grid_id: GridId) -> Self {
        Self::new(
            grid_id,
            CompositorGridPlacement {
                origin: ComposedPosition::new(0, 0),
                size: GridSize {
                    height: 0,
                    width: 0,
                },
            },
            0,
            false,
            true,
        )
    }
}

/// Safe UI compositor state owned by the runtime context.
///
/// Grid storage and reentrant line-buffer storage remain outside this state so
/// their borrows can be released before synchronous UI callbacks.
struct Compositor {
    layers: Option<Layers<LayerState>>,
    display: CompositorDrawState,
    message: MessageCompositorState,
}

impl Compositor {
    /// Creates compositor state before its default layer is initialized.
    const fn new() -> Self {
        Self {
            layers: None,
            display: CompositorDrawState::new(),
            message: MessageCompositorState::new(),
        }
    }

    /// Initializes the layer collection with its immutable default identity.
    fn initialize_layers(&mut self, default_grid_id: GridId) -> CompositorResult<()> {
        if self.layers.is_some() {
            return Err(CompositorError::InvalidLayerState);
        }
        self.layers = Some(Layers {
            default: LayerState::unplaced(default_grid_id),
            overlays: Vec::new(),
        });
        Ok(())
    }

    /// Releases all compositor layer identities during teardown.
    fn clear_layers(&mut self) -> CompositorResult<()> {
        self.layers
            .take()
            .map(drop)
            .ok_or(CompositorError::InvalidLayerState)
    }

    /// Borrows the initialized compositor layer collection.
    fn layers(&self) -> CompositorResult<&Layers<LayerState>> {
        self.layers
            .as_ref()
            .ok_or(CompositorError::InvalidLayerState)
    }

    /// Returns an owned layer snapshot for one grid-store transaction.
    fn layer_snapshot(&self) -> CompositorResult<Layers<LayerState>> {
        self.layers().cloned()
    }

    /// Resolves a placement candidate against the compositor-owned layers.
    fn grid_placement_target(
        &self,
        grid_id: GridId,
        was_registered: bool,
    ) -> CompositorResult<CompositorGridPlacementTarget> {
        let layers = self.layers()?;
        let layer_index = layers.index_of(grid_id);
        if layer_index.is_some() != was_registered {
            return Err(CompositorError::InvalidLayerState);
        }

        Ok(CompositorGridPlacementTarget {
            grid_id,
            layer_index: layer_index.unwrap_or_else(|| layers.len()),
            was_registered,
        })
    }

    /// Returns the current stack index of one grid identity.
    fn layer_index(&self, grid_id: GridId) -> CompositorResult<Option<usize>> {
        Ok(self.layers()?.index_of(grid_id))
    }

    /// Reports whether one registered layer must publish its stack index.
    fn grid_publication_pending(&self, grid_id: GridId) -> CompositorResult<bool> {
        Ok(self.layers()?.publication_pending(grid_id))
    }

    /// Takes one registered layer's current stack-index publication.
    ///
    /// Taking a publication acknowledges any pending update. A registered
    /// layer can also be taken when no update was pending because callers may
    /// need its current structural index for another position event.
    fn take_grid_publication(
        &mut self,
        grid_id: GridId,
    ) -> CompositorResult<Option<CompositorLayerPublication>> {
        Ok(self
            .layers
            .as_mut()
            .ok_or(CompositorError::InvalidLayerState)?
            .take_publication(grid_id))
    }

    /// Returns the composed-screen origin of one registered grid.
    fn grid_origin(&self, grid_id: GridId) -> CompositorResult<Option<ComposedPosition>> {
        Ok(self
            .layers()?
            .index_of(grid_id)
            .and_then(|layer_index| self.layers.as_ref()?.get(layer_index))
            .map(|layer| layer.placement.origin))
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

    /// Records a composed-screen resize and plans its external effects.
    fn resize(&mut self, size: GridSize) -> CompositorResult<CompositorResizePlan> {
        let buffer_width =
            usize::try_from(size.width).map_err(|_| CompositorError::ScreenWidthOutOfRange)?;
        self.layers
            .as_mut()
            .ok_or(CompositorError::InvalidLayerState)?
            .default
            .placement
            .size = size;
        self.display.screen_size = Some(size);

        Ok(CompositorResizePlan { size, buffer_width })
    }

    /// Updates message-grid placement and returns its required display work.
    fn set_message_position(
        &mut self,
        message_grid_id: GridId,
        default_grid: &ScreenGrid,
        request: CompositorMessagePositionRequest,
    ) -> CompositorResult<CompositorMessagePositionPlan> {
        let should_draw = self.should_draw();
        if let Some(blending) = self.layers.as_ref().and_then(|layers| {
            layers
                .index_of(message_grid_id)
                .and_then(|layer_index| layers.get(layer_index))
                .map(|layer| layer.blending)
        }) {
            self.message.blending = blending;
        }
        let plan = self
            .message
            .update_position(default_grid, request, should_draw)?;
        if let Some(layer) = self.layers.as_mut().and_then(|layers| {
            layers
                .index_of(message_grid_id)
                .and_then(|layer_index| layers.get_mut(layer_index))
        }) {
            layer.placement.origin.row = request.row;
            layer.z_index = request.z_index;
        }
        Ok(plan)
    }

    /// Prepares a new or existing grid placement without publishing external
    /// state.
    fn prepare_grid_placement(
        &mut self,
        grids: &impl GridAccess,
        target: CompositorGridPlacementTarget,
        current_window_layer: Option<usize>,
        request: CompositorGridPlacementRequest,
    ) -> CompositorResult<CompositorGridPlacementPreparation> {
        let should_draw = self.should_draw();
        let mut layers = self.layer_snapshot()?;
        if target.was_registered {
            if target.layer_index == 0 {
                return Err(CompositorError::DefaultLayerMutation);
            }
            let plan = {
                let resolved_layers = layers.layer_stack(grids)?;
                let layer = resolved_layers
                    .layer(target.layer_index)
                    .ok_or(CompositorError::InvalidLayerState)?;
                layer.try_plan_placement(request.placement, request.valid, should_draw)?
            };

            let layer = layers
                .get_mut(target.layer_index)
                .ok_or(CompositorError::InvalidLayerState)?;
            if layer.grid_id != target.grid_id {
                return Err(CompositorError::InvalidLayerState);
            }
            layer.accepts_mouse_input = request.accepts_mouse_input;
            layer.z_index = request.z_index;
            layer.blending = request.blending;
            layer.publication_pending = true;
            let deferred_completion = if should_draw {
                layer.disabled = true;
                Some(CompositorGridPlacementCompletion {
                    grid_id: target.grid_id,
                    placement: request.placement,
                })
            } else {
                layer.placement = request.placement;
                None
            };
            self.layers = Some(layers);
            return Ok(CompositorGridPlacementPreparation {
                plan,
                deferred_completion,
            });
        }

        if target.layer_index != layers.len() {
            return Err(CompositorError::InvalidLayerState);
        }
        let (insertion_index, plan) = {
            let grid = grids
                .grid(target.grid_id)
                .ok_or(CompositorError::InvalidLayerState)?;
            let insertion_index = layers.plan_layer_insertion(
                request.z_index,
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

        layers.try_append(
            target.grid_id,
            request.placement,
            request.z_index,
            request.blending,
            request.accepts_mouse_input,
        )?;
        layers.place_appended_layer(insertion_index, request.placement)?;
        self.layers = Some(layers);
        Ok(CompositorGridPlacementPreparation {
            plan,
            deferred_completion: None,
        })
    }

    /// Completes an existing-grid placement after exposed areas were
    /// recomposed.
    fn finish_grid_placement(
        &mut self,
        completion: CompositorGridPlacementCompletion,
    ) -> CompositorResult<()> {
        let mut layers = self.layer_snapshot()?;
        let layer_index = layers
            .index_of(completion.grid_id)
            .ok_or(CompositorError::InvalidLayerState)?;
        let layer = layers
            .get_mut(layer_index)
            .ok_or(CompositorError::InvalidLayerState)?;
        if layer.grid_id != completion.grid_id {
            return Err(CompositorError::InvalidLayerState);
        }
        layer.placement = completion.placement;
        layer.disabled = false;
        self.layers = Some(layers);
        Ok(())
    }

    /// Updates one registered grid's policy and adjusts its layer position.
    ///
    /// A grid that is not currently registered is accepted as a no-op,
    /// matching the previous C-side `comp_index == 0` check.
    fn update_layer_policy(
        &mut self,
        grid_id: GridId,
        z_index: i32,
        blending: bool,
        accepts_mouse_input: bool,
    ) -> CompositorResult<()> {
        let Some(mut layers) = self.layers.clone() else {
            return Ok(());
        };
        let Some(layer_index) = layers.index_of(grid_id) else {
            return Ok(());
        };
        let previous_z_index = layers
            .get(layer_index)
            .ok_or(CompositorError::InvalidLayerState)?
            .z_index;
        {
            let layer = layers
                .get_mut(layer_index)
                .ok_or(CompositorError::InvalidLayerState)?;
            layer.z_index = z_index;
            layer.blending = blending;
            layer.accepts_mouse_input = accepts_mouse_input;
        }
        let adjustment = if z_index > previous_z_index {
            CompositorLayerAdjustment::Raise
        } else if z_index < previous_z_index {
            CompositorLayerAdjustment::Lower
        } else {
            self.layers = Some(layers);
            return Ok(());
        };
        let movement = layers.plan_layer_adjustment(layer_index, adjustment)?;

        layers.move_layer(movement)?;
        self.layers = Some(layers);
        Ok(())
    }

    /// Removes one non-default grid from the compositor layer stack.
    fn remove_layer(
        &mut self,
        grids: &impl GridAccess,
        layer_index: usize,
    ) -> CompositorResult<CompositorLayerRemoval> {
        if layer_index == 0 {
            return Err(CompositorError::DefaultLayerMutation);
        }

        let mut layers = self.layer_snapshot()?;
        let recompose_area = if self.should_draw() {
            let layer = layers
                .get(layer_index)
                .ok_or(CompositorError::InvalidLayerState)?;
            let grid_id = layer.grid_id;
            let grid = grids
                .grid(grid_id)
                .ok_or(CompositorError::InvalidLayerState)?;
            Some(
                CompositorRecomposeArea::try_from_grid(grid, layer.placement.origin)
                    .ok_or(CompositorError::InvalidLayerState)?,
            )
        } else {
            None
        };

        layers.remove_layer(layer_index)?;
        self.layers = Some(layers);

        Ok(CompositorLayerRemoval { recompose_area })
    }

    /// Finds the topmost non-default compositor grid at one screen point.
    fn grid_at(
        &self,
        grids: &impl GridAccess,
        point: ComposedPosition,
        hit_test: LayerHitTest,
    ) -> CompositorResult<Option<CompositorGridHit>> {
        let layers = self.layers()?.layer_stack(grids)?;
        Ok(layers.topmost_grid_at(point, hit_test))
    }

    /// Reads the cell from the topmost non-default grid at one screen point.
    fn cell_at(
        &self,
        grids: &impl GridAccess,
        point: ComposedPosition,
    ) -> CompositorResult<Option<GridCell>> {
        let Some(hit) = self.grid_at(grids, point, LayerHitTest::Any)? else {
            return Ok(None);
        };
        let grid = grids
            .grid(hit.grid_id)
            .ok_or(CompositorError::InvalidLayerState)?;
        let row = usize::try_from(hit.local_position.row)
            .map_err(|_| CompositorError::InvalidGeometry)?;
        let column = usize::try_from(hit.local_position.column)
            .map_err(|_| CompositorError::InvalidGeometry)?;
        grid.cell(row, column)
            .map(Some)
            .ok_or(CompositorError::InvalidLayerState)
    }

    /// Reports whether a sufficiently higher layer covers a grid's cursor.
    fn is_cursor_covered(
        &self,
        grids: &impl GridAccess,
        current_grid: CompositorCursorGrid,
        point: ComposedPosition,
        minimum_z_index_gap: u32,
    ) -> CompositorResult<bool> {
        if !self.should_draw() {
            return Ok(false);
        }

        let layers = self.layers()?.layer_stack(grids)?;
        Ok(layers.is_cursor_covered_by_higher_layer(point, current_grid, minimum_z_index_gap))
    }

    /// Moves the composed cursor and applies any required layer raise.
    fn move_cursor(
        &mut self,
        grids: &impl GridAccess,
        request: CompositorCursorRequest,
    ) -> CompositorResult<CompositorCursorUpdate> {
        let mut layers = self.layer_snapshot()?;
        let action = {
            let resolved_layers = layers.layer_stack(grids)?;
            resolved_layers.plan_cursor(request)?
        };
        let CompositorCursorAction::Update(plan) = action else {
            return Ok(CompositorCursorUpdate::Ignore);
        };

        let recompose_areas = if let Some(target_index) = plan.raise_to {
            let raise_plan = {
                let resolved_layers = layers.layer_stack(grids)?;
                resolved_layers.plan_layer_raise(plan.layer_index, target_index)?
            };
            layers.move_layer(raise_plan.movement)?;
            raise_plan.recompose_areas
        } else {
            Vec::new()
        };
        self.layers = Some(layers);

        Ok(CompositorCursorUpdate::Apply {
            plan,
            recompose_areas,
        })
    }

    /// Selects the complete safe operation for one raw-line event.
    fn process_raw_line(
        &self,
        grids: &impl GridAccess,
        grid_id: GridId,
        message_layer_index: Option<usize>,
        request: CompositorRawLineRequest,
    ) -> CompositorResult<CompositorRawLineOperation> {
        if !self.should_draw() {
            return Ok(CompositorRawLineOperation::Ignore);
        }

        let layers = self.layers()?.layer_stack(grids)?;
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
        grids: &impl GridAccess,
        grid_id: GridId,
        message_layer_index: Option<usize>,
        request: CompositorScrollRequest,
    ) -> CompositorResult<CompositorScrollOperation> {
        if !self.should_draw() {
            return Ok(CompositorScrollOperation::Ignore);
        }

        let layers = self.layers()?.layer_stack(grids)?;
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
        buffer: &mut CellBuffer,
        grids: &impl GridAccess,
        message_grid: Option<&ScreenGrid>,
        request: &CompositorLineInput,
    ) -> CompositorResult<CompositorSubmission> {
        let layers = self.layers()?.layer_stack(grids)?;
        let message_separator = match self.message.separator_character_at(request.row) {
            Some(character) => {
                let message_grid = message_grid.ok_or(CompositorError::InvalidLayerState)?;
                let layer = layers
                    .layer_index_with_id(message_grid.id())
                    .and_then(|index| layers.layer(index))
                    .unwrap_or_else(|| {
                        CompositorLayer::from_grid(
                            message_grid,
                            0,
                            CompositorGridPlacement {
                                origin: ComposedPosition::new(self.message.current_row, 0),
                                size: message_grid.size(),
                            },
                            self.message.z_index,
                            self.message.blending,
                            false,
                            false,
                        )
                    });
                Some(MessageSeparator {
                    layer,
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
            &layers,
        )
    }

    /// Clamps a recomposition area's exclusive ends to the default grid.
    fn clip_recompose_area(
        &self,
        grids: &impl GridAccess,
        area: CompositorRecomposeArea,
    ) -> CompositorResult<CompositorRecomposeArea> {
        let default_grid_id = self.layers()?.default().grid_id;
        let default_grid = grids
            .grid(default_grid_id)
            .ok_or(CompositorError::InvalidLayerState)?;
        area.with_ends_clamped_to_grid(default_grid)
    }

    /// Returns the screen area occupied by one registered grid.
    fn recompose_area_for_grid(
        &self,
        grids: &impl GridAccess,
        grid_id: GridId,
    ) -> CompositorResult<CompositorRecomposeArea> {
        let layer_index = self
            .layers()?
            .index_of(grid_id)
            .ok_or(CompositorError::InvalidLayerState)?;
        let placement = self
            .layers()?
            .get(layer_index)
            .ok_or(CompositorError::InvalidLayerState)?
            .placement;
        let grid = grids
            .grid(grid_id)
            .ok_or(CompositorError::InvalidLayerState)?;
        CompositorRecomposeArea::try_from_grid(grid, placement.origin)
            .ok_or(CompositorError::InvalidGeometry)
    }

    /// Plans the line requests required to recompose one clipped screen area.
    fn plan_recompose_area(
        &self,
        grids: &impl GridAccess,
        request: CompositorAreaRequest,
    ) -> CompositorResult<CompositorRecomposePlan> {
        Ok(CompositorRecomposePlan {
            area: self.clip_recompose_area(grids, request.area)?,
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
pub(crate) struct CompositorScrollRequest {
    region: GridRegion,
    delta: GridDelta,
}

impl CompositorScrollRequest {
    /// Combines an already validated region with its scroll delta.
    pub(crate) const fn new(region: GridRegion, delta: GridDelta) -> Self {
        Self { region, delta }
    }

    /// Constructs a scroll request with an ordered grid-local region.
    fn try_new(
        top: i32,
        bottom: i32,
        left: i32,
        right: i32,
        row_delta: i32,
        column_delta: i32,
    ) -> Option<Self> {
        Some(Self::new(
            GridRegion::try_new(top..bottom, left..right)?,
            GridDelta::new(row_delta, column_delta),
        ))
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

/// A complete scroll action ready to execute after releasing grid borrows.
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
    accepts_mouse_input: bool,
    disabled: bool,
}

impl<'grid> CompositorLayer<'grid> {
    /// Constructs a layer view from grid and compositor-owned state.
    fn from_grid(
        grid: &'grid ScreenGrid,
        layer_index: usize,
        placement: CompositorGridPlacement,
        z_index: i32,
        blending: bool,
        accepts_mouse_input: bool,
        disabled: bool,
    ) -> Self {
        let allocated_size = grid.size();
        let visible_size = GridSize {
            height: allocated_size.height.min(placement.size.height),
            width: allocated_size.width.min(placement.size.width),
        };

        Self {
            grid,
            origin: placement.origin,
            requested_size: placement.size,
            visible_size,
            z_index,
            layer_index,
            blending,
            accepts_mouse_input,
            disabled,
        }
    }

    /// Returns the allocated dimensions of this layer's grid.
    fn allocated_size(self) -> GridSize {
        self.grid.size()
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
        let size = self.grid.size();
        let other_size = other.grid.size();
        let bottom = self
            .origin
            .row
            .checked_add(size.height)
            .ok_or(CompositorError::InvalidGeometry)?
            .min(
                other
                    .origin
                    .row
                    .checked_add(other_size.height)
                    .ok_or(CompositorError::InvalidGeometry)?,
            );
        let right = self
            .origin
            .column
            .checked_add(size.width)
            .ok_or(CompositorError::InvalidGeometry)?
            .min(
                other
                    .origin
                    .column
                    .checked_add(other_size.width)
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

    /// Reports whether this layer is at least `minimum_gap` above a z-index.
    fn is_at_least_z_index_above(self, lower_z_index: i32, minimum_gap: u32) -> bool {
        i64::from(self.z_index) - i64::from(lower_z_index) >= i64::from(minimum_gap)
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
    fn allows_wrap(self, is_default: bool, screen_columns: usize) -> bool {
        is_default
            || (self.origin.column == 0
                && usize::try_from(self.grid.size().width).ok() == Some(screen_columns))
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
    /// Reports whether one compositor layer participates in this hit test.
    fn accepts(self, layer: CompositorLayer<'_>) -> bool {
        match self {
            Self::Any => true,
            Self::MouseEnabled => layer.accepts_mouse_input,
        }
    }
}

/// Provides safe grid borrowing by stable Rust grid identity.
trait GridAccess {
    /// Borrows one grid by identity.
    fn grid(&self, grid_id: GridId) -> Option<&ScreenGrid>;
}

impl GridAccess for GridStore {
    fn grid(&self, grid_id: GridId) -> Option<&ScreenGrid> {
        GridStore::grid(self, grid_id).ok()
    }
}

impl GridAccess for MutexGuard<'_, GridStore> {
    fn grid(&self, grid_id: GridId) -> Option<&ScreenGrid> {
        GridStore::grid(self, grid_id).ok()
    }
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

/// One registered layer's structural index for external publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorLayerPublication {
    layer_index: usize,
}

/// A compositor layer collection with a structurally separate default layer.
///
/// The default layer always exists and can never be reordered with or removed
/// from the overlay collection.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Layers<T> {
    default: T,
    overlays: Vec<T>,
}

impl<T> Layers<T> {
    /// Returns the number of layers, including the default layer.
    fn len(&self) -> usize {
        1 + self.overlays.len()
    }

    /// Returns the default layer.
    fn default(&self) -> &T {
        &self.default
    }

    /// Returns the ordered overlay layers.
    fn overlays(&self) -> &[T] {
        &self.overlays
    }

    /// Returns one layer by its complete stack index.
    fn get(&self, index: usize) -> Option<&T> {
        if index == 0 {
            Some(&self.default)
        } else {
            self.overlays.get(index - 1)
        }
    }

    /// Mutably borrows one layer by its complete stack index.
    fn get_mut(&mut self, index: usize) -> Option<&mut T> {
        if index == 0 {
            Some(&mut self.default)
        } else {
            self.overlays.get_mut(index - 1)
        }
    }

    /// Iterates over the default layer followed by every overlay.
    fn iter(&self) -> impl Iterator<Item = &T> {
        std::iter::once(&self.default).chain(&self.overlays)
    }

    /// Mutably iterates over the default layer followed by every overlay.
    fn iter_mut(&mut self) -> impl Iterator<Item = &mut T> {
        std::iter::once(&mut self.default).chain(&mut self.overlays)
    }
}

impl Layers<LayerState> {
    /// Creates enabled layers from nonempty, uniquely ordered grid identities.
    fn try_new(layer_ids: Vec<GridId>) -> Option<Self> {
        let mut layer_ids = layer_ids.into_iter();
        let default_grid_id = layer_ids.next()?;
        let overlay_grid_ids = layer_ids.collect::<Vec<_>>();
        if overlay_grid_ids.iter().enumerate().any(|(index, grid_id)| {
            *grid_id == default_grid_id || overlay_grid_ids[..index].contains(grid_id)
        }) {
            return None;
        }

        Some(Self {
            default: LayerState::unplaced(default_grid_id),
            overlays: overlay_grid_ids
                .into_iter()
                .map(LayerState::unplaced)
                .collect(),
        })
    }

    /// Returns one identity by its layer index.
    fn layer_id(&self, index: usize) -> Option<GridId> {
        self.get(index).map(|layer| layer.grid_id)
    }

    /// Returns the layer index of one registered grid.
    fn index_of(&self, grid_id: GridId) -> Option<usize> {
        if self.default.grid_id == grid_id {
            return Some(0);
        }

        self.overlays
            .iter()
            .position(|candidate| candidate.grid_id == grid_id)
            .and_then(|index| index.checked_add(1))
    }

    /// Reports whether one registered layer must publish its stack index.
    fn publication_pending(&self, grid_id: GridId) -> bool {
        self.index_of(grid_id)
            .and_then(|layer_index| self.get(layer_index))
            .is_some_and(|layer| layer.publication_pending)
    }

    /// Takes a registered layer's current stack-index publication.
    fn take_publication(&mut self, grid_id: GridId) -> Option<CompositorLayerPublication> {
        let layer_index = self.index_of(grid_id)?;
        let layer = self.get_mut(layer_index)?;
        layer.publication_pending = false;
        Some(CompositorLayerPublication { layer_index })
    }

    /// Schedules publication for every layer in an inclusive index range.
    fn schedule_publications(
        &mut self,
        affected_indices: std::ops::RangeInclusive<usize>,
    ) -> CompositorResult<()> {
        for layer_index in affected_indices {
            self.get_mut(layer_index)
                .ok_or(CompositorError::InvalidLayerState)?
                .publication_pending = true;
        }
        Ok(())
    }

    /// Selects the stable insertion index for one new layer.
    fn plan_layer_insertion(
        &self,
        z_index: i32,
        current_window_layer: Option<usize>,
        on_top: bool,
    ) -> CompositorResult<usize> {
        if self.default.z_index > z_index {
            return Err(CompositorError::InvalidLayerState);
        }

        let mut insertion_index = self.len();
        while insertion_index > 1
            && self
                .get(insertion_index - 1)
                .ok_or(CompositorError::InvalidLayerState)?
                .z_index
                > z_index
        {
            insertion_index -= 1;
        }

        if !on_top && insertion_index > 1 {
            let preceding_index = insertion_index - 1;
            let preceding_layer = self
                .get(preceding_index)
                .ok_or(CompositorError::InvalidLayerState)?;
            if current_window_layer == Some(preceding_index) && preceding_layer.z_index == z_index {
                insertion_index = preceding_index;
            }
        }

        Ok(insertion_index)
    }

    /// Plans a stable relocation after one layer's z-index changes.
    fn plan_layer_adjustment(
        &self,
        layer_index: usize,
        adjustment: CompositorLayerAdjustment,
    ) -> CompositorResult<CompositorLayerMove> {
        if layer_index == 0 {
            return Err(CompositorError::DefaultLayerMutation);
        }
        let layer = self
            .get(layer_index)
            .ok_or(CompositorError::InvalidLayerState)?;

        let mut target_index = layer_index;
        match adjustment {
            CompositorLayerAdjustment::Raise => {
                while target_index + 1 < self.len()
                    && layer.z_index
                        > self
                            .get(target_index + 1)
                            .ok_or(CompositorError::InvalidLayerState)?
                            .z_index
                {
                    target_index += 1;
                }
            }
            CompositorLayerAdjustment::Lower => {
                while target_index > 1
                    && layer.z_index
                        < self
                            .get(target_index - 1)
                            .ok_or(CompositorError::InvalidLayerState)?
                            .z_index
                {
                    target_index -= 1;
                }
            }
        }

        Ok(CompositorLayerMove {
            source_index: layer_index,
            target_index,
        })
    }

    /// Appends one previously unregistered grid identity.
    fn try_append(
        &mut self,
        grid_id: GridId,
        placement: CompositorGridPlacement,
        z_index: i32,
        blending: bool,
        accepts_mouse_input: bool,
    ) -> CompositorResult<()> {
        if self.index_of(grid_id).is_some() {
            return Err(CompositorError::InvalidLayerState);
        }

        self.overlays.push(LayerState::new(
            grid_id,
            placement,
            z_index,
            blending,
            accepts_mouse_input,
        ));
        Ok(())
    }

    /// Removes one non-default grid identity.
    fn remove_non_default(&mut self, layer_index: usize) -> CompositorResult<GridId> {
        let overlay_index = layer_index
            .checked_sub(1)
            .ok_or(CompositorError::DefaultLayerMutation)?;
        self.overlays
            .get(overlay_index)
            .ok_or(CompositorError::InvalidLayerState)?;

        Ok(self.overlays.remove(overlay_index).grid_id)
    }

    /// Applies one validated layer relocation.
    fn apply_layer_move(&mut self, movement: CompositorLayerMove) -> CompositorResult<()> {
        let source_index = movement
            .source_index
            .checked_sub(1)
            .ok_or(CompositorError::DefaultLayerMutation)?;
        let target_index = movement
            .target_index
            .checked_sub(1)
            .ok_or(CompositorError::DefaultLayerMutation)?;
        CompositorLayerMove {
            source_index,
            target_index,
        }
        .apply_to(&mut self.overlays)
    }

    /// Borrows every registered grid as a validated compositor layer stack.
    fn layer_stack<'grid>(
        &self,
        grids: &'grid impl GridAccess,
    ) -> CompositorResult<Layers<CompositorLayer<'grid>>> {
        let mut resolved_layers = self
            .iter()
            .enumerate()
            .map(|(layer_index, layer)| {
                let grid = grids.grid(layer.grid_id)?;
                (grid.id() == layer.grid_id).then(|| {
                    CompositorLayer::from_grid(
                        grid,
                        layer_index,
                        layer.placement,
                        layer.z_index,
                        layer.blending,
                        layer.accepts_mouse_input,
                        layer.disabled,
                    )
                })
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(CompositorError::InvalidLayerState)?
            .into_iter();
        let default = resolved_layers
            .next()
            .ok_or(CompositorError::InvalidLayerState)?;
        let layers = Layers {
            default,
            overlays: resolved_layers.collect(),
        };

        Ok(layers)
    }

    /// Places the appended grid at its planned compositor-stack index.
    fn place_appended_layer(
        &mut self,
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

        self.move_layer(CompositorLayerMove {
            source_index: appended_index,
            target_index: insertion_index,
        })?;
        let grid_id = self
            .layer_id(insertion_index)
            .ok_or(CompositorError::InvalidLayerState)?;
        let layer = self
            .get_mut(insertion_index)
            .ok_or(CompositorError::InvalidLayerState)?;
        if layer.grid_id != grid_id {
            return Err(CompositorError::InvalidLayerState);
        }
        layer.placement = placement;
        Ok(())
    }

    /// Applies a relocation and schedules affected layer publications.
    fn move_layer(&mut self, movement: CompositorLayerMove) -> CompositorResult<()> {
        self.apply_layer_move(movement)?;
        self.schedule_publications(movement.affected_indices())
    }

    /// Removes one non-default layer and schedules shifted publications.
    fn remove_layer(&mut self, layer_index: usize) -> CompositorResult<()> {
        self.remove_non_default(layer_index)?;
        if layer_index < self.len() {
            self.schedule_publications(layer_index..=self.len() - 1)?;
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

/// Validated inputs for checking coverage above one compositor layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorCoveredAboveQuery {
    rectangle: CompositorRect,
    current_layer_index: usize,
    message_layer_index: Option<usize>,
    message_was_scrolled: bool,
}

impl<'grid> Layers<CompositorLayer<'grid>> {
    /// Returns the default grid at the bottom of the stack.
    fn default_layer(&self) -> CompositorLayer<'grid> {
        self.default
    }

    /// Returns one layer by its stack index.
    fn layer(&self, index: usize) -> Option<CompositorLayer<'grid>> {
        self.get(index).copied()
    }

    /// Finds the compositor layer identified by one native grid identity.
    fn layer_index_with_id(&self, grid_id: GridId) -> Option<usize> {
        self.iter().position(|layer| layer.grid.id() == grid_id)
    }

    /// Selects a grid and plans its composed-screen cursor update.
    fn plan_cursor(
        &self,
        request: CompositorCursorRequest,
    ) -> CompositorResult<CompositorCursorAction> {
        let screen_size = self.default_layer().allocated_size();
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

        let raise_to = if layer_index == 0 {
            None
        } else {
            let mut target_index = self
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
    fn topmost_grid_at(
        &self,
        point: ComposedPosition,
        hit_test: LayerHitTest,
    ) -> Option<CompositorGridHit> {
        self.overlays.iter().rev().copied().find_map(|layer| {
            hit_test
                .accepts(layer)
                .then(|| layer.positioned_grid().hit(point))
                .flatten()
        })
    }

    /// Reports whether a sufficiently higher layer covers `current_grid`'s cursor.
    fn is_cursor_covered_by_higher_layer(
        &self,
        point: ComposedPosition,
        current_grid: CompositorCursorGrid,
        minimum_z_index_gap: u32,
    ) -> bool {
        let current_z_index = self
            .layer_index_with_id(current_grid.grid_id)
            .and_then(|index| self.layer(index))
            .map_or(current_grid.z_index, |layer| layer.z_index);

        self.topmost_grid_at(point, LayerHitTest::Any)
            .and_then(|hit| self.layer_index_with_id(hit.grid_id))
            .and_then(|index| self.layer(index))
            .is_some_and(|top_layer| {
                top_layer.grid.id() != current_grid.grid_id
                    && top_layer.is_at_least_z_index_above(current_z_index, minimum_z_index_gap)
            })
    }

    /// Plans an explicit upward move and recomposition of crossed layers.
    fn plan_layer_raise(
        &self,
        layer_index: usize,
        target_index: usize,
    ) -> CompositorResult<CompositorLayerRaisePlan> {
        if layer_index == 0 {
            return Err(CompositorError::DefaultLayerMutation);
        }
        let layer = self
            .layer(layer_index)
            .ok_or(CompositorError::InvalidLayerState)?;
        if target_index < layer_index || target_index >= self.len() {
            return Err(CompositorError::InvalidLayerState);
        }

        let recompose_areas = if target_index == layer_index {
            Vec::new()
        } else {
            self.overlays
                .get(layer_index..target_index)
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
        self.layer(query.current_layer_index)
            .ok_or(CompositorError::InvalidLayerState)?;
        let layers_above = self
            .overlays
            .get(query.current_layer_index..)
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
        let default_layer = self.default_layer();
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
        let flags = if current_layer_index == 0 {
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

        let screen_size = default_layer.grid.size();
        if row >= screen_size.height {
            plan.action = CompositorRawLineAction::DiscardRow;
            return Ok(plan);
        }

        let screen_columns = screen_size.width;
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

        for (index, layer) in self.iter().copied().enumerate() {
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
pub(crate) struct CompositorRuntime {
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

    /// Returns the composed-screen dimensions captured for this event.
    pub(crate) const fn screen_size(self) -> GridSize {
        self.screen_size
    }
}

/// Emits one formatted raw-line diagnostic through Neovim's debug logger.
fn log_raw_line_diagnostic(
    diagnostic: RawLineDiagnostic,
    grid_id: GridId,
    output: &mut impl UiOutput<ComposedGridSpace>,
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

/// Validated inputs for composing one line from registered grids.
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

/// Compositor-specific operations on reusable cell storage.
impl CellBuffer {
    /// Selects and composes every layer segment for one complete line.
    fn try_compose_line(
        &mut self,
        request: CompositorLineRequest<'_>,
        layers: &Layers<CompositorLayer<'_>>,
    ) -> CompositorResult<CompositorSubmission> {
        let mut line =
            self.prepare_line(request.start_column, request.end_column, request.flags)?;
        let line_end = line.columns.end;
        let default_grid_id = layers.default_layer().grid.id();
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

        let last_layer = last_layer.ok_or(CompositorError::InvalidLineState)?;
        let allow_wrap = last_layer.allows_wrap(
            last_layer.grid.id() == default_grid_id,
            request.screen_columns,
        );
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

/// Acquires exclusive access to the context-owned compositor state.
fn lock_compositor() -> MutexGuard<'static, Compositor> {
    registered_context().compositor().lock_state()
}

/// Acquires exclusive access to the context-owned compositor buffer pool.
fn lock_compositor_buffer_pool() -> MutexGuard<'static, CellBufferPool> {
    registered_context().compositor().lock_buffer_pool()
}

/// Emits one clear raw-line event used for compositor visualization.
fn emit_compositor_debug_line(
    row: i32,
    columns: Range<i32>,
    attribute: VimSAttrT,
    output: &mut impl UiOutput<ComposedGridSpace>,
) -> CompositorResult<()> {
    let start_column =
        usize::try_from(columns.start).map_err(|_| CompositorError::InvalidGeometry)?;
    let end_column = usize::try_from(columns.end).map_err(|_| CompositorError::InvalidGeometry)?;

    let buffer_width = lock_compositor_buffer_pool().width();
    if start_column > end_column || end_column > buffer_width {
        return Err(CompositorError::InvalidLineState);
    }

    let command = BorrowedRawLineCommand::try_new(
        row,
        columns.start..columns.start,
        columns.end,
        attribute,
        LineFlags::default(),
        &[],
        &[],
    )
    .ok_or(CompositorError::InvalidLineState)?;
    output.emit_raw_line(command);
    Ok(())
}

/// Reports whether a screen character participates in negative-space blending.
fn is_negative_space(character: VimSCharT) -> bool {
    character == schar_from_ascii(' ') || character == schar_from_char(0x2800)
}

#[cfg(test)]
mod tests {
    use super::ffi::update_registered_layer_policy;
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicI32, Ordering},
    };

    use crate::{grid::LineBuffer, ui::output::CellBufferLease};

    static NEXT_TEST_GRID_ID: AtomicI32 = AtomicI32::new(10_000);

    fn assign_test_grid_id(mut grid: ScreenGrid) -> ScreenGrid {
        grid.set_id_for_test(GridId::new(
            NEXT_TEST_GRID_ID.fetch_add(1, Ordering::Relaxed),
        ));
        grid
    }

    static COMPOSITOR_LAYERS_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Serializes tests which install process-global compositor layers.
    struct TestCompositorLayers {
        _lock: MutexGuard<'static, ()>,
    }

    impl Drop for TestCompositorLayers {
        fn drop(&mut self) {
            *lock_compositor() = Compositor::new();
        }
    }

    /// Installs the supplied owned grids and layer order for one test.
    fn register_test_compositor_layers(
        grids: Vec<ScreenGrid>,
    ) -> (GridStore, TestCompositorLayers) {
        let test_lock = COMPOSITOR_LAYERS_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let layer_ids = grids.iter().map(ScreenGrid::id).collect();
        let mut grid_store = GridStore::new();
        for grid in grids {
            grid_store
                .register(grid)
                .expect("test grid identities must be unique");
        }
        let mut layers = Layers::try_new(layer_ids).expect("test compositor layers must be valid");
        for layer in layers.iter_mut() {
            let grid = grid_store
                .grid(layer.grid_id)
                .expect("test layer grid must be registered");
            layer.placement.size = grid.size();
        }

        let mut compositor = lock_compositor();
        assert!(compositor.layers.is_none());
        compositor.layers = Some(layers);

        (grid_store, TestCompositorLayers { _lock: test_lock })
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
            compositor.attach();
        }

        TestAttachedCompositor
    }

    /// Returns the compositor-owned layer identities for assertions.
    fn compositor_layer_ids() -> Vec<GridId> {
        lock_compositor()
            .layers()
            .unwrap()
            .iter()
            .map(|layer| layer.grid_id)
            .collect()
    }

    /// Acknowledges every publication in the installed test compositor.
    fn clear_registered_test_layer_publications() {
        let grid_ids = compositor_layer_ids();
        let mut compositor = lock_compositor();
        for grid_id in grid_ids {
            assert!(compositor.take_grid_publication(grid_id).unwrap().is_some());
        }
    }

    /// Takes one publication from the installed test compositor.
    fn take_registered_test_layer_publication(
        grid_id: GridId,
    ) -> Option<CompositorLayerPublication> {
        lock_compositor().take_grid_publication(grid_id).unwrap()
    }

    /// Assigns one screen origin in the installed test compositor.
    fn set_registered_test_layer_origin(grid_id: GridId, origin: ComposedPosition) {
        let mut compositor = lock_compositor();
        let layer_index = compositor.layer_index(grid_id).unwrap().unwrap();
        compositor
            .layers
            .as_mut()
            .unwrap()
            .get_mut(layer_index)
            .unwrap()
            .placement
            .origin = origin;
    }

    /// Assigns one z-index in the installed test compositor.
    fn set_registered_test_layer_z_index(grid_id: GridId, z_index: i32) {
        let mut compositor = lock_compositor();
        let layer_index = compositor.layer_index(grid_id).unwrap().unwrap();
        compositor
            .layers
            .as_mut()
            .unwrap()
            .get_mut(layer_index)
            .unwrap()
            .z_index = z_index;
    }

    /// Sets mouse-input policy for one layer in the installed test compositor.
    fn set_registered_test_layer_mouse_input(grid_id: GridId, accepts_mouse_input: bool) {
        let mut compositor = lock_compositor();
        let layer_index = compositor.layer_index(grid_id).unwrap().unwrap();
        compositor
            .layers
            .as_mut()
            .unwrap()
            .get_mut(layer_index)
            .unwrap()
            .accepts_mouse_input = accepts_mouse_input;
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
        for (column, (&character, &attribute)) in characters.iter().zip(attributes).enumerate() {
            assert!(grid.set_cell_for_test(0, column, GridCell::new(character, attribute, -1),));
        }
        assign_test_grid_id(grid)
    }

    fn grid_with_size(rows: i32, columns: i32) -> ScreenGrid {
        let mut line_buffer = LineBuffer::new(columns as usize);
        assign_test_grid_id(ScreenGrid::new(rows, columns, true, &mut line_buffer))
    }

    /// Creates zero-origin compositor state for one test grid.
    fn test_layer_state(grid: &ScreenGrid) -> LayerState {
        LayerState::new(
            grid.id(),
            CompositorGridPlacement {
                origin: ComposedPosition::new(0, 0),
                size: grid.size(),
            },
            0,
            false,
            true,
        )
    }

    #[test]
    fn layers_separate_the_default_from_unique_overlays() {
        let first = GridId::new(1);
        let second = GridId::new(2);
        let third = GridId::new(3);

        assert!(Layers::<LayerState>::try_new(Vec::new()).is_none());
        assert!(Layers::<LayerState>::try_new(vec![first, first]).is_none());
        assert!(Layers::<LayerState>::try_new(vec![first, second, second]).is_none());

        let mut layers = Layers::<LayerState>::try_new(vec![first, second, third]).unwrap();
        assert_eq!(layers.default().grid_id, first);
        assert_eq!(
            layers.default().placement,
            CompositorGridPlacement {
                origin: ComposedPosition::new(0, 0),
                size: GridSize {
                    height: 0,
                    width: 0,
                },
            }
        );
        assert!(!layers.default().disabled);
        assert_eq!(
            layers
                .overlays()
                .iter()
                .map(|layer| layer.grid_id)
                .collect::<Vec<_>>(),
            [second, third]
        );
        assert_eq!(layers.len(), 3);
        assert_eq!(layers.index_of(second), Some(1));

        layers
            .apply_layer_move(CompositorLayerMove {
                source_index: 2,
                target_index: 1,
            })
            .unwrap();
        assert_eq!(
            layers.iter().map(|layer| layer.grid_id).collect::<Vec<_>>(),
            [first, third, second]
        );
        assert_eq!(
            layers.apply_layer_move(CompositorLayerMove {
                source_index: 1,
                target_index: 0,
            }),
            Err(CompositorError::DefaultLayerMutation)
        );
        assert_eq!(
            layers.remove_non_default(0),
            Err(CompositorError::DefaultLayerMutation)
        );
        assert_eq!(layers.remove_non_default(1), Ok(third));
        assert_eq!(
            layers.iter().map(|layer| layer.grid_id).collect::<Vec<_>>(),
            [first, second]
        );
    }

    fn compositor_layer(
        grid: &ScreenGrid,
        origin_column: i32,
        visible_width: i32,
        layer_index: usize,
        disabled: bool,
    ) -> CompositorLayer<'_> {
        let grid_height = grid.size().height;
        CompositorLayer {
            grid,
            origin: ComposedPosition::new(0, origin_column),
            requested_size: GridSize {
                height: grid_height,
                width: visible_width,
            },
            visible_size: GridSize {
                height: grid_height,
                width: visible_width,
            },
            z_index: 0,
            layer_index,
            blending: false,
            accepts_mouse_input: true,
            disabled,
        }
    }

    fn compositor_layers<'grid>(
        default: CompositorLayer<'grid>,
        overlays: Vec<CompositorLayer<'grid>>,
    ) -> Layers<CompositorLayer<'grid>> {
        Layers { default, overlays }
    }

    fn message_position_request(row: i32, scrolled: bool) -> CompositorMessagePositionRequest {
        CompositorMessagePositionRequest {
            row,
            z_index: 0,
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
        compositor.initialize_layers(GridId::new(1)).unwrap();
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
        assert_eq!(compositor.layers().unwrap().default().placement.size, size);
    }

    #[test]
    fn message_position_updates_without_a_registered_message_layer() {
        let mut compositor = Compositor::new();
        let default_grid = grid_with_size(10, 18);
        compositor.layers = Some(Layers {
            default: test_layer_state(&default_grid),
            overlays: Vec::new(),
        });

        assert_eq!(
            compositor.set_message_position(
                GridId::new(0),
                &default_grid,
                message_position_request(5, false),
            ),
            Ok(CompositorMessagePositionPlan::default())
        );
        assert_eq!(compositor.message.current_row, 5);
        assert_eq!(compositor.layers().unwrap().len(), 1);
    }

    #[test]
    fn moving_message_grid_down_recomposes_the_exposed_rows() {
        let mut compositor = Compositor::new();
        let message_grid = grid_with_size(10, 20);
        let default_grid = grid_with_size(10, 18);
        compositor.layers = Some(Layers {
            default: test_layer_state(&default_grid),
            overlays: vec![test_layer_state(&message_grid)],
        });
        assert_eq!(
            compositor.set_message_position(
                message_grid.id(),
                &default_grid,
                message_position_request(5, false),
            ),
            Ok(CompositorMessagePositionPlan::default())
        );

        compositor.attach();
        let plan = compositor
            .set_message_position(
                message_grid.id(),
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
        assert_eq!(
            compositor
                .layers()
                .unwrap()
                .get(1)
                .unwrap()
                .placement
                .origin,
            ComposedPosition::new(7, 0)
        );
    }

    #[test]
    fn moving_message_grid_up_scrolls_text_and_recomposes_a_new_separator() {
        let mut compositor = Compositor::new();
        let message_grid = grid_with_size(10, 20);
        let default_grid = grid_with_size(10, 18);
        compositor.layers = Some(Layers {
            default: test_layer_state(&default_grid),
            overlays: vec![test_layer_state(&message_grid)],
        });
        compositor
            .set_message_position(
                message_grid.id(),
                &default_grid,
                message_position_request(5, false),
            )
            .unwrap();

        compositor.attach();
        let plan = compositor
            .set_message_position(
                message_grid.id(),
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
    }

    #[test]
    fn moving_a_blended_message_grid_up_recomposes_instead_of_scrolling() {
        let mut state = MessageCompositorState::new();
        let default_grid = grid_with_size(10, 18);
        state.blending = true;
        state
            .update_position(&default_grid, message_position_request(5, false), false)
            .unwrap();

        let plan = state
            .plan_redraw(message_position_request(2, true), &default_grid, true)
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
        let grid = grid_with_size(3, 4);

        assert_eq!(
            CompositorRecomposeArea::try_from_grid(&grid, ComposedPosition::new(2, 5)),
            Some(CompositorRecomposeArea {
                rows: 2..5,
                columns: 5..9,
            })
        );
    }

    #[test]
    fn grid_recomposition_area_rejects_overflowing_edges() {
        let grid = grid_with_size(1, 1);

        assert_eq!(
            CompositorRecomposeArea::try_from_grid(
                &grid,
                ComposedPosition::new(i32::MAX, i32::MAX),
            ),
            None
        );
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
        let grid = grid_with_size(3, 4);
        let grid_id = grid.id();
        let (grid_store, _registry) = register_test_compositor_layers(vec![grid]);
        set_registered_test_layer_origin(grid_id, ComposedPosition::new(2, 5));
        let compositor = lock_compositor();

        assert_eq!(
            compositor.recompose_area_for_grid(&grid_store, grid_id),
            Ok(CompositorRecomposeArea {
                rows: 2..5,
                columns: 5..9,
            })
        );
        assert_eq!(
            compositor.clip_recompose_area(
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

    #[test]
    fn layer_adjustment_raises_across_strictly_lower_z_indices() {
        let default_grid = grid_with_size(1, 1);
        let moving_grid = grid_with_size(1, 1);
        let lower_grid = grid_with_size(1, 1);
        let equal_grid = grid_with_size(1, 1);
        let mut moving_layer = test_layer_state(&moving_grid);
        moving_layer.z_index = 30;
        let mut lower_layer = test_layer_state(&lower_grid);
        lower_layer.z_index = 20;
        let mut equal_layer = test_layer_state(&equal_grid);
        equal_layer.z_index = 30;
        let layers = Layers {
            default: test_layer_state(&default_grid),
            overlays: vec![moving_layer, lower_layer, equal_layer],
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
    fn registered_layer_policy_update_updates_rust_state_and_publication() {
        let default_grid = grid_with_size(1, 1);
        let moving_grid = grid_with_size(1, 1);
        let lower_grid = grid_with_size(1, 1);
        let equal_grid = grid_with_size(1, 1);
        let moving_grid_id = moving_grid.id();
        let lower_grid_id = lower_grid.id();
        let equal_grid_id = equal_grid.id();
        let expected_registry = [
            default_grid.id(),
            lower_grid_id,
            moving_grid_id,
            equal_grid_id,
        ];
        let (_grid_store, _registry) = register_test_compositor_layers(vec![
            default_grid,
            moving_grid,
            lower_grid,
            equal_grid,
        ]);
        set_registered_test_layer_z_index(lower_grid_id, 20);
        set_registered_test_layer_z_index(equal_grid_id, 30);

        clear_registered_test_layer_publications();
        assert_eq!(
            update_registered_layer_policy(moving_grid_id, 30, true, false),
            Ok(())
        );
        assert_eq!(compositor_layer_ids(), expected_registry);
        {
            let compositor = lock_compositor();
            let layers = compositor.layers().unwrap();
            let moving_layer = layers
                .get(layers.index_of(moving_grid_id).unwrap())
                .unwrap();
            assert!(moving_layer.blending);
            assert!(!moving_layer.accepts_mouse_input);
        }
        assert_eq!(
            take_registered_test_layer_publication(lower_grid_id),
            Some(CompositorLayerPublication { layer_index: 1 })
        );
        assert_eq!(
            take_registered_test_layer_publication(moving_grid_id),
            Some(CompositorLayerPublication { layer_index: 2 })
        );
        assert!(
            !lock_compositor()
                .grid_publication_pending(equal_grid_id)
                .unwrap()
        );
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
        let mut current_layer = test_layer_state(&current_grid);
        current_layer.z_index = 10;
        let mut upper_layer = test_layer_state(&upper_grid);
        upper_layer.z_index = 20;
        let layers = Layers {
            default: test_layer_state(&default_grid),
            overlays: vec![current_layer, upper_layer],
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
        selected_grid.set_id_for_test(GridId::new(7));
        let upper_grid = grid_with_size(2, 2);
        let mut selected_layer = compositor_layer(&selected_grid, 3, 2, 1, false);
        selected_layer.origin.row = 2;
        selected_layer.z_index = 20;
        let mut upper_layer = compositor_layer(&upper_grid, 0, 2, 2, false);
        upper_layer.z_index = 10;
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 12, 0, false),
            vec![selected_layer, upper_layer],
        );

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
    fn registered_cursor_update_raises_the_layer() {
        let default_grid = grid_with_size(10, 12);
        let default_grid_id = default_grid.id();
        let mut selected_grid = grid_with_size(2, 2);
        selected_grid.set_id_for_test(GridId::new(7));
        let upper_grid = grid_with_size(2, 2);
        let selected_grid_id = selected_grid.id();
        let upper_grid_id = upper_grid.id();

        let (grid_store, _registry) =
            register_test_compositor_layers(vec![default_grid, selected_grid, upper_grid]);
        set_registered_test_layer_origin(selected_grid_id, ComposedPosition::new(2, 3));
        set_registered_test_layer_origin(upper_grid_id, ComposedPosition::new(2, 3));
        clear_registered_test_layer_publications();

        assert_eq!(
            lock_compositor().move_cursor(
                &grid_store,
                CompositorCursorRequest {
                    grid_id: GridId::new(99),
                    local_position: GridPosition::new(0, 0),
                },
            ),
            Ok(CompositorCursorUpdate::Ignore)
        );
        // The cursor lies outside the default grid, so production code will
        // recompose the exposed area before suppressing cursor output.
        assert_eq!(
            lock_compositor().move_cursor(
                &grid_store,
                CompositorCursorRequest {
                    grid_id: GridId::new(7),
                    local_position: GridPosition::new(10, 0),
                },
            ),
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
        assert_eq!(
            compositor_layer_ids(),
            [default_grid_id, upper_grid_id, selected_grid_id]
        );
        assert_eq!(
            take_registered_test_layer_publication(upper_grid_id),
            Some(CompositorLayerPublication { layer_index: 1 })
        );
        assert_eq!(
            take_registered_test_layer_publication(selected_grid_id),
            Some(CompositorLayerPublication { layer_index: 2 })
        );
    }

    #[test]
    fn layer_insertion_never_displaces_default_layer() {
        let default_grid = grid_with_size(1, 1);
        let layers = Layers {
            default: test_layer_state(&default_grid),
            overlays: Vec::new(),
        };

        assert_eq!(layers.plan_layer_insertion(0, Some(0), false), Ok(1));
    }

    #[test]
    fn registered_layers_place_existing_grid() {
        let default_grid = grid_with_size(4, 4);
        let moving_grid = grid_with_size(2, 2);
        let moving_grid_id = moving_grid.id();
        let mut recomposed_areas = Vec::new();

        let (grid_store, _registry) =
            register_test_compositor_layers(vec![default_grid, moving_grid]);
        let _compositor = attach_test_compositor();
        clear_registered_test_layer_publications();

        let target = lock_compositor()
            .grid_placement_target(moving_grid_id, true)
            .unwrap();
        assert_eq!(
            registered_context()
                .compositor()
                .place_grid_in_registered_layers(
                    grid_store,
                    target,
                    None,
                    CompositorGridPlacementRequest {
                        placement: CompositorGridPlacement::try_new(1, 2, 2, 2).unwrap(),
                        z_index: 0,
                        blending: false,
                        valid: true,
                        on_top: false,
                        accepts_mouse_input: true,
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
        assert_eq!(
            take_registered_test_layer_publication(moving_grid_id),
            Some(CompositorLayerPublication { layer_index: 1 })
        );
        assert_eq!(
            lock_compositor()
                .layers()
                .unwrap()
                .get(1)
                .unwrap()
                .placement,
            CompositorGridPlacement::try_new(1, 2, 2, 2).unwrap()
        );
        assert!(!lock_compositor().layers().unwrap().get(1).unwrap().disabled);
    }

    #[test]
    fn registered_layers_place_appended_grid() {
        let default_grid = grid_with_size(4, 4);
        let current_grid = grid_with_size(2, 2);
        let appended_grid = grid_with_size(2, 2);
        let current_grid_id = current_grid.id();
        let appended_grid_id = appended_grid.id();
        let expected_registry = [default_grid.id(), appended_grid_id, current_grid_id];
        let mut recomposed_areas = Vec::new();

        let (mut grid_store, _registry) =
            register_test_compositor_layers(vec![default_grid, current_grid]);
        assert_eq!(grid_store.register(appended_grid), Ok(appended_grid_id));
        let _compositor = attach_test_compositor();
        clear_registered_test_layer_publications();

        let (target, current_window_layer) = {
            let compositor = lock_compositor();
            (
                compositor
                    .grid_placement_target(appended_grid_id, false)
                    .unwrap(),
                compositor.layer_index(current_grid_id).unwrap(),
            )
        };
        assert_eq!(
            registered_context()
                .compositor()
                .place_grid_in_registered_layers(
                    grid_store,
                    target,
                    current_window_layer,
                    CompositorGridPlacementRequest {
                        placement: CompositorGridPlacement::try_new(1, 2, 2, 2).unwrap(),
                        z_index: 0,
                        blending: false,
                        valid: true,
                        on_top: false,
                        accepts_mouse_input: true,
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
        assert_eq!(compositor_layer_ids(), expected_registry);
        assert_eq!(
            take_registered_test_layer_publication(appended_grid_id),
            Some(CompositorLayerPublication { layer_index: 1 })
        );
        assert_eq!(
            take_registered_test_layer_publication(current_grid_id),
            Some(CompositorLayerPublication { layer_index: 2 })
        );

        assert_eq!(
            lock_compositor()
                .layers()
                .unwrap()
                .get(1)
                .unwrap()
                .placement,
            CompositorGridPlacement::try_new(1, 2, 2, 2).unwrap()
        );
    }

    #[test]
    fn layer_adjustment_lowers_across_strictly_higher_z_indices() {
        let default_grid = grid_with_size(1, 1);
        let equal_grid = grid_with_size(1, 1);
        let higher_grid = grid_with_size(1, 1);
        let moving_grid = grid_with_size(1, 1);
        let mut equal_layer = test_layer_state(&equal_grid);
        equal_layer.z_index = 10;
        let mut higher_layer = test_layer_state(&higher_grid);
        higher_layer.z_index = 20;
        let mut moving_layer = test_layer_state(&moving_grid);
        moving_layer.z_index = 10;
        let layers = Layers {
            default: test_layer_state(&default_grid),
            overlays: vec![equal_layer, higher_layer, moving_layer],
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
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 8, 0, false),
            vec![moving_layer, first_crossed_layer, second_crossed_layer],
        );

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
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 1, 0, false),
            vec![
                compositor_layer(&first_grid, 0, 1, 1, false),
                compositor_layer(&second_grid, 0, 1, 2, false),
            ],
        );

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
    fn layer_move_updates_order_and_publication() {
        let default_grid = grid_with_row(&[schar_from_ascii('d')], &[1]);
        let lower_grid = grid_with_row(&[schar_from_ascii('l')], &[2]);
        let upper_grid = grid_with_row(&[schar_from_ascii('u')], &[3]);
        let lower_grid_id = lower_grid.id();
        let upper_grid_id = upper_grid.id();
        let expected_registry = [default_grid.id(), upper_grid_id, lower_grid_id];
        let mut layers =
            Layers::try_new(vec![default_grid.id(), lower_grid_id, upper_grid_id]).unwrap();
        for grid_id in layers.iter().map(|layer| layer.grid_id).collect::<Vec<_>>() {
            assert!(layers.take_publication(grid_id).is_some());
        }
        layers
            .move_layer(CompositorLayerMove {
                source_index: 2,
                target_index: 1,
            })
            .unwrap();
        assert_eq!(
            layers.iter().map(|layer| layer.grid_id).collect::<Vec<_>>(),
            expected_registry
        );
        assert_eq!(
            layers.take_publication(upper_grid_id),
            Some(CompositorLayerPublication { layer_index: 1 })
        );
        assert_eq!(
            layers.take_publication(lower_grid_id),
            Some(CompositorLayerPublication { layer_index: 2 })
        );
    }

    #[test]
    fn layer_removal_updates_order_and_publication() {
        let default_grid = grid_with_row(&[schar_from_ascii('d')], &[1]);
        let removed_grid = grid_with_row(&[schar_from_ascii('r')], &[2]);
        let remaining_grid = grid_with_row(&[schar_from_ascii('k')], &[3]);
        let removed_grid_id = removed_grid.id();
        let remaining_grid_id = remaining_grid.id();
        let expected_registry = [default_grid.id(), remaining_grid_id];
        let mut layers =
            Layers::try_new(vec![default_grid.id(), removed_grid_id, remaining_grid_id]).unwrap();
        for grid_id in layers.iter().map(|layer| layer.grid_id).collect::<Vec<_>>() {
            assert!(layers.take_publication(grid_id).is_some());
        }
        assert_eq!(
            layers.remove_layer(0),
            Err(CompositorError::DefaultLayerMutation)
        );
        assert_eq!(layers.remove_layer(1), Ok(()));
        assert_eq!(
            layers.iter().map(|layer| layer.grid_id).collect::<Vec<_>>(),
            expected_registry
        );
        assert!(layers.take_publication(removed_grid_id).is_none());
        assert_eq!(
            layers.take_publication(remaining_grid_id),
            Some(CompositorLayerPublication { layer_index: 1 })
        );
    }

    #[test]
    fn registered_layer_removal_returns_area() {
        let default_grid = grid_with_size(4, 4);
        let removed_grid = grid_with_size(2, 2);
        let remaining_grid = grid_with_size(2, 2);
        let removed_grid_id = removed_grid.id();
        let remaining_grid_id = remaining_grid.id();
        let expected_registry = [default_grid.id(), remaining_grid_id];

        let (grid_store, _registry) =
            register_test_compositor_layers(vec![default_grid, removed_grid, remaining_grid]);
        set_registered_test_layer_origin(removed_grid_id, ComposedPosition::new(1, 1));
        let _compositor = attach_test_compositor();
        clear_registered_test_layer_publications();

        let layer_index = lock_compositor()
            .layer_index(removed_grid_id)
            .unwrap()
            .unwrap();
        assert_eq!(
            registered_context()
                .compositor()
                .remove_registered_layer(grid_store, layer_index),
            Ok(CompositorLayerRemoval {
                recompose_area: Some(CompositorRecomposeArea {
                    rows: 1..3,
                    columns: 1..3,
                }),
            })
        );
        assert_eq!(compositor_layer_ids(), expected_registry);
        assert_eq!(
            take_registered_test_layer_publication(remaining_grid_id),
            Some(CompositorLayerPublication { layer_index: 1 })
        );

        assert_eq!(lock_compositor().layer_index(removed_grid_id), Ok(None));
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
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 4, 0, false),
            vec![lower_layer, upper_layer],
        );

        assert_eq!(
            layers.topmost_grid_at(ComposedPosition::new(2, 2), LayerHitTest::Any,),
            Some(CompositorGridHit {
                grid_id: upper_grid.id(),
                local_position: GridPosition::new(1, 1),
            })
        );
    }

    #[test]
    fn registered_hit_testing_returns_grid_identity_and_coordinates() {
        let default_grid = grid_with_size(4, 4);
        let mut overlay_grid = grid_with_size(2, 2);
        assert!(overlay_grid.set_cell_for_test(0, 0, GridCell::new(schar_from_ascii('x'), 7, 9),));
        let overlay_grid_id = overlay_grid.id();
        let expected_hit = CompositorGridHit {
            grid_id: overlay_grid_id,
            local_position: GridPosition::new(0, 0),
        };
        let expected_cell = overlay_grid.cell(0, 0).unwrap();
        let (grid_store, _registry) =
            register_test_compositor_layers(vec![default_grid, overlay_grid]);
        set_registered_test_layer_origin(overlay_grid_id, ComposedPosition::new(1, 1));

        set_registered_test_layer_mouse_input(overlay_grid_id, false);
        assert_eq!(
            lock_compositor().grid_at(
                &grid_store,
                ComposedPosition::new(1, 1),
                LayerHitTest::MouseEnabled,
            ),
            Ok(None)
        );
        assert_eq!(
            lock_compositor().cell_at(&grid_store, ComposedPosition::new(1, 1)),
            Ok(Some(expected_cell))
        );

        set_registered_test_layer_mouse_input(overlay_grid_id, true);
        assert_eq!(
            lock_compositor().grid_at(
                &grid_store,
                ComposedPosition::new(1, 1),
                LayerHitTest::MouseEnabled,
            ),
            Ok(Some(expected_hit))
        );
        assert_eq!(
            lock_compositor().grid_at(
                &grid_store,
                ComposedPosition::new(0, 0),
                LayerHitTest::MouseEnabled,
            ),
            Ok(None)
        );
        assert_eq!(
            lock_compositor().cell_at(&grid_store, ComposedPosition::new(0, 0)),
            Ok(None)
        );
    }

    #[test]
    fn hit_testing_excludes_the_default_layer() {
        let default_grid = grid_with_size(3, 3);
        let layers = compositor_layers(compositor_layer(&default_grid, 0, 3, 0, false), Vec::new());

        assert!(
            layers
                .topmost_grid_at(ComposedPosition::new(1, 1), LayerHitTest::Any,)
                .is_none()
        );
    }

    #[test]
    fn positioned_grid_translates_screen_positions_to_local_coordinates() {
        let grid = grid_with_size(2, 3);
        let positioned = PositionedGrid::new(&grid, ComposedPosition::new(4, 5));

        assert!(positioned.contains(ComposedPosition::new(4, 5)));
        assert!(positioned.contains(ComposedPosition::new(5, 7)));
        assert!(!positioned.contains(ComposedPosition::new(3, 5)));
        assert!(!positioned.contains(ComposedPosition::new(6, 5)));
        assert!(!positioned.contains(ComposedPosition::new(5, 8)));
        assert_eq!(
            positioned.local_position(ComposedPosition::new(5, 7)),
            Some(GridPosition::new(1, 2))
        );
        assert_eq!(
            positioned.hit(ComposedPosition::new(5, 7)),
            Some(CompositorGridHit {
                grid_id: grid.id(),
                local_position: GridPosition::new(1, 2),
            })
        );
        assert_eq!(
            positioned.cell(ComposedPosition::new(5, 7)),
            grid.cell(1, 2)
        );
        assert_eq!(positioned.local_position(ComposedPosition::new(6, 5)), None);
        assert_eq!(positioned.cell(ComposedPosition::new(6, 5)), None);
    }

    #[test]
    fn mouse_hit_testing_skips_layers_that_reject_mouse_input() {
        let default_grid = grid_with_size(3, 3);
        let lower_grid = grid_with_size(2, 2);
        let upper_grid = grid_with_size(2, 2);
        let lower_layer = compositor_layer(&lower_grid, 1, 2, 1, false);
        let mut upper_layer = compositor_layer(&upper_grid, 1, 2, 2, false);
        upper_layer.accepts_mouse_input = false;
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 3, 0, false),
            vec![lower_layer, upper_layer],
        );
        let point = ComposedPosition::new(0, 1);

        assert_eq!(
            layers.topmost_grid_at(point, LayerHitTest::Any),
            Some(CompositorGridHit {
                grid_id: upper_grid.id(),
                local_position: GridPosition::new(0, 0),
            })
        );
        assert_eq!(
            layers.topmost_grid_at(point, LayerHitTest::MouseEnabled),
            Some(CompositorGridHit {
                grid_id: lower_grid.id(),
                local_position: GridPosition::new(0, 0),
            })
        );
    }

    #[test]
    fn hit_testing_reports_when_no_non_default_layer_contains_the_cell() {
        let default_grid = grid_with_size(3, 3);
        let overlay_grid = grid_with_size(1, 1);
        let mut overlay_layer = compositor_layer(&overlay_grid, 2, 1, 1, false);
        overlay_layer.origin.row = 2;
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 3, 0, false),
            vec![overlay_layer],
        );

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
        let mut upper_layer = compositor_layer(&upper_grid, 1, 2, 1, false);
        upper_layer.z_index = 100;
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 3, 0, false),
            vec![upper_layer],
        );
        let covered_point = ComposedPosition::new(0, 1);

        let current_cursor_grid = CompositorCursorGrid::new(current_grid.id(), 50);
        let upper_cursor_grid = CompositorCursorGrid::new(upper_grid.id(), 100);

        assert!(layers.is_cursor_covered_by_higher_layer(covered_point, current_cursor_grid, 50));
        assert!(!layers.is_cursor_covered_by_higher_layer(covered_point, current_cursor_grid, 51));
        assert!(!layers.is_cursor_covered_by_higher_layer(covered_point, upper_cursor_grid, 0));
        assert!(!layers.is_cursor_covered_by_higher_layer(
            ComposedPosition::new(2, 2),
            current_cursor_grid,
            0,
        ));
    }

    #[test]
    fn registered_layers_support_cursor_coverage_queries() {
        let default_grid = grid_with_size(4, 4);
        let current_grid = grid_with_size(2, 2);
        let upper_grid = grid_with_size(2, 2);
        let external_grid = grid_with_size(2, 2);
        let current_grid_id = current_grid.id();
        let upper_grid_id = upper_grid.id();
        let current_cursor_grid = CompositorCursorGrid::new(current_grid_id, 0);
        let external_cursor_grid = CompositorCursorGrid::new(external_grid.id(), 50);

        let (grid_store, _registry) =
            register_test_compositor_layers(vec![default_grid, current_grid, upper_grid]);
        let _compositor = attach_test_compositor();
        set_registered_test_layer_z_index(current_grid_id, 50);
        set_registered_test_layer_z_index(upper_grid_id, 100);

        assert_eq!(
            lock_compositor().is_cursor_covered(
                &grid_store,
                current_cursor_grid,
                ComposedPosition::new(0, 0),
                50,
            ),
            Ok(true)
        );
        assert_eq!(
            lock_compositor().is_cursor_covered(
                &grid_store,
                external_cursor_grid,
                ComposedPosition::new(0, 0),
                50,
            ),
            Ok(true)
        );
        assert_eq!(
            lock_compositor().is_cursor_covered(
                &grid_store,
                current_cursor_grid,
                ComposedPosition::new(0, 0),
                51,
            ),
            Ok(false)
        );
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
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 1, 0, false),
            vec![overlay],
        );
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
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 1, 0, false),
            vec![message],
        );
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
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 10, 0, false),
            vec![current_layer],
        );

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
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 8, 0, false),
            vec![overlay],
        );

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
        let layers = compositor_layers(compositor_layer(&default_grid, 0, 8, 0, false), Vec::new());

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
        let layers = compositor_layers(layer, Vec::new());

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
        let layers = compositor_layers(default_layer, vec![overlay]);

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
        let layers = compositor_layers(layer, Vec::new());

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
        let pool = Mutex::new(CellBufferPool::new());
        pool.try_lock().unwrap().resize(3);

        {
            let first = CellBufferLease::acquire(&pool);
            assert_eq!(first.characters.len(), 3);
            assert_eq!(first.attributes.len(), 3);

            {
                let second = CellBufferLease::acquire(&pool);
                assert_eq!(second.characters.len(), 3);
                assert_eq!(second.attributes.len(), 3);
            }

            assert_eq!(pool.try_lock().unwrap().available_buffer_count(), 1);
        }

        assert_eq!(pool.try_lock().unwrap().available_buffer_count(), 2);
    }

    #[test]
    fn compositor_buffer_pool_discards_old_width_storage() {
        let pool = Mutex::new(CellBufferPool::new());
        pool.try_lock().unwrap().resize(2);

        {
            let leased = CellBufferLease::acquire(&pool);
            assert_eq!(leased.characters.len(), 2);
            assert_eq!(leased.attributes.len(), 2);
            pool.try_lock().unwrap().resize(4);
        }

        assert_eq!(pool.try_lock().unwrap().available_buffer_count(), 0);

        let resized = CellBufferLease::acquire(&pool);
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
        let mut compositor = CellBuffer::new();

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
        let layers = compositor_layers(
            compositor_layer(&default_grid, 0, 6, 0, false),
            vec![
                compositor_layer(&overlay, 2, 2, 1, false),
                compositor_layer(&disabled, 0, 1, 2, true),
            ],
        );
        let mut compositor = CellBuffer::new();

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
        let layers = compositor_layers(compositor_layer(&default_grid, 0, 3, 0, false), Vec::new());
        let mut compositor = CellBuffer::new();

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
