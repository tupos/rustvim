#![forbid(unsafe_code)]

use std::{ops::Range, sync::Mutex};

use crate::{
    grid::{GridDelta, GridSize, Position, Region, VimSAttrT, VimSCharT},
    ui::LineFlags,
};

/// Receives validated UI commands belonging to coordinate space `Space`.
pub(crate) trait UiOutput<Space> {
    /// Executes one UI command without a cell payload.
    fn emit(&mut self, command: UiCommand<Space>);

    /// Executes one raw-line command with statically described cell ownership.
    fn emit_raw_line<Cells: RawLineCells>(&mut self, command: RawLineCommand<Cells>);
}

/// One complete operation to be emitted outside implementation logic.
pub(crate) enum UiCommand<Space> {
    /// Resize the output grid.
    GridResize(GridSize),

    /// Move the cursor within the output grid.
    GridCursorGoto(Position<Space>),

    /// Scroll a region of the output grid.
    GridScroll {
        region: Region<Space>,
        delta: GridDelta,
    },

    /// Write one diagnostic.
    Log(String),

    /// Flush pending UI output.
    Flush,

    /// Pause output for the requested number of milliseconds.
    Delay(u64),
}

/// Provides the parallel cell slices carried by one raw-line command.
pub(crate) trait RawLineCells {
    /// Returns the screen characters in the changed range.
    fn characters(&self) -> &[VimSCharT];

    /// Returns the attributes in the changed range.
    fn attributes(&self) -> &[VimSAttrT];
}

/// Cell payload borrowed for the duration of synchronous output.
pub(crate) struct BorrowedRawLineCells<'cells> {
    characters: &'cells [VimSCharT],
    attributes: &'cells [VimSAttrT],
}

impl<'cells> BorrowedRawLineCells<'cells> {
    /// Borrows two equally sized cell arrays.
    fn try_new(characters: &'cells [VimSCharT], attributes: &'cells [VimSAttrT]) -> Option<Self> {
        (characters.len() == attributes.len()).then_some(Self {
            characters,
            attributes,
        })
    }
}

impl RawLineCells for BorrowedRawLineCells<'_> {
    fn characters(&self) -> &[VimSCharT] {
        self.characters
    }

    fn attributes(&self) -> &[VimSAttrT] {
        self.attributes
    }
}

/// Reusable parallel storage for screen characters and attributes.
pub(crate) struct CellBuffer {
    pub(crate) characters: Vec<VimSCharT>,
    pub(crate) attributes: Vec<VimSAttrT>,
}

impl CellBuffer {
    /// Creates an empty cell buffer.
    pub(crate) const fn new() -> Self {
        Self {
            characters: Vec::new(),
            attributes: Vec::new(),
        }
    }

    /// Resizes both parallel arrays to `width`.
    pub(crate) fn resize(&mut self, width: usize) {
        self.characters.resize(width, 0);
        self.attributes.resize(width, 0);
    }

    /// Releases both cell arrays.
    #[cfg(test)]
    pub(crate) fn release(&mut self) {
        self.characters = Vec::new();
        self.attributes = Vec::new();
    }
}

/// Reusable cell storage that can be checked out without retaining its mutex.
pub(crate) struct CellBufferPool {
    width: usize,
    available_buffers: Vec<CellBuffer>,
}

impl CellBufferPool {
    /// Creates an empty reusable buffer pool.
    pub(crate) const fn new() -> Self {
        Self {
            width: 0,
            available_buffers: Vec::new(),
        }
    }

    /// Returns the configured cell width.
    pub(crate) fn width(&self) -> usize {
        self.width
    }

    /// Reconfigures retained storage for a new cell width.
    pub(crate) fn resize(&mut self, width: usize) {
        self.width = width;
        for buffer in &mut self.available_buffers {
            buffer.resize(width);
        }
    }

    /// Releases all retained cell storage.
    pub(crate) fn release(&mut self) {
        self.width = 0;
        self.available_buffers.clear();
    }

    /// Returns the number of retained buffers.
    #[cfg(test)]
    pub(crate) fn available_buffer_count(&self) -> usize {
        self.available_buffers.len()
    }

    /// Checks out reusable storage or allocates storage for reentrant work.
    fn take_buffer(&mut self) -> CellBuffer {
        let mut buffer = self.available_buffers.pop().unwrap_or_else(CellBuffer::new);
        buffer.resize(self.width);
        buffer
    }

    /// Retains returned storage when it still has the configured width.
    fn return_buffer(&mut self, buffer: CellBuffer) {
        let has_current_width =
            buffer.characters.len() == self.width && buffer.attributes.len() == self.width;
        if has_current_width {
            self.available_buffers.push(buffer);
        }
    }
}

/// Exclusive ownership of cell storage without retaining the pool lock.
#[must_use = "dropping the lease returns its cell buffer to the pool"]
pub(crate) struct CellBufferLease<'pool> {
    pool: &'pool Mutex<CellBufferPool>,
    buffer: Option<CellBuffer>,
}

impl<'pool> CellBufferLease<'pool> {
    /// Checks out one buffer while holding the pool lock only briefly.
    pub(crate) fn acquire(pool: &'pool Mutex<CellBufferPool>) -> Self {
        let buffer = pool
            .try_lock()
            .expect("cell buffer pool is already borrowed")
            .take_buffer();
        Self {
            pool,
            buffer: Some(buffer),
        }
    }

    /// Configures the pool width and checks out one matching buffer.
    fn acquire_with_width(pool: &'pool Mutex<CellBufferPool>, width: usize) -> Self {
        let buffer = {
            let mut pool_guard = pool
                .try_lock()
                .expect("cell buffer pool is already borrowed");
            pool_guard.resize(width);
            pool_guard.take_buffer()
        };
        Self {
            pool,
            buffer: Some(buffer),
        }
    }
}

impl std::ops::Deref for CellBufferLease<'_> {
    type Target = CellBuffer;

    fn deref(&self) -> &Self::Target {
        self.buffer
            .as_ref()
            .expect("cell buffer lease does not own a buffer")
    }
}

impl std::ops::DerefMut for CellBufferLease<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.buffer
            .as_mut()
            .expect("cell buffer lease does not own a buffer")
    }
}

impl Drop for CellBufferLease<'_> {
    fn drop(&mut self) {
        let buffer = self
            .buffer
            .take()
            .expect("cell buffer lease does not own a buffer");
        self.pool
            .try_lock()
            .expect("cell buffer pool is already borrowed")
            .return_buffer(buffer);
    }
}

/// Cell payload retained independently of its source grid borrow.
pub(crate) struct OwnedRawLineCells {
    buffer: CellBufferLease<'static>,
    payload_len: usize,
}

impl OwnedRawLineCells {
    /// Copies matching slices into a reusable owned buffer.
    fn try_copy_from(
        pool: &'static Mutex<CellBufferPool>,
        characters: &[VimSCharT],
        attributes: &[VimSAttrT],
    ) -> Option<Self> {
        if characters.len() != attributes.len() {
            return None;
        }

        let payload_len = characters.len();
        let mut buffer = CellBufferLease::acquire_with_width(pool, payload_len);
        buffer.characters.copy_from_slice(characters);
        buffer.attributes.copy_from_slice(attributes);
        Some(Self {
            buffer,
            payload_len,
        })
    }
}

impl RawLineCells for OwnedRawLineCells {
    fn characters(&self) -> &[VimSCharT] {
        &self.buffer.characters[..self.payload_len]
    }

    fn attributes(&self) -> &[VimSAttrT] {
        &self.buffer.attributes[..self.payload_len]
    }
}

/// A validated raw-line update whose payload ownership is carried by `Cells`.
pub(crate) struct RawLineCommand<Cells> {
    pub(crate) row: i32,
    pub(crate) columns: Range<i32>,
    pub(crate) clear_column: i32,
    pub(crate) clear_attribute: VimSAttrT,
    pub(crate) flags: LineFlags,
    cells: Cells,
}

impl<Cells: RawLineCells> RawLineCommand<Cells> {
    /// Validates one raw-line payload.
    fn try_from_cells(
        row: i32,
        columns: Range<i32>,
        clear_column: i32,
        clear_attribute: VimSAttrT,
        flags: LineFlags,
        cells: Cells,
    ) -> Option<Self> {
        if columns.start > columns.end || columns.end > clear_column {
            return None;
        }
        let payload_len = usize::try_from(columns.end.checked_sub(columns.start)?).ok()?;
        if cells.characters().len() != payload_len || cells.attributes().len() != payload_len {
            return None;
        }

        Some(Self {
            row,
            columns,
            clear_column,
            clear_attribute,
            flags,
            cells,
        })
    }

    /// Returns the screen characters in the changed range.
    pub(crate) fn characters(&self) -> &[VimSCharT] {
        self.cells.characters()
    }

    /// Returns the attributes in the changed range.
    pub(crate) fn attributes(&self) -> &[VimSAttrT] {
        self.cells.attributes()
    }
}

pub(crate) type BorrowedRawLineCommand<'cells> = RawLineCommand<BorrowedRawLineCells<'cells>>;
pub(crate) type OwnedRawLineCommand = RawLineCommand<OwnedRawLineCells>;

impl<'cells> BorrowedRawLineCommand<'cells> {
    /// Constructs a raw-line command borrowing matching payload slices.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_new(
        row: i32,
        columns: Range<i32>,
        clear_column: i32,
        clear_attribute: VimSAttrT,
        flags: LineFlags,
        characters: &'cells [VimSCharT],
        attributes: &'cells [VimSAttrT],
    ) -> Option<Self> {
        Self::try_from_cells(
            row,
            columns,
            clear_column,
            clear_attribute,
            flags,
            BorrowedRawLineCells::try_new(characters, attributes)?,
        )
    }
}

impl OwnedRawLineCommand {
    /// Constructs a raw-line command by copying matching payload slices into
    /// reusable storage.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn try_owned(
        row: i32,
        columns: Range<i32>,
        clear_column: i32,
        clear_attribute: VimSAttrT,
        flags: LineFlags,
        characters: &[VimSCharT],
        attributes: &[VimSAttrT],
        pool: &'static Mutex<CellBufferPool>,
    ) -> Option<Self> {
        Self::try_from_cells(
            row,
            columns,
            clear_column,
            clear_attribute,
            flags,
            OwnedRawLineCells::try_copy_from(pool, characters, attributes)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_POOL: Mutex<CellBufferPool> = Mutex::new(CellBufferPool::new());

    #[test]
    fn raw_line_requires_matching_payload_lengths() {
        assert!(
            BorrowedRawLineCommand::try_new(0, 2..4, 5, 0, LineFlags::default(), &[1, 2], &[3, 4],)
                .is_some()
        );
        assert!(
            OwnedRawLineCommand::try_owned(
                0,
                2..4,
                5,
                0,
                LineFlags::default(),
                &[1],
                &[3, 4],
                &TEST_POOL,
            )
            .is_none()
        );
    }

    #[test]
    fn raw_line_requires_ordered_columns() {
        let reversed_columns = Range { start: 4, end: 2 };
        assert!(
            BorrowedRawLineCommand::try_new(
                0,
                reversed_columns,
                5,
                0,
                LineFlags::default(),
                &[],
                &[],
            )
            .is_none()
        );
        assert!(
            OwnedRawLineCommand::try_owned(
                0,
                2..4,
                3,
                0,
                LineFlags::default(),
                &[1, 2],
                &[3, 4],
                &TEST_POOL,
            )
            .is_none()
        );
    }
}
