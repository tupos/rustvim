use super::LineBuffer;
use super::VimSAttrT;

/// Appearance of the marker placed on the first smooth-scrolled line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SmoothScrollMarker {
    preserve_line_number: bool,
    attribute: VimSAttrT,
}

impl SmoothScrollMarker {
    /// Creates a smooth-scroll marker configuration.
    pub(super) fn new(preserve_line_number: bool, attribute: VimSAttrT) -> Self {
        Self {
            preserve_line_number,
            attribute,
        }
    }
}

/// Validated line-buffer range ready for submission to a screen grid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LineSubmission {
    start_col: usize,
    end_col: usize,
    clear_width: usize,
    right_left: bool,
}

impl LineSubmission {
    /// Prepares a line-buffer range for final grid submission.
    ///
    /// Right-to-left rendering mirrors the completed line before translating
    /// its write and clear ranges. A smooth-scroll marker is applied after
    /// mirroring, matching the existing rendering order.
    pub(super) fn prepare(
        line_buffer: &mut LineBuffer,
        end_col: usize,
        width: usize,
        clear_end: bool,
        right_left: bool,
        smooth_scroll_marker: Option<SmoothScrollMarker>,
    ) -> Option<Self> {
        if end_col > width || width > line_buffer.len() {
            return None;
        }

        let clear_width = if clear_end { width } else { end_col };
        let (start_col, end_col, clear_width) = if right_left {
            line_buffer.mirror(0, end_col, width);
            (width - clear_width, width - end_col, width)
        } else {
            (0, end_col, clear_width)
        };

        if let Some(marker) = smooth_scroll_marker {
            let marker_written = line_buffer.put_smoothscroll_marker(
                width,
                marker.preserve_line_number,
                marker.attribute,
            );
            debug_assert!(marker_written);
        }

        Some(Self {
            start_col,
            end_col,
            clear_width,
            right_left,
        })
    }

    /// Returns the first buffered column included in the submission.
    pub(super) fn start_col(self) -> usize {
        self.start_col
    }

    /// Returns the first buffered column after the submitted text.
    pub(super) fn end_col(self) -> usize {
        self.end_col
    }

    /// Returns the grid width through which unused cells are cleared.
    pub(super) fn clear_width(self) -> usize {
        self.clear_width
    }

    /// Returns whether the prepared cells use right-to-left ordering.
    pub(super) fn is_right_left(self) -> bool {
        self.right_left
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::VimSCharT;
    use crate::grid::schar_from_ascii;

    fn initialized_line_buffer(text: &str, width: usize) -> LineBuffer {
        let mut line_buffer = LineBuffer::new(width);
        line_buffer.fill(0..width, schar_from_ascii(' '), 1, -1);
        let characters = text.chars().map(schar_from_ascii).collect::<Vec<_>>();
        assert!(line_buffer.put_chars(0, &characters));
        line_buffer
    }

    fn characters(line_buffer: &LineBuffer) -> Vec<VimSCharT> {
        line_buffer
            .cells
            .iter()
            .map(|cell| cell.character)
            .collect()
    }

    #[test]
    fn prepares_left_to_right_ranges() {
        let mut line_buffer = initialized_line_buffer("abcd", 6);

        let submission =
            LineSubmission::prepare(&mut line_buffer, 4, 6, false, false, None).unwrap();

        assert_eq!(submission.start_col(), 0);
        assert_eq!(submission.end_col(), 4);
        assert_eq!(submission.clear_width(), 4);
        assert!(!submission.is_right_left());
    }

    #[test]
    fn prepares_a_full_clear_range() {
        let mut line_buffer = initialized_line_buffer("abcd", 6);

        let submission =
            LineSubmission::prepare(&mut line_buffer, 4, 6, true, false, None).unwrap();

        assert_eq!(submission.start_col(), 0);
        assert_eq!(submission.end_col(), 4);
        assert_eq!(submission.clear_width(), 6);
    }

    #[test]
    fn mirrors_right_to_left_text_and_translates_its_range() {
        let mut line_buffer = initialized_line_buffer("abcd", 6);

        let submission =
            LineSubmission::prepare(&mut line_buffer, 4, 6, false, true, None).unwrap();

        assert_eq!(submission.start_col(), 2);
        assert_eq!(submission.end_col(), 2);
        assert_eq!(submission.clear_width(), 6);
        assert!(submission.is_right_left());
        assert_eq!(
            &characters(&line_buffer)[2..],
            [
                schar_from_ascii('d'),
                schar_from_ascii('c'),
                schar_from_ascii('b'),
                schar_from_ascii('a'),
            ]
        );
    }

    #[test]
    fn mirrors_right_to_left_text_with_a_full_clear_range() {
        let mut line_buffer = initialized_line_buffer("abcd", 6);

        let submission = LineSubmission::prepare(&mut line_buffer, 4, 6, true, true, None).unwrap();

        assert_eq!(submission.start_col(), 0);
        assert_eq!(submission.end_col(), 2);
        assert_eq!(submission.clear_width(), 6);
        assert!(submission.is_right_left());
    }

    #[test]
    fn applies_the_smooth_scroll_marker_after_range_preparation() {
        let mut line_buffer = initialized_line_buffer("12xyzw", 6);

        let submission = LineSubmission::prepare(
            &mut line_buffer,
            6,
            6,
            false,
            false,
            Some(SmoothScrollMarker::new(true, 42)),
        )
        .unwrap();

        assert_eq!(submission.end_col(), 6);
        assert_eq!(
            characters(&line_buffer),
            [
                schar_from_ascii('1'),
                schar_from_ascii('2'),
                schar_from_ascii('<'),
                schar_from_ascii('<'),
                schar_from_ascii('<'),
                schar_from_ascii('w'),
            ]
        );
        assert_eq!(
            line_buffer
                .cells
                .iter()
                .map(|cell| cell.attribute)
                .collect::<Vec<_>>(),
            [1, 1, 42, 42, 42, 1]
        );
    }

    #[test]
    fn rejects_ranges_outside_the_line_buffer() {
        let mut line_buffer = initialized_line_buffer("abcd", 4);
        let original = line_buffer.cells.clone();

        assert!(LineSubmission::prepare(&mut line_buffer, 4, 5, true, true, None).is_none());
        assert_eq!(line_buffer.cells, original);
    }
}
