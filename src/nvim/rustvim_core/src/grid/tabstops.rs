use super::VimColNrT;
use crate::defines::OptInt;

const DEFAULT_TABSTOP_WIDTH: OptInt = 8;

/// Owned tab-stop configuration used while rendering screen text.
///
/// A fixed configuration repeats one width indefinitely. A variable
/// configuration uses each configured width once and then repeats its final
/// width, matching the behavior of Neovim's `'vartabstop'` option.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TabStops(TabStopKind);

#[derive(Clone, Debug, Eq, PartialEq)]
enum TabStopKind {
    Fixed(VimColNrT),
    Variable(Box<[VimColNrT]>),
}

/// An invalid fixed or variable tab-stop width.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TabStopsError {
    InvalidFixedWidth(OptInt),
    InvalidVariableWidth { index: usize, width: VimColNrT },
}

impl TabStops {
    /// Creates a validated tab-stop configuration from Neovim option values.
    ///
    /// An empty variable-width slice selects `fixed_width`. As in
    /// `tabstop_padding()`, a zero fixed width selects the default width of
    /// eight.
    pub(super) fn try_from_options(
        fixed_width: OptInt,
        variable_widths: &[VimColNrT],
    ) -> Result<Self, TabStopsError> {
        if !variable_widths.is_empty() {
            for (index, &width) in variable_widths.iter().enumerate() {
                if width <= 0 {
                    return Err(TabStopsError::InvalidVariableWidth { index, width });
                }
            }
            return Ok(Self(TabStopKind::Variable(
                variable_widths.to_vec().into_boxed_slice(),
            )));
        }

        let effective_width = if fixed_width == 0 {
            DEFAULT_TABSTOP_WIDTH
        } else {
            fixed_width
        };
        let width = VimColNrT::try_from(effective_width)
            .ok()
            .filter(|&width| width > 0)
            .ok_or(TabStopsError::InvalidFixedWidth(fixed_width))?;

        Ok(Self(TabStopKind::Fixed(width)))
    }

    /// Returns the number of cells from `column` to the next tab stop.
    pub(super) fn padding_at(&self, column: VimColNrT) -> usize {
        let column = i64::from(column);
        let padding = match &self.0 {
            TabStopKind::Fixed(width) => {
                let width = i64::from(*width);
                width - column % width
            }
            TabStopKind::Variable(widths) => {
                let mut tab_column = 0_i64;
                for &width in widths {
                    tab_column += i64::from(width);
                    if tab_column > column {
                        let padding = tab_column - column;
                        return usize::try_from(padding)
                            .expect("tab-stop padding must fit in usize");
                    }
                }

                let repeating_width =
                    i64::from(*widths.last().expect("variable tab stops are nonempty"));
                repeating_width - (column - tab_column) % repeating_width
            }
        };

        usize::try_from(padding).expect("tab-stop padding must fit in usize")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_tab_stops_repeat() {
        let tab_stops = TabStops::try_from_options(4, &[]).unwrap();

        assert_eq!(tab_stops.padding_at(0), 4);
        assert_eq!(tab_stops.padding_at(1), 3);
        assert_eq!(tab_stops.padding_at(3), 1);
        assert_eq!(tab_stops.padding_at(4), 4);
        assert_eq!(tab_stops.padding_at(5), 3);
    }

    #[test]
    fn zero_fixed_width_uses_default() {
        let tab_stops = TabStops::try_from_options(0, &[]).unwrap();

        assert_eq!(tab_stops.padding_at(0), 8);
        assert_eq!(tab_stops.padding_at(7), 1);
        assert_eq!(tab_stops.padding_at(8), 8);
    }

    #[test]
    fn variable_tab_stops_repeat_the_final_width() {
        let tab_stops = TabStops::try_from_options(8, &[2, 4, 3]).unwrap();

        assert_eq!(tab_stops.padding_at(0), 2);
        assert_eq!(tab_stops.padding_at(1), 1);
        assert_eq!(tab_stops.padding_at(2), 4);
        assert_eq!(tab_stops.padding_at(5), 1);
        assert_eq!(tab_stops.padding_at(6), 3);
        assert_eq!(tab_stops.padding_at(8), 1);
        assert_eq!(tab_stops.padding_at(9), 3);
        assert_eq!(tab_stops.padding_at(10), 2);
    }

    #[test]
    fn padding_preserves_negative_column_behavior() {
        assert_eq!(
            TabStops::try_from_options(4, &[]).unwrap().padding_at(-1),
            5
        );
        assert_eq!(
            TabStops::try_from_options(8, &[2, 4])
                .unwrap()
                .padding_at(-1),
            3
        );
    }

    #[test]
    fn invalid_effective_widths_are_reported() {
        assert_eq!(
            TabStops::try_from_options(-1, &[]),
            Err(TabStopsError::InvalidFixedWidth(-1))
        );
        let too_wide = i64::from(VimColNrT::MAX) + 1;
        assert_eq!(
            TabStops::try_from_options(too_wide, &[]),
            Err(TabStopsError::InvalidFixedWidth(too_wide))
        );
        assert_eq!(
            TabStops::try_from_options(8, &[2, 0, 4]),
            Err(TabStopsError::InvalidVariableWidth { index: 1, width: 0 })
        );
        assert_eq!(
            TabStops::try_from_options(8, &[2, -1, 4]),
            Err(TabStopsError::InvalidVariableWidth {
                index: 1,
                width: -1,
            })
        );
    }

    #[test]
    fn variable_widths_do_not_depend_on_the_fixed_width() {
        let tab_stops = TabStops::try_from_options(-1, &[2]).unwrap();

        assert_eq!(tab_stops.padding_at(2), 2);
    }
}
