use std::error::Error;

use crate::{
    defines::{self},
    eval,
};

pub struct OptFlags;

impl OptFlags {
    /// Environment expansion.
    /// NOTE: Expand can never be used for local or hidden options.
    const EXPAND: u32 = 1 << 0;
    /// Don't expand default value.
    const NO_DEF_EXP: u32 = 1 << 1;
    /// Don't set to default value.
    const NO_DEFAULT: u32 = 1 << 2;
    /// Option has been set/reset.
    const WAS_SET: u32 = 1 << 3;
    /// Don't include in :mkvimrc output.
    const NO_MKRC: u32 = 1 << 4;
    /// Send option to remote UI.
    const UIOPTION: u32 = 1 << 5;
    /// Redraw tabline.
    const REDR_TABL: u32 = 1 << 6;
    /// Redraw status lines.
    const REDR_STAT: u32 = 1 << 7;
    /// Redraw current window and recompute text.
    const REDR_WIN: u32 = 1 << 8;
    /// Redraw current buffer and recompute text.
    const REDR_BUF: u32 = 1 << 9;
    /// Redraw all windows and recompute text.
    const REDR_ALL: u32 = Self::REDR_BUF | Self::REDR_WIN;
    /// Clear and redraw all and recompute text.
    const REDR_CLEAR: u32 = Self::REDR_ALL | Self::REDR_STAT;
    /// Comma-separated list.
    const COMMA: u32 = 1 << 10;
    /// Comma-separated list that cannot have two consecutive commas.
    const ONE_COMMA: u32 = (1 << 11) | Self::COMMA;
    /// Don't allow duplicate strings.
    const NO_DUP: u32 = 1 << 12;
    /// List of single-char flags.
    const FLAG_LIST: u32 = 1 << 13;
    /// Cannot change in modeline or secure mode.
    const SECURE: u32 = 1 << 14;
    /// Expand default value with _().
    const GETTEXT: u32 = 1 << 15;
    /// Do not use local value for global vimrc.
    const NO_GLOB: u32 = 1 << 16;
    /// Only normal file name chars allowed.
    const NFNAME: u32 = 1 << 17;
    /// Option was set from a modeline.
    const INSECURE: u32 = 1 << 18;
    /// Priority for :mkvimrc (setting option has side effects).
    const PRI_MKRC: u32 = 1 << 19;
    /// Not allowed in modeline.
    const NO_ML: u32 = 1 << 20;
    /// Update curswant required; not needed when there is a redraw flag.
    const CURSWANT: u32 = 1 << 21;
    /// Only normal directory name chars allowed.
    const NDNAME: u32 = 1 << 22;
    /// Option only changes highlight; not text.
    const HLONLY: u32 = 1 << 23;
    /// Under control of 'modelineexpr'.
    const MLE: u32 = 1 << 24;
    /// Accept a function reference or a lambda.
    const FUNC: u32 = 1 << 25;
    /// Values use colons to create sublists.
    const COLON: u32 = 1 << 26;
}

/// Option value type.
/// These types are also used as type flags by using the type value as an index for the type_flags
/// bit field (@see option_has_type()).
pub enum OptValType {
    // Make sure Nil can't be bitshifted and used as an option type flag.
    Nil = -1,
    Boolean,
    Number,
    String,
}

/// Scopes that an option can support.
pub enum OptScope {
    /// Request global option value
    Global,
    /// Request window-local option value
    Win,
    /// Request buffer-local option value
    Buf,
}

// Always update this whenever a new option scope is added.
static OPT_SCOPE_SIZE: usize = 3;

type OptScopeFlags = u8;

pub union OptValData1 {
    // boolean options are actually tri-states because they have a third "None" value.
    boolean: defines::TriState,
    number: defines::OptInt,
    string: defines::VimString,
}

pub enum OptValData {
    // boolean options are actually tri-states because they have a third "None" value.
    Boolean(defines::TriState),
    Number(defines::OptInt),
    String(&'static [&'static i8]),
}

/// Option value
pub struct OptVal {
    opt_type: OptValType,
    opt_data: OptValData,
}

/// :set operator types
pub enum SetOp {
    None = 0,
    /// "opt+=arg"
    Adding,
    /// "opt^=arg"
    Prepending,
    /// "opt-=arg"
    Removing,
}

/// Argument for the callback function (`OptDidSetCb`) invoked after an
/// option value is modified.
pub struct OptSet {}

/// Type for the callback function that is invoked after an option value is
/// changed to validate and apply the new value.
///
/// Returns Ok(()) if the option value is valid and successfully applied.
/// Otherwise returns an error message.
pub type OptDidSetCb = fn(opt_set: &mut OptSet) -> Result<(), Box<dyn Error>>;

/// Argument for the callback function (opt_expand_cb_T) invoked after a string
/// option value is expanded for cmdline completion.
pub struct OptExpand {}

pub type OptExpandCb =
    fn(opt_expand: &mut OptExpand, matches: &mut Vec<String>) -> Result<(), Box<dyn Error>>;

pub struct VimOption {
    /// full option name
    full_name: String,
    /// permissible abbreviation
    short_name: String,
    /// see above
    flags: u32,
    /// option type
    opt_type: OptValType,
    /// option scope flags, see OptScope
    scope_flags: OptScopeFlags,
    /// global option: pointer to variable;
    /// window-local option: NULL;
    /// buffer-local option: global value
    var: *mut libc::c_void,

    flags_var: *mut libc::c_uint,
    /// index of option at every scope.
    scope_idx: [i32; OPT_SCOPE_SIZE],
    /// option is immutable, trying to set it will give an error.
    immutable: bool,
    /// possible values for string options
    values: &'static [String],
    /// callback function to invoke after an option is modified to validate and
    /// apply the new value.
    opt_did_set_cb: OptDidSetCb,
    /// callback function to invoke when expanding possible values on the
    /// cmdline. Only useful for string options.
    opt_expand_cb: OptExpandCb,

    /// default value
    def_val: OptVal,
    ///< script in which the option was last set
    script_ctx: eval::SCtx,
}
