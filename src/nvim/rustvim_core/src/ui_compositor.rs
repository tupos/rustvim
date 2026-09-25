use std::{
    collections::HashMap,
    ffi::CString,
    ops::Range,
    ptr::NonNull,
    slice,
    sync::{Mutex, MutexGuard},
};

use crate::{
    defines::{CVimString, Integer, OptInt},
    generated::enums::RdbFlags,
    globals,
    grid::{
        CScreenGrid, GridId, GridRow, GridRowSegment, Highlight, ScreenGrid, ScreenGridGuard,
        VimSAttrT, VimSCharT, default_grid as c_default_grid, msg_grid as c_msg_grid,
        schar_from_ascii, schar_from_buf, schar_from_char,
    },
};

const LINE_FLAG_WRAP: libc::c_int = 1;
const LINE_FLAG_INVALID: libc::c_int = 2;

unsafe extern "C" {
    static mut p_wd: OptInt;
    static mut rdb_flags: libc::c_uint;

    fn log_debug_message(message: *const libc::c_char);
    fn os_sleep(ms: u64);
    fn syn_check_group(name: *const libc::c_char, length: libc::size_t) -> libc::c_int;
    fn syn_id2attr(highlight_id: libc::c_int) -> libc::c_int;
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
    fn ui_get_message_separator_attr() -> VimSAttrT;
}

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

/// One cell in composed-screen coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorPoint {
    row: i32,
    column: i32,
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
    origin: CompositorPoint,
}

impl<'grid> PositionedGrid<'grid> {
    /// Constructs a positioned view of a grid with valid dimensions.
    fn try_new(grid: &'grid ScreenGrid, origin: CompositorPoint) -> Option<Self> {
        (grid.rows >= 0 && grid.cols >= 0).then_some(Self { grid, origin })
    }

    /// Positions a grid at its stored compositor origin.
    fn try_from_compositor_position(grid: &'grid ScreenGrid) -> Option<Self> {
        let (row, column) = grid.compositor_position();
        Self::try_new(grid, CompositorPoint { row, column })
    }

    /// Reports whether the allocated grid contains one screen cell.
    fn contains(self, point: CompositorPoint) -> bool {
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
    fn matches(self, point: CompositorPoint, hit_test: LayerHitTest) -> bool {
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
    fn with_ends_clamped_to_grid(self, grid: &ScreenGrid) -> Option<Self> {
        if grid.rows < 0 || grid.cols < 0 {
            return None;
        }

        Some(Self {
            rows: self.rows.start..self.rows.end.min(grid.rows),
            columns: self.columns.start..self.columns.end.min(grid.cols),
        })
    }
}

/// Dimensions of a compositor layer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorSize {
    height: i32,
    width: i32,
}

impl CompositorSize {
    /// Constructs nonnegative compositor dimensions.
    fn try_new(height: i32, width: i32) -> Option<Self> {
        (height >= 0 && width >= 0).then_some(Self { height, width })
    }
}

/// Validated native inputs for positioning the composed cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorCursorRequest {
    grid_id: GridId,
    local_position: CompositorPoint,
}

/// Selected layer and composed-screen cursor action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorCursorPlan {
    layer_index: usize,
    screen_position: CompositorPoint,
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
    origin: CompositorPoint,
    size: CompositorSize,
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
            origin: CompositorPoint { row, column },
            size: CompositorSize::try_new(height, width)?,
        })
    }

    /// Applies this placement to an owned Rust grid.
    fn apply_to(self, grid: &mut ScreenGrid) {
        grid.set_compositor_position(self.origin.row, self.origin.column);
        grid.set_compositor_size(self.size.height, self.size.width);
    }

    /// Returns the exclusive bottom edge of the requested area.
    fn bottom_edge(self) -> Option<i32> {
        self.origin.row.checked_add(self.size.height)
    }

    /// Returns the exclusive right edge of the requested area.
    fn right_edge(self) -> Option<i32> {
        self.origin.column.checked_add(self.size.width)
    }

    /// Returns the allocated grid area at this placement.
    fn allocated_area(self, grid: &ScreenGrid) -> Option<CompositorRecomposeArea> {
        Some(CompositorRecomposeArea {
            rows: self.origin.row..self.origin.row.checked_add(grid.rows)?,
            columns: self.origin.column..self.origin.column.checked_add(grid.cols)?,
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
    ) -> Option<Self> {
        let redraw_area = if valid && should_draw {
            Some(placement.allocated_area(grid)?)
        } else {
            None
        };

        Some(Self {
            placement,
            moved: true,
            exposed_areas: Vec::new(),
            redraw_area,
        })
    }
}

/// A half-open rectangular region in compositor coordinates.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorRegion {
    rows: Range<i32>,
    columns: Range<i32>,
}

impl CompositorRegion {
    /// Constructs a region with ordered row and column boundaries.
    fn try_new(rows: Range<i32>, columns: Range<i32>) -> Option<Self> {
        (rows.start <= rows.end && columns.start <= columns.end).then_some(Self { rows, columns })
    }

    /// Translates a grid-local region into composed-screen coordinates.
    fn translated(self, row_offset: i32, column_offset: i32) -> Option<Self> {
        Self::try_new(
            self.rows.start.checked_add(row_offset)?..self.rows.end.checked_add(row_offset)?,
            self.columns.start.checked_add(column_offset)?
                ..self.columns.end.checked_add(column_offset)?,
        )
    }

    /// Returns the boundaries used by the existing C coverage check.
    ///
    /// The compositor historically treats the scroll region's bottom and
    /// right boundaries as inclusive for this conservative overlap check.
    fn coverage_rect(&self) -> Option<CompositorRect> {
        CompositorRect::try_new(
            self.rows.start,
            self.rows.end,
            self.columns.start,
            self.columns.end,
        )
    }
}

/// Validated native inputs for positioning the message grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorMessagePositionRequest {
    row: i32,
    separator_character: Option<VimSCharT>,
    screen_size: CompositorSize,
    scrolled: bool,
}

impl CompositorMessagePositionRequest {
    /// Constructs a message-position request from validated native values.
    fn new(
        row: i32,
        separator_character: Option<VimSCharT>,
        screen_size: CompositorSize,
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
    region: CompositorRegion,
    row_delta: i32,
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
}

impl CompositorDrawState {
    /// Creates the initial state before any composed UI is attached.
    const fn new() -> Self {
        Self {
            attached_ui_count: 0,
            screen_valid: true,
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
    fn unregister_ui(&mut self) -> Option<bool> {
        self.attached_ui_count = self.attached_ui_count.checked_sub(1)?;
        Some(self.attached_ui_count == 0)
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
    ) -> Option<CompositorMessagePositionPlan> {
        if default_grid.cols < 0 {
            return None;
        }

        if !should_draw {
            return Some(CompositorMessagePositionPlan::default());
        }

        if request.row > self.current_row {
            return Some(CompositorMessagePositionPlan {
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
            return Some(CompositorMessagePositionPlan::default());
        }

        let row_delta = self.current_row.checked_sub(request.row)?;
        if message_grid.blends_with_background() {
            let separator_offset = if request.scrolled { 1 } else { 0 };
            return Some(CompositorMessagePositionPlan {
                scroll: None,
                recompose: Some(CompositorRecomposeArea {
                    rows: request.row.saturating_sub(separator_offset).max(0)
                        ..request.screen_size.height.checked_sub(row_delta)?,
                    columns: 0..request.screen_size.width,
                }),
            });
        }

        let previous_separator_offset = if self.was_scrolled { 1 } else { 0 };
        let scroll = MessageGridScroll {
            region: CompositorRegion::try_new(
                request.row.saturating_sub(previous_separator_offset).max(0)
                    ..request.screen_size.height,
                0..request.screen_size.width,
            )?,
            row_delta,
        };
        let recompose = if request.scrolled && !self.was_scrolled && request.row > 0 {
            Some(CompositorRecomposeArea {
                rows: request.row.checked_sub(1)?..request.row,
                columns: 0..request.screen_size.width,
            })
        } else {
            None
        };

        Some(CompositorMessagePositionPlan {
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
    ) -> Option<CompositorMessagePositionPlan> {
        let separator_row = if request.scrolled && request.row > 0 {
            Some(request.row.checked_sub(1)?)
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

        Some(plan)
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

/// Validated grid-local inputs for planning one compositor scroll.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorScrollRequest {
    region: CompositorRegion,
    row_delta: i32,
    column_delta: i32,
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
            region: CompositorRegion::try_new(top..bottom, left..right)?,
            row_delta,
            column_delta,
        })
    }
}

/// Action selected for one validated compositor scroll.
#[derive(Clone, Debug, Eq, PartialEq)]
enum CompositorScrollPlan {
    /// Forward the translated scroll directly to the composed UI.
    Forward {
        region: CompositorRegion,
        row_delta: i32,
        column_delta: i32,
    },

    /// Recompose valid rows because another layer covers the region.
    Recompose {
        region: CompositorRegion,
        screen_rows: Range<i32>,
        first_grid_row: i32,
        grid_start_column: i32,
    },
}

/// A complete scroll action ready to execute after restoring grid ownership.
#[derive(Clone, Debug, Eq, PartialEq)]
enum CompositorScrollOperation {
    /// Forward the translated scroll directly to the composed UI.
    Forward {
        region: CompositorRegion,
        row_delta: i32,
        column_delta: i32,
    },

    /// Recompose only rows whose source cells are valid.
    Recompose {
        region: CompositorRegion,
        rows: Vec<i32>,
    },
}

impl CompositorScrollPlan {
    /// Resolves row validity while the selected grid is safely borrowed.
    fn prepare(self, grid: &ScreenGrid) -> CompositorScrollOperation {
        match self {
            Self::Forward {
                region,
                row_delta,
                column_delta,
            } => CompositorScrollOperation::Forward {
                region,
                row_delta,
                column_delta,
            },
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
    flags: i32,
}

impl CompositorRawLineRequest {
    /// Constructs a raw-line request with ordered grid-local columns.
    fn try_new(
        row: i32,
        start_column: i32,
        end_column: i32,
        clear_column: i32,
        flags: i32,
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
    flags: i32,
    action: CompositorRawLineAction,
}

/// Output range prepared for one raw-line UI event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorSubmission {
    start_col: usize,
    end_col: usize,
    flags: libc::c_int,
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
    ) -> Option<SegmentComposition> {
        if foreground.len() != self.characters.len() {
            return None;
        }

        self.characters.copy_from_slice(foreground.characters());
        self.attributes.copy_from_slice(foreground.attributes());

        Some(SegmentComposition {
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
    ) -> Option<SegmentComposition> {
        let composition = self.try_copy(foreground)?;
        if blending {
            self.try_blend_prefix_over(background, self.characters.len())?;
        }
        Some(composition)
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
    ) -> Option<()> {
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
    ) -> Option<()> {
        let len = self.characters.len();
        if background.len() != len || blend_len > len {
            return None;
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

        Some(())
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
    origin_row: i32,
    origin_column: i32,
    requested_width: i32,
    requested_height: i32,
    visible_width: i32,
    visible_height: i32,
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
        let visible_width = grid.cols.min(requested_width);
        let visible_height = grid.rows.min(requested_height);
        if visible_width < 0 || visible_height < 0 {
            return None;
        }

        Some(Self {
            grid,
            origin_row,
            origin_column,
            requested_width,
            requested_height,
            visible_width,
            visible_height,
            z_index: grid.z_index(),
            layer_index: grid.compositor_index(),
            blending: grid.blends_with_background(),
            disabled: grid.is_compositor_disabled(),
            is_default,
        })
    }

    /// Returns the allocated dimensions of this layer's grid.
    fn allocated_size(self) -> CompositorSize {
        CompositorSize {
            height: self.grid.rows,
            width: self.grid.cols,
        }
    }

    /// Returns the exclusive right edge of this layer.
    fn right_edge(self) -> Option<i32> {
        self.origin_column.checked_add(self.visible_width)
    }

    /// Returns the requested compositor area, optionally including the row
    /// occupied by a scrolled message separator.
    fn coverage_rect(self, include_row_above: bool) -> Option<CompositorRect> {
        let top = self.origin_row.checked_sub(i32::from(include_row_above))?;
        let bottom = self
            .origin_row
            .checked_add(self.requested_height)?
            .checked_sub(1)?;
        let right = self
            .origin_column
            .checked_add(self.requested_width)?
            .checked_sub(1)?;
        CompositorRect::try_new(top, bottom, self.origin_column, right)
    }

    /// Computes the existing compositor recomposition bounds for two layers.
    ///
    /// Empty or inverted bounds are retained because the C area renderer also
    /// receives them and applies its own clipping and debug behavior.
    fn recomposition_area_with(self, other: Self) -> Option<CompositorRecomposeArea> {
        let bottom = self
            .origin_row
            .checked_add(self.grid.rows)?
            .min(other.origin_row.checked_add(other.grid.rows)?);
        let right = self
            .origin_column
            .checked_add(self.grid.cols)?
            .min(other.origin_column.checked_add(other.grid.cols)?);

        Some(CompositorRecomposeArea {
            rows: self.origin_row.max(other.origin_row)..bottom,
            columns: self.origin_column.max(other.origin_column)..right,
        })
    }

    /// Plans repositioning and resizing this existing compositor layer.
    fn try_plan_placement(
        self,
        placement: CompositorGridPlacement,
        valid: bool,
        should_draw: bool,
    ) -> Option<CompositorGridPlacementPlan> {
        let moved = placement.origin.row != self.origin_row
            || placement.origin.column != self.origin_column;
        let mut exposed_areas = Vec::new();

        if should_draw {
            let old_bottom = self.origin_row.checked_add(self.requested_height)?;
            let old_right = self.origin_column.checked_add(self.requested_width)?;
            let new_bottom = placement.bottom_edge()?;
            let new_right = placement.right_edge()?;

            exposed_areas.push(CompositorRecomposeArea {
                rows: self.origin_row..placement.origin.row,
                columns: self.origin_column..old_right,
            });

            if self.origin_column < placement.origin.column {
                exposed_areas.push(CompositorRecomposeArea {
                    rows: placement.origin.row.max(self.origin_row)..new_bottom.min(old_bottom),
                    columns: self.origin_column..placement.origin.column,
                });
            }

            if new_right < old_right {
                exposed_areas.push(CompositorRecomposeArea {
                    rows: placement.origin.row.max(self.origin_row)..new_bottom.min(old_bottom),
                    columns: new_right..old_right,
                });
            }

            exposed_areas.push(CompositorRecomposeArea {
                rows: new_bottom..old_bottom,
                columns: self.origin_column..old_right,
            });
        }

        let redraw_area = if moved && valid && should_draw {
            Some(placement.allocated_area(self.grid)?)
        } else {
            None
        };

        Some(CompositorGridPlacementPlan {
            placement,
            moved,
            exposed_areas,
            redraw_area,
        })
    }

    /// Reports whether this layer participates in selection on `row`.
    fn is_visible_on_row(self, row: i32) -> bool {
        !self.disabled
            && row >= self.origin_row
            && self
                .origin_row
                .checked_add(self.visible_height)
                .is_some_and(|bottom| row < bottom)
    }

    /// Reports whether this layer covers one composed-screen cell.
    fn covers(self, row: i32, column: i32) -> bool {
        self.is_visible_on_row(row)
            && column >= self.origin_column
            && self.right_edge().is_some_and(|right| column < right)
    }

    /// Returns this layer as a positioned allocated grid.
    fn positioned_grid(self) -> Option<PositionedGrid<'grid>> {
        PositionedGrid::try_new(
            self.grid,
            CompositorPoint {
                row: self.origin_row,
                column: self.origin_column,
            },
        )
    }

    /// Borrows the grid cells corresponding to composed-screen columns.
    fn screen_segment(self, row: i32, columns: Range<usize>) -> Option<GridRowSegment<'grid>> {
        let start_column = i32::try_from(columns.start).ok()?;
        let end_column = i32::try_from(columns.end).ok()?;
        let grid_row = usize::try_from(row.checked_sub(self.origin_row)?).ok()?;
        let grid_start = usize::try_from(start_column.checked_sub(self.origin_column)?).ok()?;
        let grid_end = usize::try_from(end_column.checked_sub(self.origin_column)?).ok()?;

        GridRow::try_from_grid(self.grid, grid_row)?.segment(grid_start..grid_end)
    }

    /// Reports whether a line ending on this layer may retain its wrap flag.
    fn allows_wrap(self, screen_columns: usize) -> bool {
        self.is_default
            || (self.origin_column == 0
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
/// `CompositorLayerGuards` maintains the temporary C bindings separately.
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
    fn try_append(&mut self, grid_id: GridId) -> Option<()> {
        if self.index_of(grid_id).is_some() {
            return None;
        }

        self.layer_ids.push(grid_id);
        Some(())
    }

    /// Removes one non-default grid identity.
    fn remove_non_default(&mut self, layer_index: usize) -> Option<GridId> {
        if layer_index == 0 || layer_index >= self.layer_ids.len() {
            return None;
        }

        Some(self.layer_ids.remove(layer_index))
    }

    /// Applies one validated layer relocation.
    fn apply_layer_move(&mut self, movement: CompositorLayerMove) -> Option<()> {
        movement.apply_to(&mut self.layer_ids)
    }

    /// Clones the first `layer_count` layer identities into a new registry.
    fn prefix(&self, layer_count: usize) -> Option<Self> {
        Self::try_new(self.layer_ids.get(..layer_count)?.to_vec())
    }
}

/// One temporary pointer connecting a native grid identity to C storage.
///
/// This boundary-only representation will disappear when grids become
/// permanently owned by Rust.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CCompositorGridBinding {
    cgrid: NonNull<CScreenGrid>,
}

// SAFETY: Neovim invokes compositor operations synchronously on its main
// thread. Moving this wrapper does not access the referenced C grid, and the
// pointer is dereferenced only while an FFI operation has exclusive ownership
// of that grid.
unsafe impl Send for CCompositorGridBinding {}

/// Transitional C-grid pointers keyed by safe native grid identities.
#[derive(Clone, Debug)]
struct CCompositorGridBindings {
    grids: HashMap<GridId, CCompositorGridBinding>,
}

impl CCompositorGridBindings {
    /// Creates a nonempty registry containing unique C grid pointers.
    fn try_new(grids: HashMap<GridId, CCompositorGridBinding>) -> Option<Self> {
        if grids.is_empty() {
            return None;
        }

        let mut pointers = Vec::with_capacity(grids.len());
        for binding in grids.values() {
            if pointers.contains(&binding.cgrid) {
                return None;
            }
            pointers.push(binding.cgrid);
        }

        Some(Self { grids })
    }

    /// Reports whether every ordered grid has exactly one C binding.
    fn matches_registry(&self, registry: &CompositorLayerRegistry) -> bool {
        self.grids.len() == registry.len()
            && registry
                .layer_ids()
                .iter()
                .all(|grid_id| self.grids.contains_key(grid_id))
    }

    /// Returns C grid pointers in native compositor order.
    fn ordered_cgrids(&self, registry: &CompositorLayerRegistry) -> Option<Vec<*mut CScreenGrid>> {
        registry
            .layer_ids()
            .iter()
            .map(|grid_id| {
                self.grids
                    .get(grid_id)
                    .map(|binding| binding.cgrid.as_ptr())
            })
            .collect()
    }
}

/// Process-global compositor state at the transitional C boundary.
///
/// Safe compositor logic receives only a clone of `registry`; C pointers stay
/// confined to ownership adapters.
struct CompositorLayerFfiState {
    registry: CompositorLayerRegistry,
    c_grids: CCompositorGridBindings,
}

impl CompositorLayerFfiState {
    /// Combines matching safe layer order and C-grid bindings.
    fn try_new(
        registry: CompositorLayerRegistry,
        c_grids: CCompositorGridBindings,
    ) -> Option<Self> {
        c_grids
            .matches_registry(&registry)
            .then_some(Self { registry, c_grids })
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
    fn apply_to<T>(self, layers: &mut [T]) -> Option<()> {
        if self.source_index < self.target_index {
            layers
                .get_mut(self.source_index..=self.target_index)?
                .rotate_left(1);
        } else if self.source_index > self.target_index {
            layers
                .get_mut(self.target_index..=self.source_index)?
                .rotate_right(1);
        } else {
            layers.get(self.source_index)?;
        }

        Some(())
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
    ) -> Option<usize> {
        let mut insertion_index = self.layers.len();
        while insertion_index > 0 && self.layers.get(insertion_index - 1)?.z_index > z_index {
            insertion_index -= 1;
        }

        // The default grid must remain the first layer.
        if insertion_index == 0 {
            return None;
        }

        if !on_top && insertion_index > 1 {
            let preceding_index = insertion_index - 1;
            let preceding_layer = self.layer(preceding_index)?;
            if current_window_layer == Some(preceding_index) && preceding_layer.z_index == z_index {
                insertion_index = preceding_index;
            }
        }

        Some(insertion_index)
    }

    /// Finds the compositor layer identified by one native grid identity.
    fn layer_index_with_id(&self, grid_id: GridId) -> Option<usize> {
        self.layers
            .iter()
            .position(|layer| layer.grid.id() == grid_id)
    }

    /// Selects a grid and plans its composed-screen cursor update.
    fn plan_cursor(&self, request: CompositorCursorRequest) -> Option<CompositorCursorAction> {
        let screen_size = self.default_layer()?.allocated_size();
        let Some(layer_index) = self.layer_index_with_id(request.grid_id) else {
            return Some(CompositorCursorAction::Ignore);
        };

        let layer = self.layer(layer_index)?;
        let screen_position = CompositorPoint {
            row: layer.origin_row.checked_add(request.local_position.row)?,
            column: layer
                .origin_column
                .checked_add(request.local_position.column)?,
        };

        let raise_to = if layer.is_default {
            None
        } else {
            let mut target_index = self.layers.len().checked_sub(1)?;
            while target_index > 1 && self.layer(target_index)?.z_index > layer.z_index {
                target_index -= 1;
            }
            (layer_index < target_index).then_some(target_index)
        };
        let emit =
            screen_position.column < screen_size.width && screen_position.row < screen_size.height;

        Some(CompositorCursorAction::Update(CompositorCursorPlan {
            layer_index,
            screen_position,
            raise_to,
            emit,
        }))
    }

    /// Finds the topmost non-default grid containing one screen cell.
    fn topmost_grid_at(&self, point: CompositorPoint, hit_test: LayerHitTest) -> Option<GridId> {
        self.layers.get(1..)?.iter().rev().find_map(|layer| {
            let layer = *layer;
            layer
                .positioned_grid()?
                .matches(point, hit_test)
                .then_some(layer.grid.id())
        })
    }

    /// Reports whether a sufficiently higher layer covers `current_grid`'s cursor.
    fn is_cursor_covered_by_higher_layer(
        &self,
        point: CompositorPoint,
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
    ) -> Option<CompositorLayerMove> {
        let layer = self.layer(layer_index)?;
        if layer.is_default {
            return None;
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

        Some(CompositorLayerMove {
            source_index: layer_index,
            target_index,
        })
    }

    /// Plans an explicit upward move and recomposition of crossed layers.
    fn plan_layer_raise(
        &self,
        layer_index: usize,
        target_index: usize,
    ) -> Option<CompositorLayerRaisePlan> {
        let layer = self.layer(layer_index)?;
        if layer.is_default || target_index < layer_index || target_index >= self.layers.len() {
            return None;
        }

        let recompose_areas = if target_index == layer_index {
            Vec::new()
        } else {
            self.layers
                .get(layer_index + 1..=target_index)?
                .iter()
                .copied()
                .map(|crossed_layer| layer.recomposition_area_with(crossed_layer))
                .collect::<Option<Vec<_>>>()?
        };

        Some(CompositorLayerRaisePlan {
            movement: CompositorLayerMove {
                source_index: layer_index,
                target_index,
            },
            recompose_areas,
        })
    }

    /// Reports whether any layer above the current layer intersects the query.
    fn is_covered_above(&self, query: CompositorCoveredAboveQuery) -> Option<bool> {
        let first_layer_above = query.current_layer_index.checked_add(1)?;
        let layers_above = self.layers.get(first_layer_above..)?;

        Some(layers_above.iter().any(|layer| {
            let include_row_above =
                query.message_was_scrolled && query.message_layer_index == Some(layer.layer_index);
            layer
                .coverage_rect(include_row_above)
                .is_some_and(|rectangle| rectangle.intersects(query.rectangle))
        }))
    }

    /// Plans a translated scroll for one layer in this stack.
    fn plan_scroll(
        &self,
        current_layer_index: usize,
        message_layer_index: Option<usize>,
        message_was_scrolled: bool,
        request: CompositorScrollRequest,
    ) -> Option<CompositorScrollPlan> {
        let current_layer = self.layer(current_layer_index)?;
        let row_delta = request.row_delta;
        let region = request
            .region
            .translated(current_layer.origin_row, current_layer.origin_column)?;
        let covered = self.is_covered_above(CompositorCoveredAboveQuery {
            rectangle: region.coverage_rect()?,
            current_layer_index,
            message_layer_index,
            message_was_scrolled,
        })?;

        if !covered && !current_layer.blending {
            return Some(CompositorScrollPlan::Forward {
                region,
                row_delta,
                column_delta: request.column_delta,
            });
        }

        // TODO(bfredl): calculate non-overlapping subareas that can still be
        // forwarded instead of recomposing the complete covered rectangle.
        let skipped_top = row_delta.checked_neg()?.max(0);
        let skipped_bottom = row_delta.max(0);
        let first_recompose_row = region.rows.start.checked_add(skipped_top)?;
        let recompose_end_row = region.rows.end.checked_sub(skipped_bottom)?;
        let first_grid_row = first_recompose_row.checked_sub(current_layer.origin_row)?;
        let grid_start_column = region
            .columns
            .start
            .checked_sub(current_layer.origin_column)?;

        Some(CompositorScrollPlan::Recompose {
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
    ) -> Option<CompositorRawLinePlan> {
        let current_layer = self.layer(current_layer_index)?;
        let default_layer = self.default_layer()?;
        let row = request.row.checked_add(current_layer.origin_row)?;
        let start_column = request
            .columns
            .start
            .checked_add(current_layer.origin_column)?;
        let end_column = request
            .columns
            .end
            .checked_add(current_layer.origin_column)?;
        let requested_clear_column = request
            .clear_column
            .checked_add(current_layer.origin_column)?;
        let flags = if current_layer.is_default {
            request.flags
        } else {
            request.flags & !LINE_FLAG_WRAP
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
            return Some(plan);
        }

        let screen_columns = default_layer.grid.cols;
        if plan.clear_column > screen_columns {
            plan.clear_column = screen_columns;
            if plan.columns.start >= screen_columns {
                plan.action = CompositorRawLineAction::DiscardColumns;
                return Some(plan);
            }
            plan.columns.end = plan.columns.end.min(screen_columns);
        }

        let covered = self.is_covered_above(CompositorCoveredAboveQuery {
            rectangle: CompositorRect::try_new(
                plan.row,
                plan.row,
                plan.columns.start,
                plan.clear_column,
            )?,
            current_layer_index,
            message_layer_index,
            message_was_scrolled,
        })?;
        if plan.flags & LINE_FLAG_INVALID != 0 || covered || current_layer.blending {
            plan.action = CompositorRawLineAction::Recompose;
        }

        Some(plan)
    }

    /// Selects the topmost layer and its uninterrupted output range.
    fn select_segment(
        &self,
        row: i32,
        start_column: usize,
        end_column: usize,
    ) -> Option<LayerSelection> {
        let column = i32::try_from(start_column).ok()?;
        let line_end = i32::try_from(end_column).ok()?;
        let mut selected = None;
        let mut segment_end = line_end;

        for (index, layer) in self.layers.iter().copied().enumerate() {
            if !layer.is_visible_on_row(row) {
                continue;
            }
            if layer.covers(row, column) {
                selected = Some(index);
                segment_end = layer.right_edge()?;
            } else if layer.origin_column > column {
                segment_end = segment_end.min(layer.origin_column);
            }
        }

        let layer_index = selected?;
        let segment_end = usize::try_from(segment_end.min(line_end)).ok()?;
        (start_column < segment_end).then_some(LayerSelection {
            layer_index,
            columns: start_column..segment_end,
        })
    }
}

/// One temporary C pointer binding paired with its Rust grid owner.
struct GuardedCompositorLayer {
    cgrid: NonNull<CScreenGrid>,
    grid: ScreenGridGuard,
}

/// Temporary Rust ownership of compositor layers received from C.
///
/// The native registry owns layer identity and ordering. C pointers exist only
/// in the transitional binding map. Each `ScreenGridGuard` restores its grid
/// allocations when this collection is dropped.
#[must_use = "dropping the layer guards restores ownership to the C grids"]
struct CompositorLayerGuards {
    registry: CompositorLayerRegistry,
    bindings: HashMap<GridId, GuardedCompositorLayer>,
}

impl CompositorLayerGuards {
    /// Takes temporary ownership of all grids registered at the FFI boundary.
    fn try_take_registered() -> Option<Self> {
        let (registry, cgrids) = {
            let state = lock_compositor_layer_state();
            let state = state.as_ref()?;
            let registry = state.registry.clone();
            let cgrids = state.c_grids.ordered_cgrids(&registry)?;
            (registry, cgrids)
        };

        // SAFETY: The boundary state contains distinct pointers established
        // by validated FFI operations. Compositor calls are serialized on
        // Neovim's main thread, and registered C grids remain alive until
        // removal.
        unsafe { Self::try_take_for_registry(cgrids.as_ptr(), cgrids.len(), registry) }
    }

    /// Binds C grids to one supplied native registry.
    ///
    /// # Safety
    ///
    /// If `layer_count` is nonzero, `layer_grids` must point to that many
    /// readable grid pointers. Every non-null grid must be valid, distinct,
    /// exclusively accessible, and satisfy the ownership and layout
    /// requirements of `ScreenGrid`.
    unsafe fn try_take_for_registry(
        layer_grids: *const *mut CScreenGrid,
        layer_count: usize,
        registry: CompositorLayerRegistry,
    ) -> Option<Self> {
        // SAFETY: The caller satisfies the documented pointer and grid
        // ownership requirements.
        let (_, bindings) = unsafe { Self::take_bindings(layer_grids, layer_count) }?;
        Self::try_from_registry(registry, bindings)
    }

    /// Takes registered grids together with one placement candidate.
    ///
    /// # Safety
    ///
    /// If `candidate` is not registered, it must identify a valid,
    /// exclusively accessible C grid whose buffers satisfy the ownership and
    /// layout requirements of `ScreenGrid`.
    unsafe fn try_take_registered_with_candidate(
        candidate: NonNull<CScreenGrid>,
    ) -> Option<(Self, CompositorGridPlacementTarget)> {
        let mut layer_guards = Self::try_take_registered()?;
        if let Some(layer_index) = layer_guards.index_of(candidate.as_ptr()) {
            let grid_id = layer_guards.registry.layer_id(layer_index)?;
            return Some((
                layer_guards,
                CompositorGridPlacementTarget {
                    grid_id,
                    layer_index,
                    was_registered: true,
                },
            ));
        }

        // SAFETY: The caller transfers ownership of the unregistered
        // candidate for the lifetime of the returned guard collection.
        let grid = unsafe { ScreenGridGuard::take(candidate.as_ptr()) };
        let grid_id = grid.id();
        if layer_guards.bindings.contains_key(&grid_id) {
            return None;
        }

        let layer_index = layer_guards.registry.len();
        layer_guards.registry.try_append(grid_id)?;
        layer_guards
            .bindings
            .insert(
                grid_id,
                GuardedCompositorLayer {
                    cgrid: candidate,
                    grid,
                },
            )
            .is_none()
            .then_some(())?;

        Some((
            layer_guards,
            CompositorGridPlacementTarget {
                grid_id,
                layer_index,
                was_registered: false,
            },
        ))
    }

    /// Creates guards when the binding order itself is under test.
    ///
    /// # Safety
    ///
    /// If `layer_count` is nonzero, `layer_grids` must point to that many
    /// readable grid pointers. Every non-null grid must be valid, distinct,
    /// exclusively accessible, and satisfy the ownership and layout
    /// requirements of `ScreenGrid`.
    #[cfg(test)]
    unsafe fn try_take_in_binding_order(
        layer_grids: *const *mut CScreenGrid,
        layer_count: usize,
    ) -> Option<Self> {
        // SAFETY: The caller satisfies the documented pointer and grid
        // ownership requirements.
        let (layer_ids, bindings) = unsafe { Self::take_bindings(layer_grids, layer_count) }?;
        let registry = CompositorLayerRegistry::try_new(layer_ids)?;
        Self::try_from_registry(registry, bindings)
    }

    /// Pairs each C grid pointer with temporary ownership of its Rust grid.
    fn try_from_registry(
        registry: CompositorLayerRegistry,
        bindings: HashMap<GridId, GuardedCompositorLayer>,
    ) -> Option<Self> {
        if registry.len() != bindings.len()
            || registry
                .layer_ids()
                .iter()
                .any(|grid_id| !bindings.contains_key(grid_id))
        {
            return None;
        }

        Some(Self { registry, bindings })
    }

    /// Validates C bindings and takes temporary ownership of their grids.
    ///
    /// # Safety
    ///
    /// If `layer_count` is nonzero, `layer_grids` must point to that many
    /// readable grid pointers. Every non-null grid must be valid, distinct,
    /// exclusively accessible, and satisfy the ownership and layout
    /// requirements of `ScreenGrid`.
    unsafe fn take_bindings(
        layer_grids: *const *mut CScreenGrid,
        layer_count: usize,
    ) -> Option<(Vec<GridId>, HashMap<GridId, GuardedCompositorLayer>)> {
        if layer_grids.is_null() || layer_count == 0 {
            return None;
        }

        // SAFETY: The caller guarantees that the pointer array is readable for
        // `layer_count` elements for the duration of this synchronous call.
        let cgrids = unsafe { slice::from_raw_parts(layer_grids, layer_count) }
            .iter()
            .copied()
            .map(NonNull::new)
            .collect::<Option<Vec<_>>>()?;
        if cgrids
            .iter()
            .enumerate()
            .any(|(index, grid)| cgrids[..index].contains(grid))
        {
            return None;
        }

        let guarded_layers = cgrids
            .into_iter()
            .map(|cgrid| {
                // SAFETY: Null and duplicate pointers were rejected, and the
                // caller transfers ownership for this operation.
                let grid = unsafe { ScreenGridGuard::take(cgrid.as_ptr()) };
                GuardedCompositorLayer { cgrid, grid }
            })
            .collect::<Vec<_>>();
        let layer_ids = guarded_layers
            .iter()
            .map(|layer| layer.grid.id())
            .collect::<Vec<_>>();
        if layer_ids
            .iter()
            .enumerate()
            .any(|(index, grid_id)| layer_ids[..index].contains(grid_id))
        {
            return None;
        }
        let bindings = guarded_layers
            .into_iter()
            .map(|layer| (layer.grid.id(), layer))
            .collect();

        Some((layer_ids, bindings))
    }

    /// Returns the stack index belonging to one C grid identity.
    fn index_of(&self, cgrid: *mut CScreenGrid) -> Option<usize> {
        let cgrid = NonNull::new(cgrid)?;
        let grid_id = self
            .bindings
            .values()
            .find_map(|layer| (layer.cgrid == cgrid).then_some(layer.grid.id()))?;
        self.registry.index_of(grid_id)
    }

    /// Borrows one owned Rust grid by its layer index.
    fn grid(&self, index: usize) -> Option<&ScreenGrid> {
        let grid_id = self.registry.layer_id(index)?;
        self.bindings.get(&grid_id).map(|layer| &*layer.grid)
    }

    /// Borrows one registered Rust grid by its stable identity.
    fn grid_by_id(&self, grid_id: GridId) -> Option<&ScreenGrid> {
        let layer_index = self.registry.index_of(grid_id)?;
        self.grid(layer_index)
    }

    /// Mutably borrows one owned Rust grid by its layer index.
    fn grid_mut(&mut self, index: usize) -> Option<&mut ScreenGrid> {
        let grid_id = self.registry.layer_id(index)?;
        self.bindings
            .get_mut(&grid_id)
            .map(|layer| &mut *layer.grid)
    }

    /// Borrows all owned grids as a validated compositor layer stack.
    fn layer_stack(&self) -> Option<CompositorLayerStack<'_>> {
        self.layer_stack_prefix(self.registry.len())
    }

    /// Borrows an initial prefix as a validated compositor layer stack.
    fn layer_stack_prefix(&self, layer_count: usize) -> Option<CompositorLayerStack<'_>> {
        let grids = self
            .registry
            .layer_ids()
            .get(..layer_count)?
            .iter()
            .map(|grid_id| self.bindings.get(grid_id).map(|layer| &*layer.grid))
            .collect::<Option<Vec<_>>>()?;
        CompositorLayerStack::try_from_grids(&grids)
    }

    /// Places the appended owner at its planned compositor-stack index.
    fn place_appended_layer(
        &mut self,
        insertion_index: usize,
        placement: CompositorGridPlacement,
    ) -> Option<()> {
        let appended_index = self.registry.len().checked_sub(1)?;
        if insertion_index == 0 || insertion_index > appended_index {
            return None;
        }

        self.apply_layer_move(CompositorLayerMove {
            source_index: appended_index,
            target_index: insertion_index,
        })?;
        placement.apply_to(self.grid_mut(insertion_index)?);
        Some(())
    }

    /// Applies a planned relocation and synchronizes affected grid metadata.
    fn apply_layer_move(&mut self, movement: CompositorLayerMove) -> Option<()> {
        self.registry.apply_layer_move(movement)?;

        let affected_indices = movement.affected_indices();
        let first_index = *affected_indices.start();
        for (offset, index) in affected_indices.enumerate() {
            let grid_id = self.registry.layer_id(index)?;
            let layer = self.bindings.get_mut(&grid_id)?;
            layer.grid.set_compositor_index(first_index + offset);
        }

        Some(())
    }

    /// Removes one non-default layer and updates all shifted indices.
    fn remove_layer(&mut self, layer_index: usize) -> Option<()> {
        let removed_grid_id = self.registry.remove_non_default(layer_index)?;
        let mut removed_layer = self.bindings.remove(&removed_grid_id)?;
        removed_layer.grid.set_compositor_index(0);

        for new_index in layer_index..self.registry.len() {
            let grid_id = self.registry.layer_id(new_index)?;
            self.bindings
                .get_mut(&grid_id)?
                .grid
                .set_compositor_index(new_index);
        }
        Some(())
    }

    /// Captures all current identities and their temporary C bindings.
    fn ffi_state(&self) -> Option<CompositorLayerFfiState> {
        let registry = self.registry.clone();
        let c_grids = registry
            .layer_ids()
            .iter()
            .map(|grid_id| {
                let layer = self.bindings.get(grid_id)?;
                Some((*grid_id, CCompositorGridBinding { cgrid: layer.cgrid }))
            })
            .collect::<Option<HashMap<_, _>>>()?;
        let c_grids = CCompositorGridBindings::try_new(c_grids)?;

        CompositorLayerFfiState::try_new(registry, c_grids)
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
    screen_size: CompositorSize,
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
            screen_size: CompositorSize::try_new(screen_rows, screen_columns)?,
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

    /// Visualizes one compositor operation when redraw debugging is enabled.
    fn visualize(self, requested_area: &CompositorRecomposeArea, highlight_id: i32, delay: bool) {
        if !self.debug_enabled || requested_area.columns.start >= requested_area.columns.end {
            return;
        }
        let Some(area) = clip_recompose_area_to_default_grid(requested_area.clone()) else {
            return;
        };

        // SAFETY: The highlight ID was returned by `syn_check_group`, and this
        // call occurs on Neovim's main thread without holding grid ownership.
        let attribute = unsafe { syn_id2attr(highlight_id) };
        if delay {
            self.delay(area.rows.end - area.rows.start);
        }
        for row in area.rows.clone() {
            assert!(emit_compositor_debug_line(
                row,
                area.columns.clone(),
                attribute,
            ));
        }
        if delay {
            self.delay(area.rows.end - area.rows.start);
        }
    }

    /// Flushes composed output and applies the configured per-operation delay.
    fn delay(self, lines: i32) {
        let factor = lines.clamp(1, 5) as u64;
        // SAFETY: Both functions accept scalar values and are called on
        // Neovim's main thread without holding any grid guard.
        unsafe {
            ui_call_flush();
            os_sleep(factor.wrapping_mul(self.write_delay_ms));
        }
    }
}

/// Captures dynamic C-owned settings as validated native Rust values.
fn compositor_runtime_snapshot() -> Option<CompositorRuntime> {
    // SAFETY: Compositor events run on Neovim's main thread. The accessor
    // returns the current dynamic MsgSeparator attribute by value.
    let (screen_rows, screen_columns, message_separator_attribute, flags, write_delay) = unsafe {
        (
            globals::Rows,
            globals::Columns,
            ui_get_message_separator_attr(),
            rdb_flags,
            p_wd,
        )
    };

    CompositorRuntime::try_new(
        screen_rows,
        screen_columns,
        message_separator_attribute,
        flags,
        write_delay,
        *lock_compositor_debug_highlights(),
    )
}

/// Resolves one redraw-debug highlight group through Neovim's syntax API.
fn compositor_debug_highlight(name: &[u8]) -> i32 {
    // SAFETY: The byte slice supplies a readable pointer for exactly `len`
    // bytes, and `syn_check_group` does not require a trailing NUL.
    unsafe { syn_check_group(name.as_ptr().cast(), name.len()) }
}

/// Emits one formatted raw-line diagnostic through Neovim's debug logger.
fn log_raw_line_diagnostic(diagnostic: RawLineDiagnostic, grid_id: GridId) {
    let message = match diagnostic {
        RawLineDiagnostic::InvalidRow(row) => {
            format!("compositor: invalid row {row} on grid {grid_id}")
        }
        RawLineDiagnostic::ClippedColumns(requested_clear_column) => {
            format!("compositor: invalid last column {requested_clear_column} on grid {grid_id}")
        }
    };
    let message = CString::new(message).expect("formatted compositor diagnostic contains no NUL");

    // SAFETY: `CString` supplies a non-null, NUL-terminated string that
    // remains alive for the synchronous logging call.
    unsafe { log_debug_message(message.as_ptr()) };
}

/// Validated inputs for composing one screen area.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorAreaRequest {
    area: CompositorRecomposeArea,
    settings: CompositorLineSettings,
}

/// Validated inputs for composing one line from temporarily C-owned grids.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorLineInput {
    row: i32,
    columns: Range<i32>,
    flags: i32,
    settings: CompositorLineSettings,
}

/// Validated inputs for composing one complete line in Rust.
#[derive(Clone, Copy)]
struct CompositorLineRequest<'grid> {
    row: i32,
    start_column: i32,
    end_column: i32,
    flags: i32,
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
    fn resize(&mut self, width: usize) -> bool {
        if self.characters.len() != width {
            self.characters = vec![schar_from_ascii(' '); width];
            self.attributes = vec![0; width];
        }
        true
    }

    /// Releases all compositor storage.
    fn release(&mut self) -> bool {
        self.characters = Vec::new();
        self.attributes = Vec::new();
        true
    }

    /// Selects and composes every layer segment for one complete line.
    fn try_compose_line(
        &mut self,
        request: CompositorLineRequest<'_>,
        layers: &CompositorLayerStack<'_>,
    ) -> Option<CompositorSubmission> {
        let mut line =
            self.prepare_line(request.start_column, request.end_column, request.flags)?;
        let line_end = line.columns.end;
        let mut column = line.columns.start;
        let mut last_layer = None;

        while column < line_end {
            let selection = layers.select_segment(request.row, column, line_end)?;
            let selected_layer = layers.layer(selection.layer_index)?;
            let separator = request
                .message_separator
                .filter(|separator| selected_layer.layer_index <= separator.layer.layer_index);
            let composed_layer = separator.map_or(selected_layer, |separator| separator.layer);
            let output_columns = selection.columns;

            let background = if composed_layer.blending {
                Some(
                    layers
                        .default_layer()?
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

        let allow_wrap = last_layer?.allows_wrap(request.screen_columns);
        self.finish_line(line, request.flags, allow_wrap, request.abort_on_invalid)
    }

    /// Prepares one line and applies invalid-line edge expansion.
    fn prepare_line(
        &self,
        start_col: i32,
        end_col: i32,
        flags: libc::c_int,
    ) -> Option<CompositorLine> {
        let width = i32::try_from(self.characters.len()).ok()?;
        let mut start_col = start_col.max(0);
        let mut end_col = end_col;
        let mut skip_start = 0;
        let mut skip_end = 0;

        if flags & LINE_FLAG_INVALID != 0 {
            if start_col > 0 {
                start_col -= 1;
                skip_start = 1;
            }
            if end_col < width {
                end_col += 1;
                skip_end = 1;
            }
        }

        let start_col = usize::try_from(start_col).ok()?;
        let end_col = usize::try_from(end_col).ok()?;
        if start_col >= end_col || end_col > self.characters.len() {
            return None;
        }

        Some(CompositorLine {
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
    ) -> Option<()> {
        let source_len = output_columns.end.checked_sub(output_columns.start)?;
        if source_len == 0 || foreground.len() != source_len {
            return None;
        }

        let line_start = line.columns.start;
        let line_end = line.columns.end;
        if output_columns.start < line_start || output_columns.end > line_end {
            return None;
        }

        let composition = self
            .try_segment_mut(output_columns.clone())?
            .try_copy(foreground)?;

        if composition.is_followed_by_continuation() {
            *self.characters.get_mut(output_columns.end - 1)? = schar_from_ascii(' ');
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
    ) -> Option<()> {
        let source_len = output_columns.end.checked_sub(output_columns.start)?;
        if source_len == 0 {
            return None;
        }

        let line_start = line.columns.start;
        let line_end = line.columns.end;
        if output_columns.start < line_start || output_columns.end > line_end {
            return None;
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
        mut flags: libc::c_int,
        allow_wrap: bool,
        abort_on_invalid: bool,
    ) -> Option<CompositorSubmission> {
        if self.characters.get(line.columns.end - 1) == Some(&0) {
            line.skip_end = 0;
        }
        if !allow_wrap {
            flags &= !LINE_FLAG_WRAP;
        }

        let start_col = line.columns.start.checked_add(line.skip_start)?;
        let end_col = line.columns.end.checked_sub(line.skip_end)?;
        for attribute in self.attributes.get_mut(start_col..end_col)? {
            if *attribute < 0 {
                if abort_on_invalid {
                    std::process::abort();
                }
                *attribute = 0;
            }
        }

        Some(CompositorSubmission {
            start_col,
            end_col,
            flags,
        })
    }

    /// Borrows one output segment through the existing compositor abstraction.
    fn try_segment_mut(&mut self, columns: Range<usize>) -> Option<CompositorSegment<'_>> {
        let characters = self.characters.get_mut(columns.clone())?;
        let attributes = self.attributes.get_mut(columns)?;
        CompositorSegment::try_from_slices(characters, attributes)
    }

    /// Resolves wide-cell clipping at one selected segment boundary.
    fn resolve_segment_edges(
        &mut self,
        line: &mut CompositorLine,
        output_columns: Range<usize>,
        followed_by_continuation: bool,
    ) -> Option<()> {
        let source_len = output_columns.end.checked_sub(output_columns.start)?;
        let line_start = line.columns.start;
        let line_end = line.columns.end;
        let mut include_start =
            followed_by_continuation && output_columns.start == line_start && source_len == 1;
        let mut include_end = false;

        let first_character = self.characters.get_mut(output_columns.start)?;
        if *first_character == 0 {
            *first_character = schar_from_ascii(' ');
            include_end = output_columns.start + 1 == line_end;
        } else if output_columns.start == line_start
            && source_len > 1
            && self.characters.get(output_columns.start + 1) == Some(&0)
        {
            include_start = true;
        }

        if include_start {
            line.skip_start = 0;
        }
        if include_end {
            line.skip_end = 0;
        }
        Some(())
    }
}

static COMPOSITOR_LAYER_STATE: Mutex<Option<CompositorLayerFfiState>> = Mutex::new(None);

/// Acquires exclusive access to the transitional compositor boundary state.
fn lock_compositor_layer_state() -> MutexGuard<'static, Option<CompositorLayerFfiState>> {
    COMPOSITOR_LAYER_STATE
        .try_lock()
        .expect("compositor layer state is already borrowed")
}

/// Returns an owned snapshot without retaining the registry lock.
fn compositor_layer_registry() -> Option<CompositorLayerRegistry> {
    lock_compositor_layer_state()
        .as_ref()
        .map(|state| state.registry.clone())
}

/// Replaces the current registry if it still matches the supplied snapshot.
fn replace_compositor_layer_registry(
    expected: &CompositorLayerRegistry,
    replacement: CompositorLayerRegistry,
) -> Option<()> {
    let mut current_state = lock_compositor_layer_state();
    let state = current_state.as_mut()?;
    if &state.registry != expected || !state.c_grids.matches_registry(&replacement) {
        return None;
    }

    state.registry = replacement;
    Some(())
}

/// Replaces the complete boundary state if its registry snapshot still
/// matches.
fn replace_compositor_layer_state(
    expected: &CompositorLayerRegistry,
    replacement: CompositorLayerFfiState,
) -> Option<()> {
    let mut current_state = lock_compositor_layer_state();
    if &current_state.as_ref()?.registry != expected {
        return None;
    }

    *current_state = Some(replacement);
    Some(())
}

/// Initializes compositor layer ownership and its default-grid binding.
///
/// # Safety
///
/// `default_grid` must point to a valid, exclusively accessible C grid whose
/// buffers satisfy the ownership and layout requirements of `ScreenGrid`.
unsafe fn initialize_layer_registry(default_grid: *mut CScreenGrid) -> bool {
    let Some(default_cgrid) = NonNull::new(default_grid) else {
        return false;
    };

    let default_grid_id = {
        // SAFETY: The caller transfers exclusive ownership for this scope.
        let default_grid = unsafe { ScreenGridGuard::take(default_grid) };
        default_grid.id()
    };
    let Some(registry) = CompositorLayerRegistry::try_new(vec![default_grid_id]) else {
        return false;
    };
    let Some(c_grids) = CCompositorGridBindings::try_new(HashMap::from([(
        default_grid_id,
        CCompositorGridBinding {
            cgrid: default_cgrid,
        },
    )])) else {
        return false;
    };
    let Some(state) = CompositorLayerFfiState::try_new(registry, c_grids) else {
        return false;
    };

    let mut current_state = lock_compositor_layer_state();
    if current_state.is_some() {
        return false;
    }
    *current_state = Some(state);
    true
}

/// Clears compositor layer order and temporary C bindings during teardown.
fn clear_layer_registry() -> bool {
    lock_compositor_layer_state().take().is_some()
}

static MESSAGE_COMPOSITOR_STATE: Mutex<MessageCompositorState> =
    Mutex::new(MessageCompositorState::new());

/// Acquires exclusive access to the process-global message compositor state.
fn lock_message_compositor_state() -> MutexGuard<'static, MessageCompositorState> {
    MESSAGE_COMPOSITOR_STATE
        .try_lock()
        .expect("message compositor state is already borrowed")
}

static COMPOSITOR_BUFFER: Mutex<CompositorBuffer> = Mutex::new(CompositorBuffer::new());

/// Acquires exclusive access to the process-global compositor buffer.
fn lock_compositor_buffer() -> MutexGuard<'static, CompositorBuffer> {
    COMPOSITOR_BUFFER
        .try_lock()
        .expect("compositor buffer is already borrowed")
}

static COMPOSITOR_DEBUG_HIGHLIGHTS: Mutex<CompositorDebugHighlights> =
    Mutex::new(CompositorDebugHighlights::empty());

/// Acquires exclusive access to compositor debug-highlight identities.
fn lock_compositor_debug_highlights() -> MutexGuard<'static, CompositorDebugHighlights> {
    COMPOSITOR_DEBUG_HIGHLIGHTS
        .try_lock()
        .expect("compositor debug highlights are already borrowed")
}

/// Initializes the compositor debug-highlight identities in Rust-owned state.
fn initialize_debug_highlights() {
    let highlights = CompositorDebugHighlights {
        normal: compositor_debug_highlight(b"RedrawDebugNormal"),
        clear: compositor_debug_highlight(b"RedrawDebugClear"),
        composed: compositor_debug_highlight(b"RedrawDebugComposed"),
        recomposed: compositor_debug_highlight(b"RedrawDebugRecompose"),
    };
    *lock_compositor_debug_highlights() = highlights;
}

static COMPOSITOR_DRAW_STATE: Mutex<CompositorDrawState> = Mutex::new(CompositorDrawState::new());

/// Acquires exclusive access to the process-global compositor draw state.
fn lock_compositor_draw_state() -> MutexGuard<'static, CompositorDrawState> {
    COMPOSITOR_DRAW_STATE
        .try_lock()
        .expect("compositor draw state is already borrowed")
}

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
    assert!(lock_compositor_buffer().release());
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
        assert!(lock_compositor_buffer().release());
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

    // SAFETY: The dimensions were converted and validated above. The callback
    // runs before buffer resize, preserving the established event order.
    unsafe {
        ui_composed_call_grid_resize(1, Integer::from(size.width), Integer::from(size.height));
    }

    let width = usize::try_from(size.width).expect("validated width must fit usize");
    assert!(lock_compositor_buffer().resize(width));
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
        // SAFETY: Both grid guards were dropped before this synchronous UI
        // call, and the safe plan contains only validated scalar values.
        unsafe {
            ui_composed_call_grid_scroll(
                1,
                Integer::from(scroll.region.rows.start),
                Integer::from(scroll.region.rows.end),
                Integer::from(scroll.region.columns.start),
                Integer::from(scroll.region.columns.end),
                Integer::from(scroll.row_delta),
                0,
            );
        }
    }
    if let Some(area) = plan.recompose {
        // SAFETY: The caller satisfies the helper's message-grid requirements.
        // The earlier guards were dropped above.
        if !unsafe { recompose_area_from_registered_grids(message_grid, area, runtime) } {
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

    // SAFETY: Neovim's global default and message grids are distinct and are
    // exclusively accessed on the main thread during this generated event.
    let updated = unsafe {
        update_message_position(
            &raw mut c_msg_grid,
            &raw mut c_default_grid,
            request,
            runtime,
        )
    };
    assert!(updated);
}

/// Clamps a recomposition area's exclusive ends to the default grid.
///
/// The returned area contains only owned Rust values. Grid ownership is
/// restored before returning.
fn clip_recompose_area_to_default_grid(
    area: CompositorRecomposeArea,
) -> Option<CompositorRecomposeArea> {
    let layer_guards = CompositorLayerGuards::try_take_registered()?;
    area.with_ends_clamped_to_grid(layer_guards.grid(0)?)
}

/// Returns the screen area occupied by one registered grid.
fn recompose_area_for_registered_grid(grid_id: GridId) -> Option<CompositorRecomposeArea> {
    let layer_guards = CompositorLayerGuards::try_take_registered()?;
    CompositorRecomposeArea::try_from_grid(layer_guards.grid_by_id(grid_id)?)
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

    // SAFETY: The grid guard was dropped above. The process-global message
    // grid is exclusively accessed on Neovim's main thread.
    assert!(unsafe { recompose_area_from_registered_grids(&raw mut c_msg_grid, area, runtime) });
}

/// Places a new or existing grid among the registered compositor layers.
///
/// A new binding is published before recomposition. An existing grid is
/// restored to its requested placement even if recomposing an exposed area
/// fails. The callback may reenter the compositor because all grid guards are
/// restored before it is called.
fn place_grid_in_registered_layers(
    layer_guards: CompositorLayerGuards,
    target: CompositorGridPlacementTarget,
    current_window_layer: Option<usize>,
    request: CompositorGridPlacementRequest,
    should_draw: bool,
    mut recompose_area: impl FnMut(CompositorRecomposeArea) -> bool,
) -> Option<bool> {
    let plan = {
        let mut layer_guards = layer_guards;
        if target.was_registered {
            if target.layer_index == 0 {
                return None;
            }
            let plan = {
                let layers = layer_guards.layer_stack()?;
                let layer = layers.layer(target.layer_index)?;
                layer.try_plan_placement(request.placement, request.valid, should_draw)?
            };

            let grid = layer_guards.grid_mut(target.layer_index)?;
            grid.mark_compositor_index_for_update();
            if should_draw {
                grid.set_compositor_disabled(true);
            } else {
                request.placement.apply_to(grid);
            }
            plan
        } else {
            if target.layer_index.checked_add(1)? != layer_guards.registry.len() {
                return None;
            }
            let (insertion_index, plan) = {
                let grid = layer_guards.grid(target.layer_index)?;
                let layers = layer_guards.layer_stack_prefix(target.layer_index)?;
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

            let previous_registry = layer_guards.registry.prefix(target.layer_index)?;

            layer_guards.place_appended_layer(insertion_index, request.placement)?;
            let replacement_state = layer_guards.ffi_state()?;
            replace_compositor_layer_state(&previous_registry, replacement_state)?;
            plan
        }
    };

    let mut exposed_areas_composed = true;
    for area in plan.exposed_areas.iter().cloned() {
        if !recompose_area(area) {
            exposed_areas_composed = false;
            break;
        }
    }

    if should_draw && target.was_registered {
        let mut layer_guards = CompositorLayerGuards::try_take_registered()?;
        let layer_index = layer_guards.registry.index_of(target.grid_id)?;
        let grid = layer_guards.grid_mut(layer_index)?;
        grid.set_compositor_disabled(false);
        plan.placement.apply_to(grid);
    }

    if !exposed_areas_composed {
        return None;
    }

    if let Some(area) = plan.redraw_area
        && !recompose_area(area)
    {
        return None;
    }

    Some(plan.moved)
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
    let recompose_area = |area| {
        // SAFETY: Every grid guard is dropped before this closure. The
        // process-global message grid is accessed on Neovim's main thread.
        unsafe { recompose_area_from_registered_grids(&raw mut c_msg_grid, area, runtime) }
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

/// Applies one validated cursor request to the registered compositor grids.
///
/// The returned update contains only owned Rust values. Every grid is restored
/// before this function returns.
fn apply_cursor_update_to_registered_grids(
    request: CompositorCursorRequest,
) -> Option<CompositorCursorUpdate> {
    let mut layer_guards = CompositorLayerGuards::try_take_registered()?;
    let action = {
        let layers = layer_guards.layer_stack()?;
        layers.plan_cursor(request)?
    };
    let CompositorCursorAction::Update(plan) = action else {
        return Some(CompositorCursorUpdate::Ignore);
    };

    let recompose_areas = if let Some(target_index) = plan.raise_to {
        let raise_plan = {
            let layers = layer_guards.layer_stack()?;
            layers.plan_layer_raise(plan.layer_index, target_index)?
        };
        let previous_registry = layer_guards.registry.clone();
        layer_guards.apply_layer_move(raise_plan.movement)?;

        replace_compositor_layer_registry(&previous_registry, layer_guards.registry.clone())?;
        raise_plan.recompose_areas
    } else {
        Vec::new()
    };

    Some(CompositorCursorUpdate::Apply {
        plan,
        recompose_areas,
    })
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
        if !unsafe { recompose_area_from_registered_grids(message_grid, area, runtime) } {
            return false;
        }
    }

    if plan.emit {
        // SAFETY: The layer guards were dropped above, and the planned
        // coordinates were validated by the safe compositor layer stack.
        unsafe {
            ui_composed_call_grid_cursor_goto(
                1,
                Integer::from(plan.screen_position.row),
                Integer::from(plan.screen_position.column),
            );
        }
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

    // SAFETY: Neovim's message grid is exclusively accessed on the main
    // thread during this generated event.
    let updated = unsafe { process_grid_cursor_goto(&raw mut c_msg_grid, request) };
    assert!(updated);
}

/// Adjusts a registered grid's layer position after its z-index changes.
///
/// A grid that is not currently registered is accepted as a no-op, matching
/// the previous C-side `comp_index == 0` check.
///
/// Every temporarily owned grid is restored before this function returns.
fn adjust_registered_layer_order(grid_id: GridId, adjustment: CompositorLayerAdjustment) -> bool {
    let Some(mut layer_guards) = CompositorLayerGuards::try_take_registered() else {
        return false;
    };
    let Some(layer_index) = layer_guards.registry.index_of(grid_id) else {
        return true;
    };
    let movement = {
        let Some(layers) = layer_guards.layer_stack() else {
            return false;
        };
        let Some(movement) = layers.plan_layer_adjustment(layer_index, adjustment) else {
            return false;
        };
        movement
    };
    let previous_registry = layer_guards.registry.clone();
    if layer_guards.apply_layer_move(movement).is_none() {
        return false;
    }

    replace_compositor_layer_registry(&previous_registry, layer_guards.registry.clone()).is_some()
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

/// Removes one grid from the registered compositor layers.
///
/// The returned result contains only owned Rust values. Every grid is restored
/// before this function returns.
fn remove_registered_layer(
    mut layer_guards: CompositorLayerGuards,
    layer_index: usize,
    should_draw: bool,
) -> Option<CompositorLayerRemoval> {
    if layer_index == 0 {
        return None;
    }

    let recompose_area = if should_draw {
        Some(CompositorRecomposeArea::try_from_grid(
            layer_guards.grid(layer_index)?,
        )?)
    } else {
        None
    };

    let previous_registry = layer_guards.registry.clone();
    layer_guards.remove_layer(layer_index)?;
    let remaining_state = layer_guards.ffi_state()?;
    replace_compositor_layer_state(&previous_registry, remaining_state)?;

    Some(CompositorLayerRemoval { recompose_area })
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
        // SAFETY: The reduced registry has been published, and the caller
        // accesses the process-global message grid on Neovim's main thread.
        assert!(unsafe {
            recompose_area_from_registered_grids(&raw mut c_msg_grid, area, runtime)
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
    let Some(cgrid) = grid_id.and_then(|grid_id| {
        layer_guards
            .bindings
            .get(&grid_id)
            .map(|binding| binding.cgrid.as_ptr())
    }) else {
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

/// Checks cursor coverage using the registered compositor layers.
fn cursor_is_covered_by_registered_layers(
    current_grid: CompositorCursorGrid,
    row: i32,
    column: i32,
    minimum_z_index_gap: u32,
) -> Option<bool> {
    let layer_guards = CompositorLayerGuards::try_take_registered()?;
    let layers = layer_guards.layer_stack()?;
    let point = CompositorPoint { row, column };

    Some(layers.is_cursor_covered_by_higher_layer(point, current_grid, minimum_z_index_gap))
}

/// Reports whether a sufficiently higher compositor layer covers the cursor.
#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_cursor_is_covered_by_higher_layer_in_stack(
    current_grid_handle: Integer,
    current_grid_z_index: libc::c_int,
    row: libc::c_int,
    column: libc::c_int,
    minimum_z_index_gap: libc::c_int,
) -> bool {
    if !lock_compositor_draw_state().should_draw() {
        return false;
    }
    let (Ok(current_grid_handle), Ok(minimum_z_index_gap)) = (
        i32::try_from(current_grid_handle),
        u32::try_from(minimum_z_index_gap),
    ) else {
        return false;
    };

    cursor_is_covered_by_registered_layers(
        CompositorCursorGrid {
            grid_id: GridId::new(current_grid_handle),
            z_index: current_grid_z_index,
        },
        row,
        column,
        minimum_z_index_gap,
    )
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
) -> bool {
    let separator_character = lock_message_compositor_state().separator_character_at(request.row);
    let separator_active = separator_character.is_some();
    if separator_active && message_grid.is_null() {
        return false;
    }

    let mut compositor = lock_compositor_buffer();
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
    let characters = unsafe { compositor.characters.as_ptr().add(submission.start_col) };
    let attributes = unsafe { compositor.attributes.as_ptr().add(submission.start_col) };

    // SAFETY: All grid guards have restored their C grids. Both pointers remain
    // valid for the synchronous UI call while `compositor` remains borrowed
    // and describe equally sized initialized ranges.
    unsafe {
        ui_composed_call_raw_line(
            1,
            Integer::from(request.row),
            submission.start_col as Integer,
            submission.end_col as Integer,
            submission.end_col as Integer,
            0,
            submission.flags,
            characters,
            attributes,
        );
    }
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
) -> bool {
    runtime.visualize(&area, runtime.debug_highlights.recomposed, true);

    // SAFETY: The caller satisfies the helper's message-grid requirements.
    // Debug visualization completed without retaining any grid guard.
    unsafe {
        compose_area_from_registered_grids(
            message_grid,
            CompositorAreaRequest {
                area,
                settings: runtime.line_settings,
            },
        )
    }
}

/// Emits one clear raw-line event used for compositor visualization.
fn emit_compositor_debug_line(row: i32, columns: Range<i32>, attribute: VimSAttrT) -> bool {
    let (Ok(start_column), Ok(end_column)) =
        (usize::try_from(columns.start), usize::try_from(columns.end))
    else {
        return false;
    };

    let compositor = lock_compositor_buffer();
    if start_column > end_column || end_column > compositor.characters.len() {
        return false;
    }

    let characters = unsafe { compositor.characters.as_ptr().add(start_column) };
    let attributes = unsafe { compositor.attributes.as_ptr().add(start_column) };

    // SAFETY: The raw-line event contains no character payload because its
    // start and end columns are equal. The pointers nevertheless remain valid
    // for the synchronous UI call.
    unsafe {
        ui_composed_call_raw_line(
            1,
            Integer::from(row),
            Integer::from(columns.start),
            Integer::from(columns.start),
            Integer::from(columns.end),
            Integer::from(attribute),
            0,
            characters,
            attributes,
        );
    }
    true
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
        );
    }

    match plan.action {
        CompositorRawLineAction::DiscardRow => {
            log_raw_line_diagnostic(RawLineDiagnostic::InvalidRow(plan.row), grid_id);
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
            );
            runtime.visualize(
                &CompositorRecomposeArea {
                    rows: plan.row..plan.row + 1,
                    columns: plan.columns.end..plan.clear_column,
                },
                runtime.debug_highlights.clear,
                true,
            );
            debug_assert!(attributes.iter().all(|attribute| *attribute >= 0));

            // SAFETY: The validated slices have equal lengths and remain
            // readable for the duration of the synchronous UI call.
            unsafe {
                ui_composed_call_raw_line(
                    1,
                    Integer::from(plan.row),
                    Integer::from(plan.columns.start),
                    Integer::from(plan.columns.end),
                    Integer::from(plan.clear_column),
                    Integer::from(clear_attribute),
                    plan.flags,
                    characters.as_ptr(),
                    attributes.as_ptr(),
                );
            }
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
            // SAFETY: Grid ownership was restored before the synchronous UI
            // call, and the validated operation contains only scalar values.
            unsafe {
                ui_composed_call_grid_scroll(
                    1,
                    Integer::from(region.rows.start),
                    Integer::from(region.rows.end),
                    Integer::from(region.columns.start),
                    Integer::from(region.columns.end),
                    Integer::from(row_delta),
                    Integer::from(column_delta),
                );
            }
            if runtime.debug_enabled {
                runtime.delay(2);
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

    // SAFETY: Neovim's message grid is exclusively accessed on the main
    // thread during this generated event.
    let processed = unsafe { process_scroll(&raw mut c_msg_grid, GridId::new(grid), request) };
    assert!(processed);
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
        let layer_guards = unsafe {
            CompositorLayerGuards::try_take_in_binding_order(cgrids.as_ptr(), cgrids.len())
        }
        .expect("test compositor layers must be valid");
        let state = layer_guards
            .ffi_state()
            .expect("test compositor layer state must be valid");
        drop(layer_guards);

        let mut current_state = lock_compositor_layer_state();
        assert!(current_state.is_none());
        *current_state = Some(state);
        drop(current_state);

        TestCompositorLayerRegistry { _lock: test_lock }
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
        assert_eq!(registry.remove_non_default(0), None);
        assert_eq!(registry.remove_non_default(1), Some(third));
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
            origin_row: 0,
            origin_column,
            requested_width: visible_width,
            requested_height: grid.rows,
            visible_width,
            visible_height: grid.rows,
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
            screen_size: CompositorSize {
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
                screen_size: CompositorSize {
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
    fn compositor_draw_state_requires_an_attached_ui_and_valid_screen() {
        let mut state = CompositorDrawState::new();
        assert!(!state.should_draw());

        state.register_ui();
        state.register_ui();
        assert!(state.should_draw());

        assert!(state.set_screen_valid(false));
        assert!(!state.should_draw());
        assert!(!state.set_screen_valid(true));
        assert!(state.should_draw());

        assert_eq!(state.unregister_ui(), Some(false));
        assert!(state.should_draw());
        assert_eq!(state.unregister_ui(), Some(true));
        assert!(!state.should_draw());
        assert_eq!(state.unregister_ui(), None);
    }

    #[test]
    fn moving_message_grid_down_recomposes_the_exposed_rows() {
        let mut state = MessageCompositorState::new();
        let mut message_grid = grid_with_size(10, 20);
        let default_grid = grid_with_size(10, 18);
        assert_eq!(
            state.update_position(
                &mut message_grid,
                &default_grid,
                message_position_request(5, false),
                false,
            ),
            Some(CompositorMessagePositionPlan::default())
        );

        let plan = state
            .update_position(
                &mut message_grid,
                &default_grid,
                message_position_request(7, false),
                true,
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
        let mut state = MessageCompositorState::new();
        let mut message_grid = grid_with_size(10, 20);
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
            .update_position(
                &mut message_grid,
                &default_grid,
                message_position_request(2, true),
                true,
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorMessagePositionPlan {
                scroll: Some(MessageGridScroll {
                    region: CompositorRegion::try_new(2..10, 0..20).unwrap(),
                    row_delta: 3,
                }),
                recompose: Some(CompositorRecomposeArea {
                    rows: 1..2,
                    columns: 0..20,
                }),
            }
        );
        assert_eq!(state.separator_character_at(1), Some(schar_from_ascii('-')));
        state.hide_separator();
        assert_eq!(state.separator_character_at(1), None);
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
            Some(CompositorRecomposeArea {
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

            assert_eq!(
                recompose_area_for_registered_grid(grid_id),
                Some(CompositorRecomposeArea {
                    rows: 2..5,
                    columns: 5..9,
                })
            );
            assert_eq!(
                clip_recompose_area_to_default_grid(CompositorRecomposeArea {
                    rows: 1..5,
                    columns: 2..6,
                }),
                Some(CompositorRecomposeArea {
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
            let guards = unsafe {
                CompositorLayerGuards::try_take_for_registry(
                    cgrids.as_ptr(),
                    cgrids.len(),
                    registry,
                )
            }
            .unwrap();
            assert_eq!(guards.index_of(first_pointer), Some(0));
            assert_eq!(guards.index_of(second_pointer), Some(1));
            assert_eq!(
                guards.grid(0).unwrap().chars.as_slice(),
                &[schar_from_ascii('a')]
            );
            assert_eq!(
                guards.grid(1).unwrap().chars.as_slice(),
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
            Some(CompositorLayerMove {
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

            assert!(adjust_registered_layer_order(
                moving_grid_id,
                CompositorLayerAdjustment::Raise,
            ));
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
        layer.origin_row = 2;
        layer.requested_height = 3;
        let placement = CompositorGridPlacement::try_new(3, 5, 2, 2).unwrap();

        assert_eq!(
            layer.try_plan_placement(placement, true, true),
            Some(CompositorGridPlacementPlan {
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
        layer.origin_row = 2;
        layer.requested_height = 3;
        let placement = CompositorGridPlacement::try_new(2, 3, 2, 2).unwrap();

        assert_eq!(
            layer.try_plan_placement(placement, true, true),
            Some(CompositorGridPlacementPlan {
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
            Some(CompositorGridPlacementPlan {
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

        assert_eq!(layers.plan_layer_insertion(10, Some(1), true), Some(2));
        assert_eq!(layers.plan_layer_insertion(10, Some(1), false), Some(1));
        assert_eq!(layers.plan_layer_insertion(30, Some(1), true), Some(3));
        assert_eq!(layers.plan_layer_insertion(-1, None, true), None);
    }

    #[test]
    fn cursor_planning_selects_translates_and_raises_a_grid() {
        let default_grid = grid_with_size(10, 12);
        let mut selected_grid = grid_with_size(2, 2);
        selected_grid.handle = 7;
        let upper_grid = grid_with_size(2, 2);
        let mut selected_layer = compositor_layer(&selected_grid, 3, 2, 1, false);
        selected_layer.origin_row = 2;
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
                local_position: CompositorPoint { row: 1, column: 2 },
            }),
            Some(CompositorCursorAction::Update(CompositorCursorPlan {
                layer_index: 1,
                screen_position: CompositorPoint { row: 3, column: 5 },
                raise_to: Some(2),
                emit: true,
            }))
        );
        assert_eq!(
            layers.plan_cursor(CompositorCursorRequest {
                grid_id: GridId::new(7),
                local_position: CompositorPoint { row: 10, column: 2 },
            }),
            Some(CompositorCursorAction::Update(CompositorCursorPlan {
                layer_index: 1,
                screen_position: CompositorPoint { row: 12, column: 5 },
                raise_to: Some(2),
                emit: false,
            }))
        );
        assert_eq!(
            layers.plan_cursor(CompositorCursorRequest {
                grid_id: GridId::new(99),
                local_position: CompositorPoint { row: 0, column: 0 },
            }),
            Some(CompositorCursorAction::Ignore)
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
                local_position: CompositorPoint { row: 0, column: 0 },
            }),
            Some(CompositorCursorUpdate::Ignore)
        );
        assert_eq!(cgrids, [default_pointer, selected_pointer, upper_pointer]);

        // The cursor lies outside the default grid, so production code will
        // recompose the exposed area before suppressing cursor output.
        assert_eq!(
            apply_cursor_update_to_registered_grids(CompositorCursorRequest {
                grid_id: GridId::new(7),
                local_position: CompositorPoint { row: 10, column: 0 },
            }),
            Some(CompositorCursorUpdate::Apply {
                plan: CompositorCursorPlan {
                    layer_index: 1,
                    screen_position: CompositorPoint { row: 12, column: 3 },
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

        assert_eq!(layers.plan_layer_insertion(0, Some(0), false), Some(1));
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

        // SAFETY: The candidate is already registered.
        let (layer_guards, target) = unsafe {
            CompositorLayerGuards::try_take_registered_with_candidate(
                NonNull::new(moving_pointer).unwrap(),
            )
        }
        .unwrap();
        assert_eq!(
            place_grid_in_registered_layers(
                layer_guards,
                target,
                None,
                CompositorGridPlacementRequest {
                    placement: CompositorGridPlacement::try_new(1, 2, 2, 2).unwrap(),
                    valid: true,
                    on_top: false,
                },
                true,
                |area| {
                    recomposed_areas.push(area);
                    true
                },
            ),
            Some(true)
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

        // SAFETY: The candidate is distinct from every registered grid and is
        // exclusively accessible.
        let (layer_guards, target) = unsafe {
            CompositorLayerGuards::try_take_registered_with_candidate(
                NonNull::new(appended_pointer).unwrap(),
            )
        }
        .unwrap();
        let current_window_layer = layer_guards.registry.index_of(current_grid_id);
        assert_eq!(
            place_grid_in_registered_layers(
                layer_guards,
                target,
                current_window_layer,
                CompositorGridPlacementRequest {
                    placement: CompositorGridPlacement::try_new(1, 2, 2, 2).unwrap(),
                    valid: true,
                    on_top: false,
                },
                true,
                |area| {
                    recomposed_areas.push(area);
                    true
                },
            ),
            Some(true)
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
            Some(CompositorLayerMove {
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
        moving_layer.origin_row = 1;
        let mut first_crossed_layer = compositor_layer(&first_crossed_grid, 0, 2, 2, false);
        first_crossed_layer.origin_row = 2;
        let mut second_crossed_layer = compositor_layer(&second_crossed_grid, 3, 2, 3, false);
        second_crossed_layer.origin_row = 0;
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
            Some(CompositorLayerRaisePlan {
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

        assert_eq!(layers.plan_layer_raise(0, 1), None);
        assert_eq!(layers.plan_layer_raise(2, 1), None);
        assert_eq!(layers.plan_layer_raise(1, 3), None);
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
            let mut guards = unsafe {
                CompositorLayerGuards::try_take_in_binding_order(cgrids.as_ptr(), cgrids.len())
            }
            .unwrap();
            guards
                .apply_layer_move(CompositorLayerMove {
                    source_index: 2,
                    target_index: 1,
                })
                .unwrap();
            assert_eq!(guards.registry.layer_ids(), &expected_registry);
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
            let mut guards = unsafe {
                CompositorLayerGuards::try_take_in_binding_order(cgrids.as_ptr(), cgrids.len())
            }
            .unwrap();
            assert_eq!(guards.remove_layer(0), None);
            assert_eq!(guards.remove_layer(1), Some(()));
            assert_eq!(guards.registry.layer_ids(), &expected_registry);
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

        let layer_guards = CompositorLayerGuards::try_take_registered().unwrap();
        let layer_index = layer_guards.registry.index_of(removed_grid_id).unwrap();
        assert_eq!(
            remove_registered_layer(layer_guards, layer_index, true),
            Some(CompositorLayerRemoval {
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
            let layer_guards = CompositorLayerGuards::try_take_registered().unwrap();
            assert_eq!(layer_guards.registry.index_of(removed_grid_id), None);
            assert_eq!(layer_guards.index_of(removed_pointer), None);
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
        lower_layer.origin_row = 1;
        let mut upper_layer = compositor_layer(&upper_grid, 1, 1, 2, false);
        upper_layer.origin_row = 1;
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 4, 0, false),
                lower_layer,
                upper_layer,
            ],
        };

        assert_eq!(
            layers.topmost_grid_at(CompositorPoint { row: 2, column: 2 }, LayerHitTest::Any,),
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
                .topmost_grid_at(CompositorPoint { row: 1, column: 1 }, LayerHitTest::Any,)
                .is_none()
        );
    }

    #[test]
    fn positioned_grid_uses_stored_compositor_position() {
        let mut grid = grid_with_size(2, 3);
        grid.set_compositor_position(4, 5);
        let positioned = PositionedGrid::try_from_compositor_position(&grid).unwrap();

        assert!(positioned.contains(CompositorPoint { row: 4, column: 5 }));
        assert!(positioned.contains(CompositorPoint { row: 5, column: 7 }));
        assert!(!positioned.contains(CompositorPoint { row: 3, column: 5 }));
        assert!(!positioned.contains(CompositorPoint { row: 6, column: 5 }));
        assert!(!positioned.contains(CompositorPoint { row: 5, column: 8 }));
    }

    #[test]
    fn positioned_grid_hit_testing_uses_external_half_open_bounds() {
        let grid = grid_with_size(2, 3);
        let positioned =
            PositionedGrid::try_new(&grid, CompositorPoint { row: 4, column: 5 }).unwrap();

        assert!(positioned.matches(
            CompositorPoint { row: 4, column: 5 },
            LayerHitTest::MouseEnabled,
        ));
        assert!(positioned.matches(
            CompositorPoint { row: 5, column: 7 },
            LayerHitTest::MouseEnabled,
        ));
        assert!(!positioned.matches(
            CompositorPoint { row: 6, column: 5 },
            LayerHitTest::MouseEnabled,
        ));
        assert!(!positioned.matches(
            CompositorPoint { row: 5, column: 8 },
            LayerHitTest::MouseEnabled,
        ));
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
        let point = CompositorPoint { row: 0, column: 1 };

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
        overlay_layer.origin_row = 2;
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 3, 0, false),
                overlay_layer,
            ],
        };

        assert!(
            layers
                .topmost_grid_at(CompositorPoint { row: 0, column: 0 }, LayerHitTest::Any,)
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
        let covered_point = CompositorPoint { row: 0, column: 1 };

        let current_cursor_grid = CompositorCursorGrid::from(&current_grid);
        let upper_cursor_grid = CompositorCursorGrid::from(&upper_grid);

        assert!(layers.is_cursor_covered_by_higher_layer(covered_point, current_cursor_grid, 0));
        assert!(!layers.is_cursor_covered_by_higher_layer(covered_point, current_cursor_grid, 1));
        assert!(!layers.is_cursor_covered_by_higher_layer(covered_point, upper_cursor_grid, 0));
        assert!(!layers.is_cursor_covered_by_higher_layer(
            CompositorPoint { row: 2, column: 2 },
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

            assert_eq!(
                cursor_is_covered_by_registered_layers(current_cursor_grid, 0, 0, 0),
                Some(true)
            );
            assert_eq!(
                cursor_is_covered_by_registered_layers(external_cursor_grid, 0, 0, 0),
                Some(true)
            );
            assert_eq!(
                cursor_is_covered_by_registered_layers(current_cursor_grid, 0, 0, 1),
                Some(false)
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
        assert!(
            short_output
                .try_copy(
                    GridRow::try_from_grid(&foreground, 0)
                        .unwrap()
                        .segment(0..2)
                        .unwrap()
                )
                .is_none()
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
        let region = CompositorRegion::try_new(4..7, 0..2).unwrap();
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
        overlay.origin_row = 2;
        overlay.requested_width = 3;
        overlay.requested_height = 2;
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
            Some(true)
        );
        assert_eq!(
            layers.is_covered_above(CompositorCoveredAboveQuery {
                rectangle,
                current_layer_index: 1,
                message_layer_index: None,
                message_was_scrolled: false,
            }),
            Some(false)
        );
    }

    #[test]
    fn scrolled_message_layer_covers_separator_row() {
        let default_grid = grid_with_row(&[schar_from_ascii('a')], &[1]);
        let message_grid = grid_with_row(&[schar_from_ascii('m')], &[2]);
        let mut message = compositor_layer(&message_grid, 1, 1, 1, false);
        message.origin_row = 2;
        message.requested_width = 2;
        message.requested_height = 1;
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

        assert_eq!(layers.is_covered_above(query), Some(false));
        assert_eq!(
            layers.is_covered_above(CompositorCoveredAboveQuery {
                message_was_scrolled: true,
                ..query
            }),
            Some(true)
        );
    }

    #[test]
    fn plans_forwarded_raw_line_in_composed_screen_coordinates() {
        let default_grid = grid_with_size(5, 10);
        let current_grid = grid_with_size(2, 5);
        let mut current_layer = compositor_layer(&current_grid, 3, 5, 1, false);
        current_layer.origin_row = 1;
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
                    flags: LINE_FLAG_WRAP,
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
                flags: 0,
                action: CompositorRawLineAction::Forward,
            }
        );
    }

    #[test]
    fn rejects_unordered_raw_line_columns() {
        assert!(CompositorRawLineRequest::try_new(0, 2, 1, 3, 0).is_none());
        assert!(CompositorRawLineRequest::try_new(0, 1, 3, 2, 0).is_none());
    }

    #[test]
    fn plans_clipped_raw_line_recomposition() {
        let default_grid = grid_with_size(5, 8);
        let overlay_grid = grid_with_size(1, 1);
        let mut overlay = compositor_layer(&overlay_grid, 6, 1, 1, false);
        overlay.origin_row = 1;
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
                    flags: LINE_FLAG_WRAP,
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
                flags: LINE_FLAG_WRAP,
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
                    flags: 0,
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
                    flags: 0,
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
        layer.origin_row = 3;
        let layers = CompositorLayerStack {
            layers: vec![layer],
        };

        let plan = layers
            .plan_scroll(
                0,
                None,
                false,
                CompositorScrollRequest {
                    region: CompositorRegion::try_new(2..7, 1..5).unwrap(),
                    row_delta: 2,
                    column_delta: -1,
                },
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorScrollPlan::Forward {
                region: CompositorRegion::try_new(5..10, 5..9).unwrap(),
                row_delta: 2,
                column_delta: -1,
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
        default_layer.origin_row = 3;
        let mut overlay = compositor_layer(&overlay_grid, 6, 1, 1, false);
        overlay.origin_row = 6;
        overlay.requested_width = 1;
        overlay.requested_height = 1;
        let layers = CompositorLayerStack {
            layers: vec![default_layer, overlay],
        };

        let plan = layers
            .plan_scroll(
                0,
                None,
                false,
                CompositorScrollRequest {
                    region: CompositorRegion::try_new(2..7, 1..5).unwrap(),
                    row_delta: 2,
                    column_delta: 0,
                },
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorScrollPlan::Recompose {
                region: CompositorRegion::try_new(5..10, 5..9).unwrap(),
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
        layer.origin_row = 3;
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
                    region: CompositorRegion::try_new(2..7, 1..5).unwrap(),
                    row_delta: -2,
                    column_delta: 0,
                },
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorScrollPlan::Recompose {
                region: CompositorRegion::try_new(5..10, 5..9).unwrap(),
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

        assert!(
            output
                .try_compose(
                    GridRow::try_from_grid(&background, 0)
                        .unwrap()
                        .segment(0..2)
                        .unwrap(),
                    GridRow::try_from_grid(&foreground, 0)
                        .unwrap()
                        .segment(0..1)
                        .unwrap(),
                    false,
                )
                .is_none()
        );
    }

    #[test]
    fn owned_buffer_composes_grid_segments() {
        let foreground = grid_with_row(&[schar_from_ascii('x'), schar_from_ascii('y')], &[1, -1]);
        let foreground = GridRow::try_from_grid(&foreground, 0)
            .unwrap()
            .segment(0..2)
            .unwrap();
        let mut compositor = CompositorBuffer::new();

        assert!(compositor.resize(2));
        let mut line = compositor.prepare_line(0, 2, 0).unwrap();
        assert_eq!(line.columns, 0..2);
        compositor
            .try_compose_grid_segment(&mut line, 0..2, foreground, None)
            .unwrap();
        let submission = compositor.finish_line(line, 0, true, false).unwrap();

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
                flags: 0,
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

        assert!(compositor.resize(6));
        let submission = compositor
            .try_compose_line(
                CompositorLineRequest {
                    row: 0,
                    start_column: 0,
                    end_column: 6,
                    flags: 0,
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

        assert!(compositor.resize(3));
        let submission = compositor
            .try_compose_line(
                CompositorLineRequest {
                    row: 0,
                    start_column: 0,
                    end_column: 3,
                    flags: LINE_FLAG_WRAP,
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
        assert_eq!(submission.flags, 0);
        assert!(compositor.release());
        assert!(compositor.characters.is_empty());
        assert!(compositor.attributes.is_empty());
    }
}
