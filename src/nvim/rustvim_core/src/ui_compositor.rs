use std::{
    ops::Range,
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
    visible_width: i32,
    visible_height: i32,
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
            visible_width,
            visible_height,
            layer_index: grid.compositor_index(),
            blending: grid.blends_with_background(),
            disabled: grid.is_compositor_disabled(),
            is_default,
        })
    }

    /// Returns the exclusive right edge of this layer.
    fn right_edge(self) -> Option<i32> {
        self.origin_column.checked_add(self.visible_width)
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

/// Ordered borrowed grids used to compose one line.
struct CompositorLayerStack<'grid> {
    layers: Vec<CompositorLayer<'grid>>,
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
    if layer_grids.is_null() || layer_count == 0 {
        return false;
    }

    let (Ok(row), Ok(start_column), Ok(end_column), Ok(screen_columns)) = (
        i32::try_from(request.row),
        i32::try_from(request.start_col),
        i32::try_from(request.end_col),
        usize::try_from(request.screen_columns),
    ) else {
        return false;
    };

    // SAFETY: The caller guarantees that the pointer array is readable for
    // `layer_count` elements for the duration of this synchronous call.
    let layer_grids = unsafe { slice::from_raw_parts(layer_grids, layer_count) };
    if layer_grids
        .iter()
        .enumerate()
        .any(|(index, grid)| grid.is_null() || layer_grids[..index].contains(grid))
    {
        return false;
    }

    let separator_active = row == request.message_separator_row;
    if separator_active && message_grid.is_null() {
        return false;
    }
    let message_grid_index = separator_active
        .then(|| layer_grids.iter().position(|grid| *grid == message_grid))
        .flatten();
    let separate_message_grid = separator_active && message_grid_index.is_none();

    let mut compositor = lock_compositor_buffer();
    let submission = (|| {
        let grid_guards = layer_grids
            .iter()
            .map(|grid| {
                // SAFETY: Null and duplicate pointers were rejected, and the
                // caller transfers ownership for this operation.
                unsafe { ScreenGridGuard::take(*grid) }
            })
            .collect::<Vec<_>>();

        let message_grid_guard = separate_message_grid.then(|| {
            // SAFETY: The non-null message grid is distinct from every layer
            // pointer and the caller transfers ownership for this operation.
            unsafe { ScreenGridGuard::take(message_grid) }
        });

        let grids = grid_guards.iter().map(|grid| &**grid).collect::<Vec<_>>();
        let layers = CompositorLayerStack::try_from_grids(&grids)?;

        let message_separator = if separator_active {
            let message_grid = match message_grid_index {
                Some(index) => *grids.get(index)?,
                None => message_grid_guard.as_deref()?,
            };
            Some(MessageSeparator {
                layer: CompositorLayer::try_from_grid(
                    message_grid,
                    std::ptr::eq(message_grid, *grids.first()?),
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
            visible_width,
            visible_height: grid.rows,
            layer_index,
            blending: false,
            disabled,
            is_default: layer_index == 0,
        }
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
