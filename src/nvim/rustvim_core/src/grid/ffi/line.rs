//! C boundary adapters for buffered grid-line rendering.

use super::super::CGridLineSubmissionConfig;
use super::super::CStr;
use super::super::CVirtText;
use super::super::FoldColumn;
use super::super::FoldColumnInfo;
use super::super::FoldColumnSymbols;
use super::super::GridId;
use super::super::GridOutputTarget;
use super::super::GridViewDescriptor;
use super::super::Highlight;
use super::super::LineBufferGuard;
use super::super::LineSubmission;
use super::super::LineTextRenderOptions;
use super::super::LineTextVirtualColumns;
use super::super::OptInt;
use super::super::Slf;
use super::super::SmoothScrollMarker;
use super::super::StoredGridGuard;
use super::super::TabStops;
use super::super::TruncationSide;
use super::super::VimColNrT;
use super::super::VimHandleT;
use super::super::VimSAttrT;
use super::super::VimSCharT;
use super::super::VirtualText;
use super::super::VirtualTextChunk;
use super::super::VirtualTextHighlightMode;
use super::super::VirtualTextRenderOptions;
use super::super::lock_glyph_mapping;
use super::super::lock_grid_store;
use super::super::lock_message_grid_state;
use super::super::schar_from_ascii;
use super::super::with_line_grid;

/// Begins a buffered line operation through a persistent view descriptor.
fn begin_grid_view_line(descriptor: GridViewDescriptor, row: i32) {
    let row = row
        .checked_add(descriptor.offset.row)
        .expect("line grid row translation overflowed");

    let mut line_buffer = LineBufferGuard::acquire();
    with_line_grid(|line_grid| {
        line_grid.begin(
            descriptor.target,
            row,
            descriptor.offset.column,
            &mut line_buffer,
        );
    });
}

/// Starts a buffered line operation through a registered window view.
///
/// The matching flush must occur on the same thread.
#[unsafe(no_mangle)]
pub extern "C" fn grid_window_line_start(window_grid_handle: VimHandleT, row: libc::c_int) {
    let descriptor = lock_grid_store()
        .window_view_descriptor(GridId::new(window_grid_handle))
        .expect("window grid must own a registered view");
    begin_grid_view_line(descriptor, row);
}

/// Begins a buffered line operation through the adjusted message view.
///
/// The matching flush must occur on the same thread.
#[unsafe(no_mangle)]
pub extern "C" fn grid_message_view_line_start(row: libc::c_int) {
    let descriptor = lock_message_grid_state().view();
    begin_grid_view_line(descriptor, row);
}

/// Begins a buffered line operation on an identified screen grid.
///
/// The matching flush must occur on the same thread.
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_start_grid(
    grid_handle: VimHandleT,
    row: libc::c_int,
    col: libc::c_int,
) {
    let mut line_buffer = LineBufferGuard::acquire();
    with_line_grid(|line_grid| {
        line_grid.begin(GridId::new(grid_handle), row, col, &mut line_buffer);
    });
}

/// Get present char from current rendered screen line
///
/// This indicates what already is on screen, not the pending render buffer.
///
/// @return char or space if out of bounds
///
/// # Safety
///
/// An active line operation must reference a valid grid. `attr` must be null
/// or point to valid, properly aligned writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_line_getchar(col: libc::c_int, attr: *mut VimSAttrT) -> VimSCharT {
    let (sc, a) = with_line_grid(|line_grid| line_grid.read_cell(col));
    if !attr.is_null() {
        unsafe {
            *attr = a;
        }
    }
    sc
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_put_schar(col: libc::c_int, schar: VimSCharT, attr: VimSAttrT) {
    let mut line_buffer = LineBufferGuard::acquire();
    with_line_grid(|line_grid| {
        line_grid.put_cell(col, schar, attr, &mut line_buffer);
    });
}

/// # Safety
///
/// An active line operation must reference a valid grid and line buffer. If
/// `text_len` is `-1`, `text` must point to a NUL-terminated string.
/// Otherwise, `text_len` must be nonnegative and `text` must be valid for
/// reads of that many bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_line_puts(
    col: libc::c_int,
    text: *const libc::c_char,
    text_len: libc::c_int,
    attr: VimSAttrT,
) -> libc::c_int {
    let mut line_buffer = LineBufferGuard::acquire();
    let len = if text_len == -1 {
        unsafe { libc::strlen(text) }
    } else {
        text_len as usize
    };
    // SAFETY: The caller guarantees that `text` covers `len` readable bytes.
    let text = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), len) };
    let num_cells =
        with_line_grid(|line_grid| line_grid.put_text(col, text, attr, &mut line_buffer));
    num_cells as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_fill(
    start_col: libc::c_int,
    end_col: libc::c_int,
    sc: VimSCharT,
    attr: libc::c_int,
) -> libc::c_int {
    let mut line_buffer = LineBufferGuard::acquire();
    with_line_grid(|line_grid| line_grid.fill_cells(start_col, end_col, sc, attr, &mut line_buffer))
}

/// Writes a popup-menu truncation marker into the current line buffer.
///
/// Invalid signed values, integer overflow, empty ranges, and ranges outside
/// the initialized line buffer are ignored.
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_put_truncation(
    start_col: libc::c_int,
    width: libc::c_int,
    marker: VimSCharT,
    attr: VimSAttrT,
    right_to_left: bool,
) {
    let (Ok(start_col), Ok(width)) = (usize::try_from(start_col), usize::try_from(width)) else {
        return;
    };
    let Some(end_col) = start_col.checked_add(width) else {
        return;
    };
    let side = if right_to_left {
        TruncationSide::Left
    } else {
        TruncationSide::Right
    };
    let marker = (marker != 0).then_some(marker);

    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.put_truncation_marker(start_col..end_col, side, marker, attr);
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_clear_end(
    start_col: libc::c_int,
    end_col: libc::c_int,
    bg_attr: VimSAttrT,
    clear_attr: VimSAttrT,
) {
    with_line_grid(|line_grid| {
        line_grid.set_clear_range(start_col, end_col, bg_attr, clear_attr);
    });
}

/// move the cursor to a position in a currently rendered line.
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_cursor_goto(col: libc::c_int) {
    let (target, command) = with_line_grid(|line_grid| line_grid.cursor_update(col));
    target.emit(command);
}

#[unsafe(no_mangle)]
pub extern "C" fn grid_line_mirror(width: libc::c_int) {
    let mut line_buffer = LineBufferGuard::acquire();
    with_line_grid(|line_grid| {
        line_grid.mirror_cells(width, &mut line_buffer);
    });
}

/// Resets the visible line-buffer cells before drawing a window line.
///
/// Negative and out-of-bounds widths are ignored.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_reset(width: libc::c_int) {
    let Ok(width) = usize::try_from(width) else {
        return;
    };
    let mut line_buffer = LineBufferGuard::acquire();
    if width <= line_buffer.len() {
        line_buffer.fill(0..width, schar_from_ascii(' '), 0, -1);
    }
}

/// Fills character and attribute cells and returns the resulting column.
///
/// Invalid positions, negative widths, integer overflow, and out-of-bounds
/// ranges leave both the line buffer and returned column unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_fill_chars_and_attrs(
    start_col: libc::c_int,
    width: libc::c_int,
    sc: VimSCharT,
    attr: VimSAttrT,
) -> libc::c_int {
    if start_col < 0 || width < 0 {
        return start_col;
    }
    let Some(end_col) = start_col.checked_add(width) else {
        return start_col;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    if line_buffer.fill_chars_and_attrs(start_col as usize..end_col as usize, sc, attr) {
        end_col
    } else {
        start_col
    }
}

/// Writes one complete line-buffer cell and returns the following column.
///
/// A negative or out-of-bounds column leaves both the line buffer and returned
/// column unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_put_cell(
    col: libc::c_int,
    sc: VimSCharT,
    attr: VimSAttrT,
    vcol: VimColNrT,
) -> libc::c_int {
    if col < 0 {
        return col;
    }
    let Some(next_col) = col.checked_add(1) else {
        return col;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    if line_buffer.update_cell(col as usize, Some(sc), attr, vcol) {
        next_col
    } else {
        col
    }
}

/// Generates and writes one complete fold column.
///
/// When both output pointers are null, cells are written directly to the
/// global line buffer. Otherwise both pointers must describe writable arrays
/// containing `width` elements.
///
/// Invalid dimensions or output pointers leave the destination unchanged and
/// return `start_col`.
///
/// # Safety
///
/// When non-null, `out_vcols` and `out_chars` must point to separate writable
/// arrays of `width` initialized elements. They must not alias Rust-owned
/// line-buffer storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn linebuf_render_fold_column(
    start_col: libc::c_int,
    attr: VimSAttrT,
    width: libc::c_int,
    level: libc::c_int,
    lowest_level: libc::c_int,
    starts_here: bool,
    is_closed: bool,
    virtual_line: bool,
    open: VimSCharT,
    closed: VimSCharT,
    separator: VimSCharT,
    inner: VimSCharT,
    out_vcols: *mut VimColNrT,
    out_chars: *mut VimSCharT,
) -> libc::c_int {
    let (Ok(start_col_index), Ok(width), Ok(level), Ok(lowest_level)) = (
        usize::try_from(start_col),
        usize::try_from(width),
        usize::try_from(level),
        usize::try_from(lowest_level),
    ) else {
        return start_col;
    };

    let fold_column = FoldColumn::render(
        width,
        FoldColumnInfo::new(level, lowest_level, starts_here, is_closed),
        FoldColumnSymbols::new(open, closed, separator, (inner != 0).then_some(inner)),
        virtual_line,
    );

    if out_vcols.is_null() && out_chars.is_null() {
        let mut line_buffer = LineBufferGuard::acquire();
        let end_col = line_buffer.put_fold_column(start_col_index, &fold_column, attr);
        return libc::c_int::try_from(end_col).unwrap_or(start_col);
    }
    if out_vcols.is_null() || out_chars.is_null() {
        return start_col;
    }

    // SAFETY: The caller guarantees that both output pointers address
    // separate writable arrays containing `width` initialized elements.
    let out_vcols = unsafe { std::slice::from_raw_parts_mut(out_vcols, width) };
    // SAFETY: The caller guarantees that this pointer addresses `width`
    // initialized elements and does not alias `out_vcols`.
    let out_chars = unsafe { std::slice::from_raw_parts_mut(out_chars, width) };
    if !fold_column.copy_to(out_chars, out_vcols) {
        return start_col;
    }

    start_col
        .checked_add(libc::c_int::try_from(width).unwrap_or(0))
        .unwrap_or(start_col)
}

/// Writes one rendered glyph and returns its screen-cell width.
///
/// Invalid positions, insufficient space, and virtual-column overflow leave
/// the line buffer unchanged and return zero.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_put_rendered_glyph(
    col: libc::c_int,
    sc: VimSCharT,
    attr: VimSAttrT,
    vcol: VimColNrT,
) -> libc::c_int {
    let Ok(col) = usize::try_from(col) else {
        return 0;
    };
    let mut line_buffer = LineBufferGuard::acquire();
    let glyph_map = lock_glyph_mapping();

    line_buffer
        .put_rendered_glyph(col, sc, attr, vcol, &glyph_map)
        .and_then(|width| libc::c_int::try_from(width).ok())
        .unwrap_or(0)
}

/// Copies `len` values from a C allocation into Rust-owned storage.
///
/// # Safety
///
/// For a nonzero `len`, `items` must point to `len` readable initialized
/// values.
unsafe fn copy_c_slice<T: Copy>(items: *const T, len: usize) -> Option<Vec<T>> {
    if len == 0 {
        return Some(Vec::new());
    }
    if items.is_null() {
        return None;
    }

    // SAFETY: The caller guarantees that `items` covers `len` initialized
    // values.
    Some(unsafe { std::slice::from_raw_parts(items, len) }.to_vec())
}

/// Copies and validates a C variable-tab-stop representation.
///
/// # Safety
///
/// A non-null `variable_tabstops` must address a readable count followed by
/// that many initialized widths.
unsafe fn tab_stops_from_c(
    fixed_width: OptInt,
    variable_tabstops: *const VimColNrT,
) -> Option<TabStops> {
    let variable_widths = if variable_tabstops.is_null() {
        Vec::new()
    } else {
        // SAFETY: The caller guarantees that the count is readable.
        let count = usize::try_from(unsafe { *variable_tabstops }).ok()?;
        if count == 0 {
            Vec::new()
        } else {
            // SAFETY: The caller guarantees that the count is followed by
            // `count` initialized widths.
            unsafe { copy_c_slice(variable_tabstops.add(1), count)? }
        }
    };

    TabStops::try_from_options(fixed_width, &variable_widths).ok()
}

/// C-compatible state returned after rendering a complete text run.
#[repr(C)]
pub struct CLineTextRenderResult {
    end_col: libc::c_int,
    end_vcol: VimColNrT,
    color_cols_consumed: libc::size_t,
    color_cols_exhausted: bool,
}

impl CLineTextRenderResult {
    fn unchanged(col: libc::c_int, vcol: VimColNrT) -> Self {
        Self {
            end_col: col,
            end_vcol: vcol,
            color_cols_consumed: 0,
            color_cols_exhausted: false,
        }
    }
}

/// Copies and renders one complete text run.
///
/// Invalid dimensions, pointers, or tab-stop values leave the line buffer
/// unchanged and return the original column state.
///
/// # Safety
///
/// For a nonzero `text_len`, `text` must point to that many readable bytes.
/// Every non-null `explicit_vcols` and `color_columns` pointer must address the
/// corresponding number of readable initialized values. A non-null
/// `variable_tabstops` must address a readable count followed by that many
/// initialized widths. These allocations must remain valid for this call and
/// must not alias Rust-owned line-buffer storage.
///
/// The global line buffer must be initialized. This function must run on
/// Neovim's main thread while the highlight tables are not otherwise accessed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn linebuf_render_text(
    col: libc::c_int,
    text: *const libc::c_char,
    text_len: libc::size_t,
    max_col: libc::c_int,
    attr: VimSAttrT,
    inc_vcol: bool,
    first_vcol: VimColNrT,
    explicit_vcols: *const VimColNrT,
    explicit_vcols_len: libc::size_t,
    color_columns: *const VimColNrT,
    color_columns_len: libc::size_t,
    color_column_attr: VimSAttrT,
    tabstop: OptInt,
    variable_tabstops: *const VimColNrT,
) -> CLineTextRenderResult {
    let unchanged = CLineTextRenderResult::unchanged(col, first_vcol);
    let (Ok(start_col), Ok(max_col)) = (usize::try_from(col), usize::try_from(max_col)) else {
        return unchanged;
    };

    // SAFETY: The caller guarantees that `text` covers `text_len` bytes.
    let Some(text) = (unsafe { copy_c_slice(text.cast::<u8>(), text_len) }) else {
        return unchanged;
    };
    let explicit_vcols = if explicit_vcols.is_null() {
        if explicit_vcols_len != 0 {
            return unchanged;
        }
        None
    } else {
        // SAFETY: The caller guarantees that `explicit_vcols` covers its
        // supplied length.
        let Some(values) = (unsafe { copy_c_slice(explicit_vcols, explicit_vcols_len) }) else {
            return unchanged;
        };
        Some(values)
    };
    let color_columns = if color_columns.is_null() {
        if color_columns_len != 0 {
            return unchanged;
        }
        None
    } else {
        // SAFETY: The caller guarantees that `color_columns` covers its
        // supplied length.
        let Some(values) = (unsafe { copy_c_slice(color_columns, color_columns_len) }) else {
            return unchanged;
        };
        Some(values)
    };
    // SAFETY: The caller guarantees the counted variable-tab-stop allocation.
    let Some(tab_stops) = (unsafe { tab_stops_from_c(tabstop, variable_tabstops) }) else {
        return unchanged;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    let mut glyph_mapping = lock_glyph_mapping();
    let virtual_columns = if inc_vcol {
        LineTextVirtualColumns::Incrementing {
            color_columns: color_columns.as_deref(),
            color_column_highlight: Highlight(color_column_attr),
        }
    } else if let Some(explicit_vcols) = explicit_vcols.as_deref() {
        LineTextVirtualColumns::Provided(explicit_vcols)
    } else {
        LineTextVirtualColumns::Unavailable
    };
    let result = line_buffer.render_text(
        &text,
        &mut glyph_mapping,
        LineTextRenderOptions::new(
            start_col,
            max_col,
            Highlight(attr),
            first_vcol,
            virtual_columns,
            &tab_stops,
        ),
    );

    CLineTextRenderResult {
        end_col: libc::c_int::try_from(result.end_col).unwrap_or(col),
        end_vcol: result.end_virtual_column,
        color_cols_consumed: result.color_columns_consumed,
        color_cols_exhausted: result.color_columns_exhausted,
    }
}

/// Copies and renders one complete virtual-text item.
///
/// Invalid inputs leave the line buffer unchanged and return `col`.
///
/// # Safety
///
/// For a nonempty `virtual_text`, its item pointer must address every
/// initialized chunk. Each non-null chunk text pointer must address a readable
/// NUL-terminated string. A non-null `variable_tabstops` must address a
/// readable count followed by that many initialized widths. These allocations
/// must remain valid for this call and must not alias Rust-owned storage.
///
/// The global line buffer must be initialized. This function must run on
/// Neovim's main thread while the highlight tables are not otherwise accessed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn linebuf_render_virtual_text(
    col: libc::c_int,
    virtual_text: CVirtText,
    hl_mode: libc::c_int,
    max_col: libc::c_int,
    vcol: VimColNrT,
    skip_cells: libc::c_int,
    eol_hl: bool,
    tabstop: OptInt,
    variable_tabstops: *const VimColNrT,
) -> libc::c_int {
    let (Ok(start_col), Ok(max_col), Ok(skip_cells)) = (
        usize::try_from(col),
        usize::try_from(max_col),
        usize::try_from(skip_cells),
    ) else {
        return col;
    };

    // SAFETY: The caller guarantees that the vector describes initialized
    // virtual-text chunks.
    let Some(raw_chunks) = (unsafe { virtual_text.as_slice() }) else {
        return col;
    };
    let mut chunks = Vec::with_capacity(raw_chunks.len());
    let mut highlight = Highlight::default();
    for chunk in raw_chunks {
        if chunk.hl_id > 0 {
            highlight = highlight.combine(Highlight::from_group(chunk.hl_id));
        }
        if chunk.text.is_null() {
            continue;
        }

        // SAFETY: The caller guarantees that each non-null text pointer
        // addresses a readable NUL-terminated string.
        let text = unsafe { CStr::from_ptr(chunk.text) }.to_bytes().to_vec();
        chunks.push(VirtualTextChunk::new(text, highlight));
        highlight = Highlight::default();
    }
    let virtual_text = VirtualText::new(chunks);

    // SAFETY: The caller guarantees the counted variable-tab-stop allocation.
    let Some(tab_stops) = (unsafe { tab_stops_from_c(tabstop, variable_tabstops) }) else {
        return col;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    let mut glyph_mapping = lock_glyph_mapping();
    let end_col = virtual_text.render(
        &mut line_buffer,
        &mut glyph_mapping,
        VirtualTextRenderOptions::new(
            start_col,
            max_col,
            vcol,
            skip_cells,
            eol_hl,
            &tab_stops,
            VirtualTextHighlightMode::from(hl_mode),
        ),
    );

    libc::c_int::try_from(end_col).unwrap_or(col)
}

/// Updates one line-buffer cell and returns the following column.
///
/// `replace_char` controls whether `sc` replaces the current character. The
/// attribute and virtual column are always updated when the cell is valid.
/// A negative or out-of-bounds column leaves the buffer unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_update_cell(
    col: libc::c_int,
    sc: VimSCharT,
    replace_char: bool,
    attr: VimSAttrT,
    vcol: VimColNrT,
) -> libc::c_int {
    if col < 0 {
        return col;
    }
    let Some(next_col) = col.checked_add(1) else {
        return col;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    if line_buffer.update_cell(col as usize, replace_char.then_some(sc), attr, vcol) {
        next_col
    } else {
        col
    }
}

/// Fills a line-buffer range with one virtual column.
///
/// Negative positions, negative widths, integer overflow, and out-of-bounds
/// ranges leave the line buffer unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_fill_vcols(start_col: libc::c_int, width: libc::c_int, vcol: VimColNrT) {
    if start_col < 0 || width < 0 {
        return;
    }
    let Some(end_col) = start_col.checked_add(width) else {
        return;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.fill_vcols(start_col as usize..end_col as usize, vcol);
}

/// Fills a line-buffer range with consecutive virtual columns.
///
/// Negative positions, negative widths, integer overflow, and out-of-bounds
/// ranges leave the line buffer unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_fill_vcols_from(
    start_col: libc::c_int,
    width: libc::c_int,
    first_vcol: VimColNrT,
) {
    if start_col < 0 || width < 0 {
        return;
    }
    let Some(end_col) = start_col.checked_add(width) else {
        return;
    };

    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.fill_vcols_from(start_col as usize..end_col as usize, first_vcol);
}

/// Writes a two-cell character sequence without changing cell metadata.
///
/// A negative or out-of-bounds column leaves the line buffer unchanged.
#[unsafe(no_mangle)]
pub extern "C" fn linebuf_put_char_pair(col: libc::c_int, first: VimSCharT, second: VimSCharT) {
    let Ok(col) = usize::try_from(col) else {
        return;
    };
    let mut line_buffer = LineBufferGuard::acquire();
    line_buffer.put_chars(col, &[first, second]);
}

/// End a group of grid_line_puts calls and send the screen buffer to the UI layer.
#[unsafe(no_mangle)]
pub extern "C" fn grid_line_flush() {
    let output = {
        let mut line_buffer = LineBufferGuard::acquire();
        with_line_grid(|line_grid| line_grid.flush(&mut line_buffer))
    };
    if let Some((target, commands)) = output {
        target.emit_lines(commands);
    }
}

/// Mirrors a right-to-left message line and flushes it when its row is valid.
///
/// This is a stopgap until message.c has been refactored to behave
#[unsafe(no_mangle)]
pub extern "C" fn grid_flush_message_line(right_left: bool) {
    let output = {
        let mut line_buffer = LineBufferGuard::acquire();
        with_line_grid(|line_grid| line_grid.flush_message_line(right_left, &mut line_buffer))
    };
    if let Some((target, commands)) = output {
        target.emit_lines(commands);
    }
}

/// # Safety
///
/// `config` must be null or point to an initialized
/// `CGridLineSubmissionConfig`. The global line buffer must be initialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn grid_put_window_line(
    window_grid_handle: VimHandleT,
    config: *const CGridLineSubmissionConfig,
) {
    if config.is_null() {
        return;
    }

    let Ok(descriptor) = lock_grid_store().window_view_descriptor(GridId::new(window_grid_handle))
    else {
        return;
    };
    // SAFETY: The caller guarantees that `config` references an initialized
    // value for the duration of this call.
    let config = unsafe { &*config };
    let Some(row) = config.row.checked_add(descriptor.offset.row) else {
        return;
    };

    let (Ok(end_col), Ok(width)) = (
        usize::try_from(config.end_col),
        usize::try_from(config.width),
    ) else {
        return;
    };
    let smooth_scroll_marker = config.put_smoothscroll_marker.then(|| {
        SmoothScrollMarker::new(config.preserve_line_number, config.smoothscroll_marker_attr)
    });

    let (output_target, commands) = {
        let mut line_buffer = LineBufferGuard::acquire();
        let Some(submission) = LineSubmission::prepare(
            &mut line_buffer,
            end_col,
            width,
            config.clear_end,
            config.right_left,
            smooth_scroll_marker,
        ) else {
            return;
        };
        let (Ok(start_col), Ok(end_col), Ok(clear_width)) = (
            libc::c_int::try_from(submission.start_col()),
            libc::c_int::try_from(submission.end_col()),
            libc::c_int::try_from(submission.clear_width()),
        ) else {
            return;
        };
        let flags = if submission.is_right_left() {
            config.flags | Slf::Rightleft as libc::c_int
        } else {
            config.flags
        };

        let Ok(mut grid) = StoredGridGuard::acquire(descriptor.target) else {
            return;
        };
        let output_target = GridOutputTarget::new(grid.id(), grid.size().width);
        let commands = grid.put_line_buffer(
            row,
            descriptor.offset.column,
            start_col,
            end_col,
            clear_width,
            config.bg_attr,
            0,
            config.last_vcol,
            flags,
            &mut line_buffer,
        );
        (output_target, commands)
    };
    output_target.emit_lines(commands);
}
