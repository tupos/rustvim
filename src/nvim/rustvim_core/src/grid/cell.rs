use super::{VimColNrT, VimSAttrT, VimSCharT};

/// Complete contents of one rendered screen cell.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GridCell {
    pub(super) character: VimSCharT,
    pub(super) attribute: VimSAttrT,
    pub(super) virtual_column: VimColNrT,
}

impl GridCell {
    /// Creates a cell containing its character and rendering metadata.
    pub(crate) const fn new(
        character: VimSCharT,
        attribute: VimSAttrT,
        virtual_column: VimColNrT,
    ) -> Self {
        Self {
            character,
            attribute,
            virtual_column,
        }
    }
}
