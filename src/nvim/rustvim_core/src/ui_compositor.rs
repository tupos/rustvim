use std::{ops::Range, slice};

use crate::grid::{
    CScreenGrid, GridRow, GridRowSegment, Highlight, ScreenGrid, ScreenGridGuard, VimSAttrT,
    VimSCharT, schar_from_ascii, schar_from_char,
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

/// C-compatible result of copying one grid segment into compositor output.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CCompositorGridSegmentResult {
    copied: bool,
    followed_by_continuation: bool,
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

    /// Copies a logical row range from a Rust screen grid.
    pub(crate) fn try_copy_from_grid(
        &mut self,
        grid: &ScreenGrid,
        row: usize,
        columns: Range<usize>,
    ) -> Option<SegmentComposition> {
        let foreground = GridRow::try_from_grid(grid, row)?.segment(columns)?;
        self.try_copy(foreground)
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
        self.try_compose_with(
            background,
            foreground,
            blending,
            |background, foreground, through| {
                let (attribute, through) =
                    Highlight::new(background).blend(Highlight::new(foreground), through);
                (attribute.raw(), through)
            },
        )
    }

    /// Composes a segment using the supplied attribute-blending operation.
    ///
    /// Keeping the cell traversal independent of the highlight implementation
    /// permits deterministic tests of wide-cell and negative-space behavior.
    fn try_compose_with(
        &mut self,
        background: GridRowSegment<'_>,
        foreground: GridRowSegment<'_>,
        blending: bool,
        mut blend_attributes: impl FnMut(VimSAttrT, VimSAttrT, bool) -> (VimSAttrT, bool),
    ) -> Option<SegmentComposition> {
        let len = self.characters.len();
        if background.len() != len {
            return None;
        }

        let composition = self.try_copy(foreground)?;

        if blending {
            let background_characters = background.characters();
            let background_attributes = background.attributes();
            let mut index = 0;

            while index < len {
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
        }

        Some(composition)
    }
}

/// Copies one C-owned grid segment into compositor output.
///
/// # Safety
///
/// `grid` must be null or point to a valid, exclusively accessible
/// `CScreenGrid` whose buffers satisfy the ownership and layout requirements
/// of `ScreenGrid`. For a nonempty requested range, `characters` and
/// `attributes` must point to separate writable arrays containing at least
/// `end_col - start_col` elements. Those arrays must not overlap the grid
/// allocations.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ui_comp_copy_grid_segment(
    grid: *mut CScreenGrid,
    row: libc::c_int,
    start_col: libc::c_int,
    end_col: libc::c_int,
    characters: *mut VimSCharT,
    attributes: *mut VimSAttrT,
) -> CCompositorGridSegmentResult {
    let not_copied = CCompositorGridSegmentResult::default();
    if grid.is_null() || characters.is_null() || attributes.is_null() {
        return not_copied;
    }

    let (Ok(row), Ok(start_col), Ok(end_col)) = (
        usize::try_from(row),
        usize::try_from(start_col),
        usize::try_from(end_col),
    ) else {
        return not_copied;
    };
    let Some(len) = end_col.checked_sub(start_col) else {
        return not_copied;
    };
    if len == 0 {
        return not_copied;
    }

    // SAFETY: The caller transfers exclusive ownership of the grid buffers
    // into the guard for the duration of this function.
    let rust_grid = unsafe { ScreenGridGuard::take(grid) };
    let composition = {
        // SAFETY: The caller guarantees that both output pointers describe
        // separate writable arrays of `len` elements which do not overlap the
        // grid allocations.
        let characters = unsafe { slice::from_raw_parts_mut(characters, len) };
        let attributes = unsafe { slice::from_raw_parts_mut(attributes, len) };
        CompositorSegment::try_from_slices(characters, attributes)
            .and_then(|mut output| output.try_copy_from_grid(&rust_grid, row, start_col..end_col))
    };

    let Some(composition) = composition else {
        return not_copied;
    };
    CCompositorGridSegmentResult {
        copied: true,
        followed_by_continuation: composition.is_followed_by_continuation(),
    }
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
    fn ffi_copies_grid_segment_and_restores_ownership() {
        let grid = grid_with_row(
            &[schar_from_ascii('x'), schar_from_ascii('y'), 0],
            &[1, 2, 3],
        );
        // SAFETY: This transfers every grid allocation to its C representation.
        let mut cgrid = unsafe { grid.to_cgrid() };
        let mut characters = [0; 2];
        let mut attributes = [0; 2];

        // SAFETY: `cgrid` exclusively owns valid grid allocations, and the
        // output arrays are separate, writable, and have the requested length.
        let result = unsafe {
            ui_comp_copy_grid_segment(
                &mut cgrid,
                0,
                0,
                2,
                characters.as_mut_ptr(),
                attributes.as_mut_ptr(),
            )
        };

        assert!(result.copied);
        assert!(result.followed_by_continuation);
        assert_eq!(characters, [schar_from_ascii('x'), schar_from_ascii('y')]);
        assert_eq!(attributes, [1, 2]);

        // SAFETY: The FFI function returned ownership to `cgrid`.
        let restored_grid = unsafe { ScreenGrid::take_from_cgrid(&mut cgrid) };
        assert_eq!(
            restored_grid.chars.as_slice(),
            &[schar_from_ascii('x'), schar_from_ascii('y'), 0]
        );
    }

    #[test]
    fn ffi_restores_grid_ownership_after_rejecting_a_segment() {
        let grid = grid_with_row(&[schar_from_ascii('x'), schar_from_ascii('y')], &[1, 2]);
        // SAFETY: This transfers every grid allocation to its C representation.
        let mut cgrid = unsafe { grid.to_cgrid() };
        let mut characters = [0; 2];
        let mut attributes = [0; 2];

        // SAFETY: The output arrays are valid, but columns 1..3 exceed the
        // source row and are rejected by the safe Rust implementation.
        let rejected = unsafe {
            ui_comp_copy_grid_segment(
                &mut cgrid,
                0,
                1,
                3,
                characters.as_mut_ptr(),
                attributes.as_mut_ptr(),
            )
        };
        assert!(!rejected.copied);

        // SAFETY: The rejected operation returned ownership to `cgrid`.
        let restored_grid = unsafe { ScreenGrid::take_from_cgrid(&mut cgrid) };
        assert_eq!(
            restored_grid.chars.as_slice(),
            &[schar_from_ascii('x'), schar_from_ascii('y')]
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
            .try_compose_with(
                GridRow::try_from_grid(&background, 0)
                    .unwrap()
                    .segment(0..4)
                    .unwrap(),
                GridRow::try_from_grid(&foreground, 0)
                    .unwrap()
                    .segment(0..4)
                    .unwrap(),
                true,
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
}
