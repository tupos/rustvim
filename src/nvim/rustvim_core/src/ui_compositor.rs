use crate::grid::{
    GridRowSegment, Highlight, VimSAttrT, VimSCharT, schar_from_ascii, schar_from_char,
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

/// Mutable output slices for one compositor span.
///
/// Characters and attributes are kept in parallel slices because that is the
/// representation consumed by Neovim's raw-line UI event.
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
        if background.len() != len || foreground.len() != len {
            return None;
        }

        self.characters.copy_from_slice(foreground.characters());
        self.attributes.copy_from_slice(foreground.attributes());

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

        Some(SegmentComposition {
            followed_by_continuation: foreground.is_followed_by_continuation(),
        })
    }
}

/// Reports whether a screen character participates in negative-space blending.
fn is_negative_space(character: VimSCharT) -> bool {
    character == schar_from_ascii(' ') || character == schar_from_char(0x2800)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{GridRow, LineBuffer, ScreenGrid};

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
