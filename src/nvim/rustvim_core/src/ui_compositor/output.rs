#![forbid(unsafe_code)]

use std::ops::Range;

use crate::{
    grid::{ComposedPosition, ComposedRegion, GridDelta, GridSize, VimSAttrT, VimSCharT},
    ui::LineFlags,
};

/// Receives validated output commands produced by the safe compositor.
pub(super) trait CompositorOutput {
    /// Executes one compositor output command.
    fn emit(&mut self, command: UiCommand<'_>);
}

/// One complete operation to be emitted outside compositor implementation
/// logic.
pub(super) enum UiCommand<'cells> {
    /// Resize the composed output grid.
    GridResize(GridSize),

    /// Move the cursor on the composed output grid.
    GridCursorGoto(ComposedPosition),

    /// Scroll a region of the composed output grid.
    GridScroll {
        region: ComposedRegion,
        delta: GridDelta,
    },

    /// Submit one raw-line update to the composed output grid.
    RawLine(RawLineCommand<'cells>),

    /// Write one compositor diagnostic.
    Log(String),

    /// Flush pending UI output.
    Flush,

    /// Pause output for the requested number of milliseconds.
    Delay(u64),
}

/// A validated raw-line update borrowing equally sized character and
/// attribute slices.
pub(super) struct RawLineCommand<'cells> {
    pub(super) row: i32,
    pub(super) columns: Range<i32>,
    pub(super) clear_column: i32,
    pub(super) clear_attribute: VimSAttrT,
    pub(super) flags: LineFlags,
    pub(super) characters: &'cells [VimSCharT],
    pub(super) attributes: &'cells [VimSAttrT],
}

impl<'cells> RawLineCommand<'cells> {
    /// Constructs a raw-line command whose payload matches its changed range.
    pub(super) fn try_new(
        row: i32,
        columns: Range<i32>,
        clear_column: i32,
        clear_attribute: VimSAttrT,
        flags: LineFlags,
        characters: &'cells [VimSCharT],
        attributes: &'cells [VimSAttrT],
    ) -> Option<Self> {
        if columns.start > columns.end || columns.end > clear_column {
            return None;
        }
        let payload_len = usize::try_from(columns.end.checked_sub(columns.start)?).ok()?;
        if characters.len() != payload_len || attributes.len() != payload_len {
            return None;
        }

        Some(Self {
            row,
            columns,
            clear_column,
            clear_attribute,
            flags,
            characters,
            attributes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_line_requires_matching_payload_lengths() {
        assert!(
            RawLineCommand::try_new(0, 2..4, 5, 0, LineFlags::default(), &[1, 2], &[3, 4],)
                .is_some()
        );
        assert!(
            RawLineCommand::try_new(0, 2..4, 5, 0, LineFlags::default(), &[1], &[3, 4]).is_none()
        );
    }

    #[test]
    fn raw_line_requires_ordered_columns() {
        let reversed_columns = Range { start: 4, end: 2 };
        assert!(
            RawLineCommand::try_new(0, reversed_columns, 5, 0, LineFlags::default(), &[], &[],)
                .is_none()
        );
        assert!(
            RawLineCommand::try_new(0, 2..4, 3, 0, LineFlags::default(), &[1, 2], &[3, 4],)
                .is_none()
        );
    }
}
