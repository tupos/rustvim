//! Process-lifetime ownership and operations of the compositor subsystem.

#![forbid(unsafe_code)]

use std::sync::{Mutex, MutexGuard};

use crate::{
    grid::{ComposedGridSpace, DEFAULT_GRID_ID, GridContext, GridId, GridSize},
    ui::{
        LineFlags,
        output::{
            BorrowedRawLineCommand, CellBufferLease, CellBufferPool, RawLineCells, RawLineCommand,
            UiCommand, UiOutput,
        },
    },
};

use super::{
    Compositor, CompositorDebugHighlights, CompositorError, CompositorLineInput,
    CompositorRawLineAction, CompositorRawLineOperation, CompositorRawLineRequest,
    CompositorRecomposeArea, CompositorResult, CompositorRuntime, CompositorScrollOperation,
    CompositorScrollRequest, RawLineDiagnostic, log_raw_line_diagnostic,
};

/// State owned by the compositor subsystem for the lifetime of the editor.
pub(crate) struct CompositorContext {
    state: Mutex<Compositor>,
    buffer_pool: Mutex<CellBufferPool>,
    debug_highlights: Mutex<CompositorDebugHighlights>,
}

impl CompositorContext {
    /// Constructs compositor state before subsystem initialization.
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(Compositor::new()),
            buffer_pool: Mutex::new(CellBufferPool::new()),
            debug_highlights: Mutex::new(CompositorDebugHighlights::empty()),
        }
    }

    /// Processes one validated grid-resize event.
    ///
    /// A resize for a non-default grid is intentionally ignored because
    /// composed output always targets the default grid.
    pub(crate) fn process_grid_resize(
        &self,
        grid_id: GridId,
        size: GridSize,
        output: &mut impl UiOutput<ComposedGridSpace>,
    ) -> CompositorResult<()> {
        if grid_id != DEFAULT_GRID_ID {
            return Ok(());
        }

        let plan = self.lock_state().resize(size)?;

        output.emit(UiCommand::GridResize(plan.size));

        self.lock_buffer_pool().resize(plan.buffer_width);
        Ok(())
    }

    /// Composes and submits one validated line from registered grids.
    pub(in crate::ui_compositor) fn compose_registered_line(
        &self,
        grids: &GridContext,
        message_grid_id: Option<GridId>,
        request: CompositorLineInput,
        output: &mut impl UiOutput<ComposedGridSpace>,
    ) -> CompositorResult<()> {
        let separator_active = self
            .lock_state()
            .message_separator_character_at(request.row)
            .is_some();
        let mut line_buffer = self.acquire_buffer();
        let submission = {
            let grid_store = grids.lock_store();
            let message_grid = if separator_active {
                let message_grid_id = message_grid_id.ok_or(CompositorError::InvalidLayerState)?;
                Some(
                    grid_store
                        .grid(message_grid_id)
                        .map_err(|_| CompositorError::InvalidLayerState)?,
                )
            } else {
                None
            };
            self.lock_state()
                .compose_line(&mut line_buffer, &grid_store, message_grid, &request)
        }?;

        let output_range = submission.start_col..submission.end_col;
        let characters = line_buffer
            .characters
            .get(output_range.clone())
            .ok_or(CompositorError::InvalidLineState)?;
        let attributes = line_buffer
            .attributes
            .get(output_range)
            .ok_or(CompositorError::InvalidLineState)?;
        let start_column =
            i32::try_from(submission.start_col).map_err(|_| CompositorError::InvalidGeometry)?;
        let end_column =
            i32::try_from(submission.end_col).map_err(|_| CompositorError::InvalidGeometry)?;
        let command = BorrowedRawLineCommand::try_new(
            request.row,
            start_column..end_column,
            end_column,
            0,
            submission.flags,
            characters,
            attributes,
        )
        .ok_or(CompositorError::InvalidLineState)?;
        output.emit_raw_line(command);
        Ok(())
    }

    /// Processes one validated raw-line event.
    ///
    /// A disabled compositor or a grid absent from the registered layers
    /// produces a successful no-op.
    pub(crate) fn process_grid_raw_line<Cells: RawLineCells>(
        &self,
        grids: &GridContext,
        message_grid_id: Option<GridId>,
        grid_id: GridId,
        line: &RawLineCommand<Cells>,
        runtime: CompositorRuntime,
        output: &mut impl UiOutput<ComposedGridSpace>,
    ) -> CompositorResult<()> {
        let request = CompositorRawLineRequest {
            row: line.row,
            columns: line.columns.clone(),
            clear_column: line.clear_column,
            flags: line.flags,
        };
        let operation = {
            let grid_store = grids.lock_store();
            let compositor = self.lock_state();
            let message_layer_index = message_grid_id
                .map(|grid_id| compositor.layer_index(grid_id))
                .transpose()?
                .flatten();
            compositor.process_raw_line(&grid_store, grid_id, message_layer_index, request)?
        };
        let CompositorRawLineOperation::Process(plan) = operation else {
            return Ok(());
        };

        if plan.clear_column != plan.requested_clear_column {
            log_raw_line_diagnostic(
                RawLineDiagnostic::ClippedColumns(plan.requested_clear_column),
                grid_id,
                output,
            );
        }

        match plan.action {
            CompositorRawLineAction::DiscardRow => {
                log_raw_line_diagnostic(RawLineDiagnostic::InvalidRow(plan.row), grid_id, output);
                Ok(())
            }
            CompositorRawLineAction::DiscardColumns => Ok(()),
            CompositorRawLineAction::Recompose => {
                runtime.visualize(
                    &CompositorRecomposeArea {
                        rows: plan.row..plan.row + 1,
                        columns: plan.columns.start..plan.clear_column,
                    },
                    runtime.debug_highlights.composed,
                    true,
                    output,
                );
                self.compose_registered_line(
                    grids,
                    message_grid_id,
                    CompositorLineInput {
                        row: plan.row,
                        columns: plan.columns.start..plan.clear_column,
                        flags: plan.flags,
                        settings: runtime.line_settings,
                    },
                    output,
                )
            }
            CompositorRawLineAction::Forward => {
                let payload_len = plan
                    .columns
                    .end
                    .checked_sub(plan.columns.start)
                    .and_then(|length| usize::try_from(length).ok())
                    .ok_or(CompositorError::InvalidGeometry)?;
                let characters = line
                    .characters()
                    .get(..payload_len)
                    .ok_or(CompositorError::InvalidLineState)?;
                let attributes = line
                    .attributes()
                    .get(..payload_len)
                    .ok_or(CompositorError::InvalidLineState)?;

                runtime.visualize(
                    &CompositorRecomposeArea {
                        rows: plan.row..plan.row + 1,
                        columns: plan.columns.clone(),
                    },
                    runtime.debug_highlights.normal,
                    plan.columns.end >= plan.clear_column,
                    output,
                );
                runtime.visualize(
                    &CompositorRecomposeArea {
                        rows: plan.row..plan.row + 1,
                        columns: plan.columns.end..plan.clear_column,
                    },
                    runtime.debug_highlights.clear,
                    true,
                    output,
                );
                debug_assert!(attributes.iter().all(|attribute| *attribute >= 0));

                let command = BorrowedRawLineCommand::try_new(
                    plan.row,
                    plan.columns,
                    plan.clear_column,
                    line.clear_attribute,
                    plan.flags,
                    characters,
                    attributes,
                )
                .ok_or(CompositorError::InvalidLineState)?;
                output.emit_raw_line(command);
                Ok(())
            }
        }
    }

    /// Processes one validated grid-scroll event.
    ///
    /// A disabled compositor or a grid absent from the registered layers
    /// produces a successful no-op.
    pub(crate) fn process_grid_scroll(
        &self,
        grids: &GridContext,
        message_grid_id: Option<GridId>,
        grid_id: GridId,
        request: CompositorScrollRequest,
        runtime: CompositorRuntime,
        output: &mut impl UiOutput<ComposedGridSpace>,
    ) -> CompositorResult<()> {
        let operation = {
            let grid_store = grids.lock_store();
            let compositor = self.lock_state();
            let message_layer_index = message_grid_id
                .map(|grid_id| compositor.layer_index(grid_id))
                .transpose()?
                .flatten();
            compositor.scroll(&grid_store, grid_id, message_layer_index, request)?
        };

        match operation {
            CompositorScrollOperation::Ignore => Ok(()),
            CompositorScrollOperation::Forward { region, delta } => {
                output.emit(UiCommand::GridScroll { region, delta });
                if runtime.debug_enabled {
                    runtime.delay(2, output);
                }
                Ok(())
            }
            CompositorScrollOperation::Recompose { region, rows } => {
                runtime.visualize(
                    &CompositorRecomposeArea {
                        rows: region.rows.clone(),
                        columns: region.columns.clone(),
                    },
                    runtime.debug_highlights.recomposed,
                    true,
                    output,
                );
                for row in rows {
                    self.compose_registered_line(
                        grids,
                        message_grid_id,
                        CompositorLineInput {
                            row,
                            columns: region.columns.clone(),
                            flags: LineFlags::default(),
                            settings: runtime.line_settings,
                        },
                        output,
                    )?;
                }
                Ok(())
            }
        }
    }

    /// Acquires the safe compositor state.
    pub(in crate::ui_compositor) fn lock_state(&self) -> MutexGuard<'_, Compositor> {
        self.state
            .try_lock()
            .expect("compositor state is already borrowed")
    }

    /// Acquires the compositor buffer pool.
    pub(in crate::ui_compositor) fn lock_buffer_pool(&self) -> MutexGuard<'_, CellBufferPool> {
        self.buffer_pool
            .try_lock()
            .expect("compositor buffer pool is already borrowed")
    }

    /// Checks out compositor storage without retaining the pool mutex.
    pub(in crate::ui_compositor) fn acquire_buffer(&self) -> CellBufferLease<'_> {
        CellBufferLease::acquire(&self.buffer_pool)
    }

    /// Acquires compositor debug-highlight identities.
    pub(in crate::ui_compositor) fn lock_debug_highlights(
        &self,
    ) -> MutexGuard<'_, CompositorDebugHighlights> {
        self.debug_highlights
            .try_lock()
            .expect("compositor debug highlights are already borrowed")
    }
}
