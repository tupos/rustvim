//! Minimal unsafe boundary to Neovim's existing logger.

use std::ffi::CString;
use std::ffi::c_char;
use std::ffi::c_int;
use std::ptr;

use tracing::Level;
use tracing::Metadata;

/// Native log levels accepted by Neovim's C logger.
#[repr(i32)]
enum NvimLogLevel {
    /// Debug and trace diagnostics.
    Debug = 1,
    /// Informational diagnostics.
    Info = 2,
    /// Warning diagnostics.
    Warn = 3,
    /// Error diagnostics.
    Error = 4,
}

impl NvimLogLevel {
    /// Converts a tracing level into Neovim's native severity.
    fn from_tracing(level: &Level) -> Self {
        if *level == Level::ERROR {
            Self::Error
        } else if *level == Level::WARN {
            Self::Warn
        } else if *level == Level::INFO {
            Self::Info
        } else {
            Self::Debug
        }
    }

    /// Returns the integer representation accepted by C.
    const fn into_native(self) -> c_int {
        self as c_int
    }
}

unsafe extern "C" {
    /// Reports whether Neovim currently accepts this native log level.
    fn rust_log_level_enabled(log_level: c_int) -> bool;

    /// Sends one fully formatted event to Neovim's logger.
    fn rust_log_event(
        log_level: c_int,
        context: *const c_char,
        func_name: *const c_char,
        line_num: c_int,
        message: *const c_char,
    ) -> bool;
}

/// Reports whether an event at `level` should be formatted.
pub(super) fn enabled(level: &Level) -> bool {
    // SAFETY: The function accepts one scalar and does not retain state.
    unsafe { rust_log_level_enabled(NvimLogLevel::from_tracing(level).into_native()) }
}

/// Emits one formatted event with its static tracing metadata.
pub(super) fn emit(metadata: &Metadata<'_>, message: &str) {
    let context = c_string(&format!("{}: ", metadata.target()));
    let source = metadata.file().or(metadata.module_path()).map(c_string);
    let source_pointer = source
        .as_ref()
        .map_or(ptr::null(), |source| source.as_ptr());
    let line_number = metadata
        .line()
        .and_then(|line| c_int::try_from(line).ok())
        .unwrap_or(-1);
    let message = c_string(message);

    // SAFETY: Every non-null pointer is supplied by a live `CString` for this
    // synchronous call. Native severity and line values were validated above.
    let _ = unsafe {
        rust_log_event(
            NvimLogLevel::from_tracing(metadata.level()).into_native(),
            context.as_ptr(),
            source_pointer,
            line_number,
            message.as_ptr(),
        )
    };
}

/// Produces a NUL-terminated string while preserving embedded NULs visibly.
fn c_string(value: &str) -> CString {
    CString::new(value.replace('\0', "\\0"))
        .expect("replacing embedded NULs must produce a valid C string")
}
