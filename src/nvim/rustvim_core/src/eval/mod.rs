use crate::pos::{self};

/// Type used for script ID
pub type ScriptID = i32;

/// SCript ConteXt (SCTX): identifies a script line. When sourcing a script `line_num` is zero,
/// `sourcing_lnum` is the current line number. When executing a user function `line_num` is the
/// line where the function was defined, `sourcing_lnum` is the line number inside the function.
/// When stored with a function, mapping, option, etc. `line_num` is the line number in the script
/// `script_id`.
pub struct SCtx {
    /// script ID
    script_id: ScriptID,
    /// sourcing sequence number
    source_seq_num: i32,
    /// line number
    line_num: pos::LineNum,
    /// Only used when sc_sid is SID_API_CLIENT.
    chan: u64,
}

type CScriptID = libc::c_int;

#[repr(C)]
pub struct CSCtx {
    sc_sid: CScriptID,
    sc_seq: libc::c_int,
    sc_lnum: pos::VimLineNumT,
    sc_chan: u64,
}
