#![forbid(unsafe_code)]

use std::{borrow::Cow, ops::Range};

use crate::{
    grid::{GridDelta, GridSize, Position, Region, VimSAttrT, VimSCharT},
    ui::LineFlags,
};

/// Receives validated UI commands belonging to coordinate space `Space`.
pub(crate) trait UiOutput<Space> {
    /// Executes one UI command.
    fn emit(&mut self, command: UiCommand<'_, Space>);
}

/// One complete operation to be emitted outside implementation logic.
pub(crate) enum UiCommand<'cells, Space> {
    /// Resize the output grid.
    GridResize(GridSize),

    /// Move the cursor within the output grid.
    GridCursorGoto(Position<Space>),

    /// Scroll a region of the output grid.
    GridScroll {
        region: Region<Space>,
        delta: GridDelta,
    },

    /// Submit one raw-line update to the output grid.
    RawLine(RawLineCommand<'cells>),

    /// Write one diagnostic.
    Log(String),

    /// Flush pending UI output.
    Flush,

    /// Pause output for the requested number of milliseconds.
    Delay(u64),
}

/// A validated raw-line update with equally sized character and attribute
/// payloads.
pub(crate) struct RawLineCommand<'cells> {
    pub(crate) row: i32,
    pub(crate) columns: Range<i32>,
    pub(crate) clear_column: i32,
    pub(crate) clear_attribute: VimSAttrT,
    pub(crate) flags: LineFlags,
    pub(crate) characters: Cow<'cells, [VimSCharT]>,
    pub(crate) attributes: Cow<'cells, [VimSAttrT]>,
}

impl<'cells> RawLineCommand<'cells> {
    /// Constructs a raw-line command borrowing matching payload slices.
    pub(crate) fn try_new(
        row: i32,
        columns: Range<i32>,
        clear_column: i32,
        clear_attribute: VimSAttrT,
        flags: LineFlags,
        characters: &'cells [VimSCharT],
        attributes: &'cells [VimSAttrT],
    ) -> Option<Self> {
        Self::try_from_payloads(
            row,
            columns,
            clear_column,
            clear_attribute,
            flags,
            Cow::Borrowed(characters),
            Cow::Borrowed(attributes),
        )
    }

    /// Validates one borrowed or owned raw-line payload.
    #[allow(clippy::too_many_arguments)]
    fn try_from_payloads(
        row: i32,
        columns: Range<i32>,
        clear_column: i32,
        clear_attribute: VimSAttrT,
        flags: LineFlags,
        characters: Cow<'cells, [VimSCharT]>,
        attributes: Cow<'cells, [VimSAttrT]>,
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

impl RawLineCommand<'static> {
    /// Constructs a raw-line command owning matching payload vectors.
    pub(crate) fn try_owned(
        row: i32,
        columns: Range<i32>,
        clear_column: i32,
        clear_attribute: VimSAttrT,
        flags: LineFlags,
        characters: Vec<VimSCharT>,
        attributes: Vec<VimSAttrT>,
    ) -> Option<Self> {
        Self::try_from_payloads(
            row,
            columns,
            clear_column,
            clear_attribute,
            flags,
            Cow::Owned(characters),
            Cow::Owned(attributes),
        )
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
            RawLineCommand::try_owned(0, 2..4, 5, 0, LineFlags::default(), vec![1], vec![3, 4],)
                .is_none()
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
            RawLineCommand::try_owned(0, 2..4, 3, 0, LineFlags::default(), vec![1, 2], vec![3, 4],)
                .is_none()
        );
    }
}
