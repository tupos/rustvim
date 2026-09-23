use std::{
    ops::Range,
    ptr::NonNull,
    slice,
    sync::{Mutex, MutexGuard},
};

use crate::{
    defines::Integer,
    grid::{
        CScreenGrid, GridRow, GridRowSegment, Highlight, ScreenGrid, ScreenGridGuard, VimSAttrT,
        VimSCharT, schar_from_ascii, schar_from_char,
    },
};

const LINE_FLAG_WRAP: libc::c_int = 1;
const LINE_FLAG_INVALID: libc::c_int = 2;

/// C callback used to recompose one half-open screen area.
type CompositorAreaCallback = unsafe extern "C" fn(
    start_row: Integer,
    end_row: Integer,
    start_column: Integer,
    end_column: Integer,
);

unsafe extern "C" {
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

/// C representation of one complete compositor-line request.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CCompositorLineRequest {
    row: Integer,
    start_col: Integer,
    end_col: Integer,
    flags: libc::c_int,
    message_separator_row: libc::c_int,
    message_separator_character: VimSCharT,
    message_separator_attribute: VimSAttrT,
    screen_columns: libc::c_int,
    abort_on_invalid: bool,
}

/// C representation of one raw-line planning request.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CCompositorRawLineRequest {
    row: Integer,
    start_col: Integer,
    end_col: Integer,
    clear_col: Integer,
    flags: libc::c_int,
    message_was_scrolled: bool,
}

/// C representation of a validated raw-line plan.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CCompositorRawLinePlan {
    row: Integer,
    start_col: Integer,
    end_col: Integer,
    clear_col: Integer,
    requested_clear_col: Integer,
    flags: libc::c_int,
    row_outside: bool,
    columns_clipped: bool,
    discard: bool,
    recompose: bool,
}

/// C representation of one grid-scroll planning request.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CCompositorScrollRequest {
    top: Integer,
    bottom: Integer,
    left: Integer,
    right: Integer,
    row_delta: Integer,
}

/// C representation of a validated compositor scroll plan.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CCompositorScrollPlan {
    top: Integer,
    bottom: Integer,
    left: Integer,
    right: Integer,
    first_recompose_row: Integer,
    recompose_end_row: Integer,
    first_grid_row: libc::c_int,
    grid_start_column: libc::c_int,
    recompose: bool,
}

/// One cell in composed-screen coordinates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompositorPoint {
    row: i32,
    column: i32,
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

/// Validated grid-local inputs for planning one compositor scroll.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorScrollRequest {
    region: CompositorRegion,
    row_delta: i32,
}

impl TryFrom<CCompositorScrollRequest> for CompositorScrollRequest {
    type Error = ();

    fn try_from(request: CCompositorScrollRequest) -> Result<Self, Self::Error> {
        let (Ok(top), Ok(bottom), Ok(left), Ok(right), Ok(row_delta)) = (
            i32::try_from(request.top),
            i32::try_from(request.bottom),
            i32::try_from(request.left),
            i32::try_from(request.right),
            i32::try_from(request.row_delta),
        ) else {
            return Err(());
        };

        Ok(Self {
            region: CompositorRegion::try_new(top..bottom, left..right).ok_or(())?,
            row_delta,
        })
    }
}

/// Action selected for one validated compositor scroll.
#[derive(Clone, Debug, Eq, PartialEq)]
enum CompositorScrollPlan {
    /// Forward the translated scroll directly to the composed UI.
    Forward { region: CompositorRegion },

    /// Recompose valid rows because another layer covers the region.
    Recompose {
        region: CompositorRegion,
        screen_rows: Range<i32>,
        first_grid_row: i32,
        grid_start_column: i32,
    },
}

impl From<CompositorScrollPlan> for CCompositorScrollPlan {
    fn from(plan: CompositorScrollPlan) -> Self {
        let (
            region,
            first_recompose_row,
            recompose_end_row,
            first_grid_row,
            grid_start_column,
            recompose,
        ) = match plan {
            CompositorScrollPlan::Forward { region } => (region, 0, 0, 0, 0, false),
            CompositorScrollPlan::Recompose {
                region,
                screen_rows,
                first_grid_row,
                grid_start_column,
            } => (
                region,
                screen_rows.start,
                screen_rows.end,
                first_grid_row,
                grid_start_column,
                true,
            ),
        };

        Self {
            top: Integer::from(region.rows.start),
            bottom: Integer::from(region.rows.end),
            left: Integer::from(region.columns.start),
            right: Integer::from(region.columns.end),
            first_recompose_row: Integer::from(first_recompose_row),
            recompose_end_row: Integer::from(recompose_end_row),
            first_grid_row,
            grid_start_column,
            recompose,
        }
    }
}

/// Validated grid-local inputs for planning one raw-line event.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorRawLineRequest {
    row: i32,
    columns: Range<i32>,
    clear_column: i32,
    flags: libc::c_int,
}

impl TryFrom<CCompositorRawLineRequest> for CompositorRawLineRequest {
    type Error = ();

    fn try_from(request: CCompositorRawLineRequest) -> Result<Self, Self::Error> {
        let (Ok(row), Ok(start_column), Ok(end_column), Ok(clear_column)) = (
            i32::try_from(request.row),
            i32::try_from(request.start_col),
            i32::try_from(request.end_col),
            i32::try_from(request.clear_col),
        ) else {
            return Err(());
        };

        (start_column <= end_column && end_column <= clear_column)
            .then_some(Self {
                row,
                columns: start_column..end_column,
                clear_column,
                flags: request.flags,
            })
            .ok_or(())
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

/// Translated and clipped raw-line state returned to the C callback boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CompositorRawLinePlan {
    row: i32,
    columns: Range<i32>,
    clear_column: i32,
    requested_clear_column: i32,
    flags: libc::c_int,
    action: CompositorRawLineAction,
}

impl From<CompositorRawLinePlan> for CCompositorRawLinePlan {
    fn from(plan: CompositorRawLinePlan) -> Self {
        let row_outside = plan.action == CompositorRawLineAction::DiscardRow;
        let discard = matches!(
            plan.action,
            CompositorRawLineAction::DiscardRow | CompositorRawLineAction::DiscardColumns
        );
        let recompose = plan.action == CompositorRawLineAction::Recompose;

        Self {
            row: Integer::from(plan.row),
            start_col: Integer::from(plan.columns.start),
            end_col: Integer::from(plan.columns.end),
            clear_col: Integer::from(plan.clear_column),
            requested_clear_col: Integer::from(plan.requested_clear_column),
            flags: plan.flags,
            row_outside,
            columns_clipped: plan.clear_column != plan.requested_clear_column,
            discard,
            recompose,
        }
    }
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
    accepts_mouse_input: bool,
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
            accepts_mouse_input: grid.accepts_mouse_input(),
            is_default,
        })
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

    /// Reports whether the allocated grid contains one composed-screen cell.
    ///
    /// Hit testing follows the original compositor behavior and therefore
    /// uses the allocated grid size, independently of its requested visible
    /// size and temporary compositor-disabled state.
    fn contains_allocated_cell(self, point: CompositorPoint) -> bool {
        point.row >= self.origin_row
            && self
                .origin_row
                .checked_add(self.grid.rows)
                .is_some_and(|bottom| point.row < bottom)
            && point.column >= self.origin_column
            && self
                .origin_column
                .checked_add(self.grid.cols)
                .is_some_and(|right| point.column < right)
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
    /// Reports whether one layer participates in this hit test.
    fn accepts(self, layer: CompositorLayer<'_>) -> bool {
        match self {
            Self::Any => true,
            Self::MouseEnabled => layer.accepts_mouse_input,
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

    /// Finds the topmost non-default layer containing one screen cell.
    fn topmost_layer_at(&self, point: CompositorPoint, hit_test: LayerHitTest) -> Option<usize> {
        self.layers.get(1..)?.iter().rev().find_map(|layer| {
            let layer = *layer;
            (layer.contains_allocated_cell(point) && hit_test.accepts(layer))
                .then_some(layer.layer_index)
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
            return Some(CompositorScrollPlan::Forward { region });
        }

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

/// One C grid identity paired with its temporary Rust owner.
struct GuardedCompositorLayer {
    cgrid: NonNull<CScreenGrid>,
    grid: ScreenGridGuard,
}

/// Temporary Rust ownership of the ordered compositor layers received from C.
///
/// Each contained `ScreenGridGuard` restores its grid allocations when this
/// collection is dropped.
#[must_use = "dropping the layer guards restores ownership to the C grids"]
struct CompositorLayerGuards {
    layers: Vec<GuardedCompositorLayer>,
}

impl CompositorLayerGuards {
    /// Validates a C layer array and takes temporary ownership of every grid.
    ///
    /// # Safety
    ///
    /// If `layer_count` is nonzero, `layer_grids` must point to that many
    /// readable grid pointers. Every non-null grid must be valid, distinct,
    /// exclusively accessible, and satisfy the ownership and layout
    /// requirements of `ScreenGrid`.
    unsafe fn try_take(layer_grids: *const *mut CScreenGrid, layer_count: usize) -> Option<Self> {
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

        let layers = cgrids
            .into_iter()
            .map(|cgrid| {
                // SAFETY: Null and duplicate pointers were rejected, and the
                // caller transfers ownership for this operation.
                let grid = unsafe { ScreenGridGuard::take(cgrid.as_ptr()) };
                GuardedCompositorLayer { cgrid, grid }
            })
            .collect();
        Some(Self { layers })
    }

    /// Returns the stack index belonging to one C grid identity.
    fn index_of(&self, cgrid: *mut CScreenGrid) -> Option<usize> {
        let cgrid = NonNull::new(cgrid)?;
        self.layers.iter().position(|layer| layer.cgrid == cgrid)
    }

    /// Borrows one owned Rust grid by its layer index.
    fn grid(&self, index: usize) -> Option<&ScreenGrid> {
        self.layers.get(index).map(|layer| &*layer.grid)
    }

    /// Borrows all owned grids as a validated compositor layer stack.
    fn layer_stack(&self) -> Option<CompositorLayerStack<'_>> {
        let grids = self
            .layers
            .iter()
            .map(|layer| &*layer.grid)
            .collect::<Vec<_>>();
        CompositorLayerStack::try_from_grids(&grids)
    }

    /// Applies a planned relocation and synchronizes affected grid metadata.
    fn apply_layer_move(&mut self, movement: CompositorLayerMove) -> Option<()> {
        movement.apply_to(&mut self.layers)?;

        let affected_indices = movement.affected_indices();
        let first_index = *affected_indices.start();
        for (offset, layer) in self
            .layers
            .get_mut(affected_indices)?
            .iter_mut()
            .enumerate()
        {
            layer.grid.set_compositor_index(first_index + offset);
        }

        Some(())
    }

    /// Removes one non-default layer from the active prefix.
    ///
    /// The removed layer is moved to the inactive tail so the C boundary can
    /// shrink its vector after every grid allocation has been restored.
    fn remove_layer(&mut self, layer_index: usize) -> Option<()> {
        if layer_index == 0 {
            return None;
        }

        let last_index = self.layers.len().checked_sub(1)?;
        self.apply_layer_move(CompositorLayerMove {
            source_index: layer_index,
            target_index: last_index,
        })?;
        self.layers.last_mut()?.grid.set_compositor_index(0);
        Some(())
    }

    /// Writes the current grid identity order into a C pointer slice.
    fn write_cgrid_order(&self, cgrids: &mut [*mut CScreenGrid]) -> bool {
        if cgrids.len() != self.layers.len() {
            return false;
        }

        for (cgrid, layer) in cgrids.iter_mut().zip(&self.layers) {
            *cgrid = layer.cgrid.as_ptr();
        }
        true
    }
}

/// Message-separator replacement applied below its compositor layer.
#[derive(Clone, Copy)]
struct MessageSeparator<'grid> {
    layer: CompositorLayer<'grid>,
    character: VimSCharT,
    attribute: VimSAttrT,
}

/// Validated inputs for composing one complete line in Rust.
#[derive(Clone, Copy)]
struct CompositorLineRequest<'grid> {
    row: i32,
    start_column: i32,
    end_column: i32,
    flags: libc::c_int,
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

static COMPOSITOR_BUFFER: Mutex<CompositorBuffer> = Mutex::new(CompositorBuffer::new());

/// Acquires exclusive access to the process-global compositor buffer.
fn lock_compositor_buffer() -> MutexGuard<'static, CompositorBuffer> {
    COMPOSITOR_BUFFER
        .try_lock()
        .expect("compositor buffer is already borrowed")
}

#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_resize_buffer(width: Integer) -> bool {
    let Ok(width) = usize::try_from(width) else {
        return false;
    };
    lock_compositor_buffer().resize(width)
}

#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_release_buffer() -> bool {
    lock_compositor_buffer().release()
}

/// Adjusts one layer's position after its z-index changes.
///
/// Every temporarily owned grid is restored before this function returns.
///
/// # Safety
///
/// `layer_grids` must point to `layer_count` writable, distinct, non-null grid
/// pointers. Every grid must be exclusively accessible and its buffers must
/// satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_adjust_layer_order(
    layer_grids: *mut *mut CScreenGrid,
    layer_count: usize,
    layer_index: usize,
    raise: bool,
) -> bool {
    // SAFETY: The caller satisfies the documented layer-array and grid
    // ownership requirements.
    let Some(mut layer_guards) =
        (unsafe { CompositorLayerGuards::try_take(layer_grids, layer_count) })
    else {
        return false;
    };
    let adjustment = if raise {
        CompositorLayerAdjustment::Raise
    } else {
        CompositorLayerAdjustment::Lower
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
    if layer_guards.apply_layer_move(movement).is_none() {
        return false;
    }

    // SAFETY: The caller guarantees that the pointer array is writable for
    // `layer_count` elements for the duration of this synchronous call.
    let cgrids = unsafe { slice::from_raw_parts_mut(layer_grids, layer_count) };
    layer_guards.write_cgrid_order(cgrids)
}

/// Removes one non-default grid from the active compositor-layer prefix.
///
/// The C grid identity is converted into a Rust layer index at this boundary.
/// Safe removal logic works only with that index and the owned Rust grids.
///
/// The removed grid is moved to the final array position so C can shrink its
/// layer vector after this function restores ownership. If the grid is not an
/// active layer, the array is unchanged and `was_removed` receives `false`.
///
/// # Safety
///
/// `layer_grids` must point to `layer_count` writable, distinct, non-null grid
/// pointers. Every grid must be exclusively accessible and its buffers must
/// satisfy the ownership and layout requirements of `ScreenGrid`.
///
/// `grid` must be non-null. If it identifies an entry in `layer_grids`, that
/// entry must not be the default layer. `was_removed` must point to writable,
/// properly aligned storage for one `bool` and must not overlap the layer
/// array or any grid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_remove_layer(
    layer_grids: *mut *mut CScreenGrid,
    layer_count: usize,
    grid: *mut CScreenGrid,
    was_removed: *mut bool,
) -> bool {
    if grid.is_null() || was_removed.is_null() {
        return false;
    }

    // SAFETY: The caller satisfies the documented layer-array and grid
    // ownership requirements.
    let Some(mut layer_guards) =
        (unsafe { CompositorLayerGuards::try_take(layer_grids, layer_count) })
    else {
        return false;
    };
    let removed = match layer_guards.index_of(grid) {
        Some(layer_index) => {
            if layer_guards.remove_layer(layer_index).is_none() {
                return false;
            }
            true
        }
        None => false,
    };

    // SAFETY: The caller guarantees that the pointer array is writable for
    // `layer_count` elements for the duration of this synchronous call.
    let cgrids = unsafe { slice::from_raw_parts_mut(layer_grids, layer_count) };
    if !layer_guards.write_cgrid_order(cgrids) {
        return false;
    }

    // SAFETY: The caller provides valid, non-overlapping output storage.
    unsafe { was_removed.write(removed) };
    true
}

/// Raises one compositor layer and recomposes every crossed layer intersection.
///
/// Grid ownership is restored before `compose_area` is invoked.
///
/// # Safety
///
/// `layer_grids` must point to `layer_count` writable, distinct, non-null grid
/// pointers. Every grid must be exclusively accessible and its buffers must
/// satisfy the ownership and layout requirements of `ScreenGrid`.
///
/// `grid` must identify one grid in `layer_grids`, and `compose_area` must be
/// a valid callback for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_raise_layer(
    layer_grids: *mut *mut CScreenGrid,
    layer_count: usize,
    grid: *mut CScreenGrid,
    target_index: usize,
    compose_area: Option<CompositorAreaCallback>,
) -> bool {
    if grid.is_null() {
        return false;
    }
    let Some(compose_area) = compose_area else {
        return false;
    };

    let plan = (|| {
        // SAFETY: The caller satisfies the documented layer-array and grid
        // ownership requirements.
        let mut layer_guards =
            unsafe { CompositorLayerGuards::try_take(layer_grids, layer_count) }?;
        let layer_index = layer_guards.index_of(grid)?;
        let plan = {
            let layers = layer_guards.layer_stack()?;
            layers.plan_layer_raise(layer_index, target_index)?
        };
        layer_guards.apply_layer_move(plan.movement)?;

        // SAFETY: The caller guarantees that the pointer array is writable for
        // `layer_count` elements for the duration of this synchronous call.
        let cgrids = unsafe { slice::from_raw_parts_mut(layer_grids, layer_count) };
        layer_guards.write_cgrid_order(cgrids).then_some(plan)
    })();
    let Some(plan) = plan else {
        return false;
    };

    for area in plan.recompose_areas {
        // SAFETY: The caller supplied a valid callback. The layer guards were
        // dropped above, so all grid allocations are owned by C again.
        unsafe {
            compose_area(
                Integer::from(area.rows.start),
                Integer::from(area.rows.end),
                Integer::from(area.columns.start),
                Integer::from(area.columns.end),
            );
        }
    }

    true
}

/// Finds the topmost non-default compositor layer at one screen coordinate.
///
/// Returns `usize::MAX` when no matching layer exists or the layer array is
/// invalid. Every temporarily owned grid is restored before this function
/// returns.
///
/// # Safety
///
/// `layer_grids` must point to `layer_count` readable, distinct, non-null grid
/// pointers. Every grid must be exclusively accessible and its buffers must
/// satisfy the ownership and layout requirements of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_find_layer_at_coord(
    layer_grids: *const *mut CScreenGrid,
    layer_count: usize,
    row: libc::c_int,
    column: libc::c_int,
    require_mouse_input: bool,
) -> usize {
    let hit_test = if require_mouse_input {
        LayerHitTest::MouseEnabled
    } else {
        LayerHitTest::Any
    };

    // SAFETY: The caller satisfies the documented layer-array and grid
    // ownership requirements.
    let Some(layer_guards) = (unsafe { CompositorLayerGuards::try_take(layer_grids, layer_count) })
    else {
        return usize::MAX;
    };
    let Some(layers) = layer_guards.layer_stack() else {
        return usize::MAX;
    };

    layers
        .topmost_layer_at(CompositorPoint { row, column }, hit_test)
        .unwrap_or(usize::MAX)
}

/// Composes and submits one complete line from an ordered C grid array.
///
/// # Safety
///
/// `layer_grids` must point to `layer_count` readable, distinct, non-null grid
/// pointers. Every grid must be exclusively accessible and its buffers must
/// satisfy the ownership and layout requirements of `ScreenGrid`.
///
/// When the requested row is the message-separator row, `message_grid` must
/// either identify one grid in `layer_grids` or identify another exclusively
/// accessible grid satisfying the same requirements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_compose_line(
    layer_grids: *const *mut CScreenGrid,
    layer_count: usize,
    message_grid: *mut CScreenGrid,
    request: CCompositorLineRequest,
) -> bool {
    let (Ok(row), Ok(start_column), Ok(end_column), Ok(screen_columns)) = (
        i32::try_from(request.row),
        i32::try_from(request.start_col),
        i32::try_from(request.end_col),
        usize::try_from(request.screen_columns),
    ) else {
        return false;
    };

    let separator_active = row == request.message_separator_row;
    if separator_active && message_grid.is_null() {
        return false;
    }
    let mut compositor = lock_compositor_buffer();
    let submission = (|| {
        // SAFETY: The caller satisfies the documented layer-array and grid
        // ownership requirements.
        let layer_guards = unsafe { CompositorLayerGuards::try_take(layer_grids, layer_count) }?;
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
                character: request.message_separator_character,
                attribute: request.message_separator_attribute,
            })
        } else {
            None
        };

        compositor.try_compose_line(
            CompositorLineRequest {
                row,
                start_column,
                end_column,
                flags: request.flags,
                message_separator,
                screen_columns,
                abort_on_invalid: request.abort_on_invalid,
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
            row as Integer,
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

#[unsafe(no_mangle)]
pub extern "C" fn ui_comp_emit_clear_line(
    row: Integer,
    start_col: Integer,
    end_col: Integer,
    attribute: VimSAttrT,
) -> bool {
    let (Ok(start_col), Ok(end_col)) = (usize::try_from(start_col), usize::try_from(end_col))
    else {
        return false;
    };

    let compositor = lock_compositor_buffer();
    if start_col > end_col || end_col > compositor.characters.len() {
        return false;
    }

    let characters = unsafe { compositor.characters.as_ptr().add(start_col) };
    let attributes = unsafe { compositor.attributes.as_ptr().add(start_col) };

    // SAFETY: The raw-line event contains no character payload because its
    // start and end columns are equal. The pointers nevertheless remain valid
    // for the synchronous UI call.
    unsafe {
        ui_composed_call_raw_line(
            1,
            row,
            start_col as Integer,
            start_col as Integer,
            end_col as Integer,
            attribute as Integer,
            0,
            characters,
            attributes,
        );
    }
    true
}

/// Reports whether a scrolled grid row should be recomposed.
///
/// # Safety
///
/// `grid` must be null or point to a valid, exclusively accessible
/// `CScreenGrid` whose buffers satisfy the ownership and layout requirements
/// of `ScreenGrid`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_should_compose_scroll_row(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    start_col: libc::c_int,
) -> bool {
    if grid.is_null() {
        return false;
    }

    let (Ok(row), Ok(start_col)) = (usize::try_from(row), usize::try_from(start_col)) else {
        return false;
    };

    // SAFETY: The caller transfers exclusive ownership of the grid buffers
    // into the guard for the duration of this function.
    let rust_grid = unsafe { ScreenGridGuard::take(grid) };
    GridRow::try_from_grid(&rust_grid, row).is_some_and(|row| row.is_valid_at(start_col))
}

/// Plans translation, clipping, and composition for one raw-line event.
///
/// # Safety
///
/// `layer_grids` must point to `layer_count` readable, distinct, non-null grid
/// pointers. Every grid must be exclusively accessible and its buffers must
/// satisfy the ownership and layout requirements of `ScreenGrid`.
///
/// `current_grid` must identify one grid in `layer_grids`. `message_grid` is
/// used only for pointer identity and may be null. `output` must point to
/// writable, properly aligned storage for one `CCompositorRawLinePlan`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_plan_raw_line(
    layer_grids: *const *mut CScreenGrid,
    layer_count: usize,
    current_grid: *mut CScreenGrid,
    message_grid: *mut CScreenGrid,
    request: CCompositorRawLineRequest,
    output: *mut CCompositorRawLinePlan,
) -> bool {
    if current_grid.is_null() || output.is_null() {
        return false;
    }

    let Ok(raw_line) = CompositorRawLineRequest::try_from(request) else {
        return false;
    };

    // SAFETY: The caller satisfies the documented layer-array and grid
    // ownership requirements.
    let Some(layer_guards) = (unsafe { CompositorLayerGuards::try_take(layer_grids, layer_count) })
    else {
        return false;
    };
    let Some(current_layer_index) = layer_guards.index_of(current_grid) else {
        return false;
    };
    let message_layer_index = layer_guards.index_of(message_grid);
    let Some(layers) = layer_guards.layer_stack() else {
        return false;
    };

    let Some(plan) = layers.plan_raw_line(
        current_layer_index,
        message_layer_index,
        request.message_was_scrolled,
        raw_line,
    ) else {
        return false;
    };

    // SAFETY: The caller guarantees writable, aligned storage for one plan.
    unsafe { output.write(plan.into()) };
    true
}

/// Plans coordinate translation and recomposition for one grid scroll.
///
/// The returned plan contains only owned scalar data. All grid guards restore
/// their allocations before C executes the selected compositor callbacks.
///
/// # Safety
///
/// `layer_grids` must point to `layer_count` readable, distinct, non-null grid
/// pointers. Every grid must be exclusively accessible and its buffers must
/// satisfy the ownership and layout requirements of `ScreenGrid`.
///
/// `current_grid` must identify one grid in `layer_grids`. `message_grid` is
/// used only for pointer identity and may be null. `output` must point to
/// writable, properly aligned storage for one `CCompositorScrollPlan`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_plan_scroll(
    layer_grids: *const *mut CScreenGrid,
    layer_count: usize,
    current_grid: *mut CScreenGrid,
    message_grid: *mut CScreenGrid,
    message_was_scrolled: bool,
    request: CCompositorScrollRequest,
    output: *mut CCompositorScrollPlan,
) -> bool {
    if current_grid.is_null() || output.is_null() {
        return false;
    }

    let Ok(request) = CompositorScrollRequest::try_from(request) else {
        return false;
    };

    // SAFETY: The caller satisfies the documented layer-array and grid
    // ownership requirements.
    let Some(layer_guards) = (unsafe { CompositorLayerGuards::try_take(layer_grids, layer_count) })
    else {
        return false;
    };
    let Some(current_layer_index) = layer_guards.index_of(current_grid) else {
        return false;
    };
    let message_layer_index = layer_guards.index_of(message_grid);
    let Some(layers) = layer_guards.layer_stack() else {
        return false;
    };
    let Some(plan) = layers.plan_scroll(
        current_layer_index,
        message_layer_index,
        message_was_scrolled,
        request,
    ) else {
        return false;
    };

    // SAFETY: The caller guarantees writable, aligned storage for one plan.
    unsafe { output.write(plan.into()) };
    true
}

/// Reports whether a screen character participates in negative-space blending.
fn is_negative_space(character: VimSCharT) -> bool {
    character == schar_from_ascii(' ') || character == schar_from_char(0x2800)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::LineBuffer;

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
        grid
    }

    fn grid_with_size(rows: i32, columns: i32) -> ScreenGrid {
        let mut line_buffer = LineBuffer::new(columns as usize);
        ScreenGrid::new(rows, columns, true, &mut line_buffer)
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
            accepts_mouse_input: true,
            is_default: layer_index == 0,
        }
    }

    #[test]
    fn layer_guards_validate_identity_and_restore_all_grids() {
        let first = grid_with_row(&[schar_from_ascii('a')], &[1]);
        let second = grid_with_row(&[schar_from_ascii('b')], &[2]);
        // SAFETY: Each conversion transfers its grid allocations to a distinct
        // C representation.
        let (mut first_cgrid, mut second_cgrid) = unsafe { (first.to_cgrid(), second.to_cgrid()) };
        let first_pointer = &mut first_cgrid as *mut CScreenGrid;
        let second_pointer = &mut second_cgrid as *mut CScreenGrid;
        let cgrids = [first_pointer, second_pointer];

        {
            // SAFETY: Both pointers identify distinct, exclusively accessible
            // C grids and the pointer array remains readable for this scope.
            let guards =
                unsafe { CompositorLayerGuards::try_take(cgrids.as_ptr(), cgrids.len()) }.unwrap();
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
    fn guarded_layer_move_updates_pointer_order_and_grid_indices() {
        let default_grid = grid_with_row(&[schar_from_ascii('d')], &[1]);
        let lower_grid = grid_with_row(&[schar_from_ascii('l')], &[2]);
        let upper_grid = grid_with_row(&[schar_from_ascii('u')], &[3]);
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
        let mut cgrids = [default_pointer, lower_pointer, upper_pointer];

        {
            // SAFETY: The pointers identify distinct, exclusively accessible
            // C grids and the pointer array remains readable for this scope.
            let mut guards =
                unsafe { CompositorLayerGuards::try_take(cgrids.as_ptr(), cgrids.len()) }.unwrap();
            guards
                .apply_layer_move(CompositorLayerMove {
                    source_index: 2,
                    target_index: 1,
                })
                .unwrap();
            assert!(guards.write_cgrid_order(&mut cgrids));
        }

        assert_eq!(cgrids, [default_pointer, upper_pointer, lower_pointer]);

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
    fn guarded_layer_removal_updates_order_and_indices() {
        let default_grid = grid_with_row(&[schar_from_ascii('d')], &[1]);
        let removed_grid = grid_with_row(&[schar_from_ascii('r')], &[2]);
        let remaining_grid = grid_with_row(&[schar_from_ascii('k')], &[3]);
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
        let mut cgrids = [default_pointer, removed_pointer, remaining_pointer];

        {
            // SAFETY: The pointers identify distinct, exclusively accessible
            // C grids and the pointer array remains readable for this scope.
            let mut guards =
                unsafe { CompositorLayerGuards::try_take(cgrids.as_ptr(), cgrids.len()) }.unwrap();
            assert_eq!(guards.remove_layer(0), None);
            assert_eq!(guards.remove_layer(1), Some(()));
            assert!(guards.write_cgrid_order(&mut cgrids));
        }

        assert_eq!(
            cgrids,
            [default_pointer, remaining_pointer, removed_pointer]
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
            layers.topmost_layer_at(CompositorPoint { row: 2, column: 2 }, LayerHitTest::Any,),
            Some(2)
        );
    }

    #[test]
    fn hit_testing_excludes_the_default_layer() {
        let default_grid = grid_with_size(3, 3);
        let layers = CompositorLayerStack {
            layers: vec![compositor_layer(&default_grid, 0, 3, 0, false)],
        };

        assert_eq!(
            layers.topmost_layer_at(CompositorPoint { row: 1, column: 1 }, LayerHitTest::Any,),
            None
        );
    }

    #[test]
    fn mouse_hit_testing_skips_layers_that_reject_mouse_input() {
        let default_grid = grid_with_size(3, 3);
        let lower_grid = grid_with_size(2, 2);
        let upper_grid = grid_with_size(2, 2);
        let lower_layer = compositor_layer(&lower_grid, 1, 2, 1, false);
        let mut upper_layer = compositor_layer(&upper_grid, 1, 2, 2, false);
        upper_layer.accepts_mouse_input = false;
        let layers = CompositorLayerStack {
            layers: vec![
                compositor_layer(&default_grid, 0, 3, 0, false),
                lower_layer,
                upper_layer,
            ],
        };
        let point = CompositorPoint { row: 0, column: 1 };

        assert_eq!(layers.topmost_layer_at(point, LayerHitTest::Any), Some(2));
        assert_eq!(
            layers.topmost_layer_at(point, LayerHitTest::MouseEnabled),
            Some(1)
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

        assert_eq!(
            layers.topmost_layer_at(CompositorPoint { row: 0, column: 0 }, LayerHitTest::Any,),
            None
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
    fn ffi_checks_scroll_cell_validity_and_restores_ownership() {
        let grid = grid_with_row(&[schar_from_ascii('x'), schar_from_ascii('y')], &[1, -1]);
        // SAFETY: This transfers every grid allocation to its C representation.
        let mut cgrid = unsafe { grid.to_cgrid() };

        // SAFETY: `cgrid` exclusively owns valid grid allocations. Each call
        // restores those allocations before returning.
        let (valid, invalid, outside) = unsafe {
            (
                ui_comp_should_compose_scroll_row(&mut cgrid, 0, 0),
                ui_comp_should_compose_scroll_row(&mut cgrid, 0, 1),
                ui_comp_should_compose_scroll_row(&mut cgrid, 1, 0),
            )
        };

        assert!(valid);
        assert!(!invalid);
        assert!(!outside);

        // SAFETY: The FFI function returned ownership to `cgrid`.
        let restored_grid = unsafe { ScreenGrid::take_from_cgrid(&mut cgrid) };
        assert_eq!(
            restored_grid.chars.as_slice(),
            &[schar_from_ascii('x'), schar_from_ascii('y')]
        );
        assert_eq!(restored_grid.attrs.as_slice(), &[1, -1]);
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
                },
            )
            .unwrap();

        assert_eq!(
            plan,
            CompositorScrollPlan::Forward {
                region: CompositorRegion::try_new(5..10, 5..9).unwrap(),
            }
        );
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
