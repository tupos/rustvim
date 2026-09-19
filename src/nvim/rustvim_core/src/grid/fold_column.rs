use super::{VimColNrT, VimSCharT, schar_from_ascii};

const NO_VIRTUAL_COLUMN: VimColNrT = -1;
const CLOSED_FOLD_VIRTUAL_COLUMN: VimColNrT = -2;
const FOLD_COLUMN_VIRTUAL_COLUMN: VimColNrT = -3;

/// Fold state needed to render one fold column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FoldColumnInfo {
    level: usize,
    lowest_level: usize,
    starts_here: bool,
    is_closed: bool,
}

impl FoldColumnInfo {
    /// Creates fold-column state for one buffer line.
    ///
    /// A zero fold level always represents a line outside a fold and therefore
    /// cannot be closed.
    pub(super) fn new(
        level: usize,
        lowest_level: usize,
        starts_here: bool,
        is_closed: bool,
    ) -> Self {
        Self {
            level,
            lowest_level,
            starts_here,
            is_closed: level != 0 && is_closed,
        }
    }
}

/// Characters configured for the semantic parts of a fold column.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FoldColumnSymbols {
    open: VimSCharT,
    closed: VimSCharT,
    separator: VimSCharT,
    inner: Option<VimSCharT>,
}

impl FoldColumnSymbols {
    /// Creates a fold-column symbol set.
    ///
    /// `inner` being `None` selects numeric depth indicators, followed by `>`
    /// when the displayed depth exceeds nine.
    pub(super) fn new(
        open: VimSCharT,
        closed: VimSCharT,
        separator: VimSCharT,
        inner: Option<VimSCharT>,
    ) -> Self {
        Self {
            open,
            closed,
            separator,
            inner,
        }
    }
}

/// One generated fold-column cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct FoldColumnCell {
    character: VimSCharT,
    virtual_column: VimColNrT,
}

impl FoldColumnCell {
    /// Returns the screen character displayed in this cell.
    pub(super) fn character(self) -> VimSCharT {
        self.character
    }

    /// Returns the semantic virtual-column marker for this cell.
    pub(super) fn virtual_column(self) -> VimColNrT {
        self.virtual_column
    }
}

/// A completely generated fold column.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FoldColumn {
    cells: Box<[FoldColumnCell]>,
}

impl FoldColumn {
    /// Generates a fold column with exactly `width` cells.
    ///
    /// Virtual filler lines reuse only the outer fold levels so fold-open and
    /// fold-closed markers are not displayed twice.
    pub(super) fn render(
        width: usize,
        info: FoldColumnInfo,
        symbols: FoldColumnSymbols,
        virtual_line: bool,
    ) -> Self {
        let first_level = info
            .level
            .saturating_add(1)
            .saturating_sub(width)
            .saturating_sub(usize::from(info.is_closed))
            .max(1);
        let closed_column = width.min(info.level);
        let cells = (0..width)
            .map(|index| {
                let mut character = if index >= info.level {
                    schar_from_ascii(' ')
                } else if info.is_closed && index + 1 == closed_column {
                    symbols.closed
                } else if info.starts_here && first_level.saturating_add(index) >= info.lowest_level
                {
                    symbols.open
                } else {
                    Self::separator_character(first_level, index, symbols)
                };

                if virtual_line && info.level != 0 && info.starts_here {
                    let outer_level = info.lowest_level.saturating_sub(1);
                    let outer_first_level =
                        outer_level.saturating_add(1).saturating_sub(width).max(1);
                    character = if index >= outer_level {
                        schar_from_ascii(' ')
                    } else {
                        Self::separator_character(outer_first_level, index, symbols)
                    };
                }

                let virtual_column = if index >= info.level {
                    NO_VIRTUAL_COLUMN
                } else if info.is_closed && index + 1 == closed_column {
                    CLOSED_FOLD_VIRTUAL_COLUMN
                } else {
                    FOLD_COLUMN_VIRTUAL_COLUMN
                };
                FoldColumnCell {
                    character,
                    virtual_column,
                }
            })
            .collect();

        Self { cells }
    }

    /// Returns the generated cells in display order.
    pub(super) fn cells(&self) -> &[FoldColumnCell] {
        &self.cells
    }

    fn separator_character(
        first_level: usize,
        index: usize,
        symbols: FoldColumnSymbols,
    ) -> VimSCharT {
        if first_level == 1 {
            symbols.separator
        } else if let Some(inner) = symbols.inner {
            inner
        } else {
            let displayed_level = first_level.saturating_add(index);
            if displayed_level <= 9 {
                schar_from_ascii(char::from(b'0' + displayed_level as u8))
            } else {
                schar_from_ascii('>')
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbols(inner: Option<char>) -> FoldColumnSymbols {
        FoldColumnSymbols::new(
            schar_from_ascii('-'),
            schar_from_ascii('+'),
            schar_from_ascii('|'),
            inner.map(schar_from_ascii),
        )
    }

    fn characters(column: &FoldColumn) -> Vec<VimSCharT> {
        column.cells().iter().map(|cell| cell.character()).collect()
    }

    fn virtual_columns(column: &FoldColumn) -> Vec<VimColNrT> {
        column
            .cells()
            .iter()
            .map(|cell| cell.virtual_column())
            .collect()
    }

    #[test]
    fn fills_columns_outside_folds_with_spaces() {
        let column = FoldColumn::render(
            3,
            FoldColumnInfo::new(0, 0, false, false),
            symbols(None),
            false,
        );

        assert_eq!(characters(&column), [schar_from_ascii(' '); 3]);
        assert_eq!(virtual_columns(&column), [NO_VIRTUAL_COLUMN; 3]);
    }

    #[test]
    fn marks_levels_opened_on_the_current_line() {
        let column = FoldColumn::render(
            4,
            FoldColumnInfo::new(3, 2, true, false),
            symbols(None),
            false,
        );

        assert_eq!(
            characters(&column),
            [
                schar_from_ascii('|'),
                schar_from_ascii('-'),
                schar_from_ascii('-'),
                schar_from_ascii(' '),
            ]
        );
        assert_eq!(
            virtual_columns(&column),
            [
                FOLD_COLUMN_VIRTUAL_COLUMN,
                FOLD_COLUMN_VIRTUAL_COLUMN,
                FOLD_COLUMN_VIRTUAL_COLUMN,
                NO_VIRTUAL_COLUMN,
            ]
        );
    }

    #[test]
    fn marks_a_closed_fold_at_the_deepest_visible_level() {
        let column = FoldColumn::render(
            2,
            FoldColumnInfo::new(4, 4, false, true),
            symbols(None),
            false,
        );

        assert_eq!(
            characters(&column),
            [schar_from_ascii('2'), schar_from_ascii('+')]
        );
        assert_eq!(
            virtual_columns(&column),
            [FOLD_COLUMN_VIRTUAL_COLUMN, CLOSED_FOLD_VIRTUAL_COLUMN]
        );
    }

    #[test]
    fn uses_inner_and_overflow_symbols_for_hidden_levels() {
        let inner_column = FoldColumn::render(
            2,
            FoldColumnInfo::new(4, 4, false, false),
            symbols(Some(':')),
            false,
        );
        let overflow_column = FoldColumn::render(
            2,
            FoldColumnInfo::new(12, 12, false, false),
            symbols(None),
            false,
        );

        assert_eq!(characters(&inner_column), [schar_from_ascii(':'); 2]);
        assert_eq!(characters(&overflow_column), [schar_from_ascii('>'); 2]);
    }

    #[test]
    fn virtual_lines_show_only_outer_fold_levels() {
        let column = FoldColumn::render(
            3,
            FoldColumnInfo::new(3, 2, true, true),
            symbols(None),
            true,
        );

        assert_eq!(
            characters(&column),
            [
                schar_from_ascii('|'),
                schar_from_ascii(' '),
                schar_from_ascii(' '),
            ]
        );
        assert_eq!(
            virtual_columns(&column),
            [
                FOLD_COLUMN_VIRTUAL_COLUMN,
                FOLD_COLUMN_VIRTUAL_COLUMN,
                CLOSED_FOLD_VIRTUAL_COLUMN,
            ]
        );
    }
}
