//! Native adapters borrowing the safe logger owned by the tracing subscriber.

use std::borrow::Cow;
use std::ffi::CStr;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::mem::MaybeUninit;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use tracing::Event;
use tracing::Level;

use crate::buffer::LogBuffer;
use crate::layer;
use crate::logger::LogGuard;
use crate::logger::LogMessage;
use crate::logger::LogMetadata;
use crate::logger::LogOutput;
use crate::logger::Logger;
use crate::logger::Rejected;

/// Native VimVarIndex value for v:servername from eval_defs.h.
const VV_SEND_SERVER: libc::c_int = 28;

/// Layout of Neovim's native statistics, used only to increment log_skip.
#[repr(C)]
struct NativeStats {
    /// Native fsync counter, preceding the logging statistic.
    fsync: i64,
    /// Native redraw counter, preceding the logging statistic.
    redraw: i64,
    /// Attempts skipped before initialization or due to recursion.
    log_skip: i16,
}

unsafe extern "C" {
    /// Native severity threshold, including temporary Lua callback adjustments.
    static mut g_min_log_level: libc::c_int;
    /// Native statistics storage, initialized before Rust startup.
    static mut g_stats: NativeStats;
    /// Nonzero when this process acts as a remote UI client.
    static mut ui_client_channel_id: u64;

    /// Releases storage returned by Neovim's native allocation functions.
    fn xfree(pointer: *mut libc::c_void);
    /// Updates a native environment variable during single-threaded initialization.
    fn os_setenv(
        name: *const libc::c_char,
        value: *const libc::c_char,
        overwrite: bool,
    ) -> libc::c_int;
    /// Returns an allocated environment value, or NULL when unset or empty.
    fn os_getenv(name: *const libc::c_char) -> *mut libc::c_char;
    /// Finds the platform-specific tail of a native path, accepting NULL.
    fn path_tail(path: *const libc::c_char) -> *const libc::c_char;
    /// Borrows the current string value of a native v: variable.
    fn get_vim_var_str(index: libc::c_int) -> *const libc::c_char;
    /// Fills local calendar time, returning NULL if conversion fails.
    fn os_localtime(time: *mut libc::tm) -> *mut libc::tm;
    /// Schedules an existing native error message without logging it recursively.
    fn msg_schedule_semsg(format: *const libc::c_char, ...);
    /// Prints libuv's ASCII handle diagnostic to a valid native stream.
    fn uv_print_all_handles(loop_pointer: *mut libc::c_void, output: *mut libc::FILE);
}

/// Reads Neovim's current threshold, including temporary Lua callback adjustments.
fn minimum() -> i32 {
    // SAFETY: Neovim owns this process-lifetime variable and its native updates.
    unsafe { g_min_log_level }
}

/// Filters tracing fields before they are evaluated or formatted.
pub(super) fn enabled(level: &Level) -> bool {
    Logger::level_number(*level) >= minimum()
}

/// Accounts for native attempts skipped before initialization or due to recursion.
fn skipped() {
    // SAFETY: This mirrors the native counter update in process-lifetime g_stats.
    unsafe { g_stats.log_skip = g_stats.log_skip.wrapping_add(1) };
}

/// Reserves logging before invoking native formatting or resolving runtime metadata.
fn reserve<'a>(
    logger: &'a Logger,
    level: Option<Level>,
    function: Option<&str>,
    context: Option<&str>,
    line: i32,
) -> Option<LogGuard<'a>> {
    let result = match level {
        Some(level) => logger.prepare(level, minimum()),
        None => logger.prepare_unfiltered(),
    };
    match result {
        Ok(guard) => Some(guard),
        Err(Rejected::Filtered) => None,
        Err(Rejected::Recursive { notify }) => {
            skipped();
            if notify {
                let message = format!(
                    "E5430: {}:{line}: recursive log!",
                    function.or(context).unwrap_or("(null)")
                );
                let message = CString::new(message.replace('\0', "\\0")).unwrap();
                // SAFETY: The native scheduler copies this live string synchronously.
                unsafe { msg_schedule_semsg(c"%s".as_ptr(), message.as_ptr()) };
            }
            None
        }
    }
}

/// Copies optional C text to UTF-8 for a synchronous boundary call.
///
/// # Safety
///
/// A non-null pointer must remain a readable NUL-terminated string for 'a.
unsafe fn text<'a>(pointer: *const libc::c_char) -> Option<Cow<'a, str>> {
    if pointer.is_null() {
        None
    } else {
        // SAFETY: The caller supplies the readable string lifetime.
        Some(unsafe { CStr::from_ptr(pointer) }.to_string_lossy())
    }
}

/// Publishes the selected native path during startup without changing encoding.
fn set_path(name: &CStr, path: &Path) {
    let value =
        CString::new(path.as_os_str().as_encoded_bytes()).expect("native paths cannot contain NUL");
    // SAFETY: Startup owns environment mutation; neither string is retained.
    unsafe { os_setenv(name.as_ptr(), value.as_ptr(), true) };
}

/// Resolves native context after reserving logging, then produces owned UTF-8.
fn metadata() -> Option<LogMetadata> {
    let mut calendar = MaybeUninit::<libc::tm>::uninit();
    // SAFETY: os_localtime initializes this correctly sized native tm on success.
    if unsafe { os_localtime(calendar.as_mut_ptr()) }.is_null() {
        return None;
    }
    // SAFETY: The non-null return above confirms calendar was initialized.
    let calendar = unsafe { calendar.assume_init() };
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_millis();
    let timestamp = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{millis:03}",
        calendar.tm_year + 1900,
        calendar.tm_mon + 1,
        calendar.tm_mday,
        calendar.tm_hour,
        calendar.tm_min,
        calendar.tm_sec
    );

    // SAFETY: These native helpers return live strings or NULL. The parent
    // allocation is owned here; the v:servername value remains native-owned.
    let (parent, server, ui) = unsafe {
        let parent = os_getenv(c"NVIM".as_ptr());
        let parent_name = text(path_tail(parent)).unwrap().into_owned();
        xfree(parent.cast());
        let server_name = text(path_tail(get_vim_var_str(VV_SEND_SERVER)))
            .unwrap()
            .into_owned();
        (parent_name, server_name, ui_client_channel_id != 0)
    };
    Some(LogMetadata {
        timestamp,
        ui,
        parent,
        server,
        pid: std::process::id(),
    })
}

/// Writes a tracing event directly, reserving logging before Debug fields run.
pub(super) fn emit(logger: &Logger, event: &Event<'_>) {
    let trace = event.metadata();
    let context = format!("{}: ", trace.target());
    let source = trace
        .file()
        .or(trace.module_path())
        .zip(trace.line().and_then(|line| i32::try_from(line).ok()));
    let Some(guard) = reserve(
        logger,
        Some(*trace.level()),
        source.map(|source| source.0),
        Some(&context),
        source.map_or(-1, |source| source.1),
    ) else {
        return;
    };
    let message = layer::format_event(event);
    let message = if message.contains('\0') {
        Cow::Owned(message.replace('\0', "\\0"))
    } else {
        Cow::Borrowed(message.as_str())
    };
    if let Some(metadata) = metadata() {
        let _ = guard.write(
            &metadata,
            &LogMessage {
                level: *trace.level(),
                context: Some(&context),
                source,
                message: &message,
                newline: true,
            },
        );
    }
}

/// Initializes the logger and installs its tracing subscriber during native startup.
///
/// Native environment and path initialization must already have completed.
#[unsafe(no_mangle)]
pub extern "C" fn log_init() {
    let requested = std::env::var_os("NVIM_LOG_FILE").map(PathBuf::from);
    let home = rust_os::env::cached_home_directory();
    let default =
        rust_os::stdpaths::user_state_subpath(home.as_deref(), Path::new("logs/nvim.log"), 0, true);
    let logger = Logger::initialize(requested, default);
    if let Some(path) = logger.wanted_path() {
        set_path(c"__NVIM_LOG_FILE_WANT", path);
    }
    if let Some(path) = logger.environment_path() {
        set_path(c"NVIM_LOG_FILE", path);
    }
    if let Some(error) = logger.directory_error() {
        if let Some(guard) = reserve(&logger, Some(Level::WARN), Some("log_init"), None, -1) {
            if let Some(metadata) = metadata() {
                let _ = guard.write(
                    &metadata,
                    &LogMessage {
                        level: Level::WARN,
                        context: None,
                        source: Some(("log_init", line!() as i32)),
                        message: &format!("Failed to create log directory: {error}"),
                        newline: true,
                    },
                );
            }
        }
    }
    crate::initialize(logger).expect("log_init must install the logging subscriber exactly once");
}

/// Formats one native message synchronously in reusable Rust-owned storage.
///
/// # Safety
///
/// context and function must be NULL or readable NUL-terminated strings.
/// format must be readable and NUL-terminated. arguments must remain live for
/// the whole call. formatter must follow vsnprintf's return convention, write
/// only within the supplied slice, terminate completed output with NUL, and
/// neither retain the borrowed pointers nor unwind across the C boundary.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustvim_logmsg(
    native_level: libc::c_int,
    context: *const libc::c_char,
    function: *const libc::c_char,
    line: libc::c_int,
    newline: bool,
    fatal: bool,
    format: *const libc::c_char,
    arguments: *mut libc::c_void,
    formatter: Option<
        unsafe extern "C" fn(
            *mut libc::c_char,
            usize,
            *const libc::c_char,
            *mut libc::c_void,
        ) -> libc::c_int,
    >,
) -> bool {
    let level = match native_level {
        1 => Level::DEBUG,
        2 => Level::INFO,
        3 => Level::WARN,
        4 => Level::ERROR,
        _ => return false,
    };
    let Some(formatter) = formatter else {
        return false;
    };
    if format.is_null() || arguments.is_null() {
        return false;
    }
    layer::with_logger(|logger| {
        if !enabled(&level) {
            return false;
        }
        // SAFETY: The caller keeps both optional strings readable throughout this call.
        let (context, function) = unsafe { (text(context), text(function)) };
        let format_message = |output: &mut [u8]| {
            // SAFETY: The callback receives an exclusively borrowed initialized slice.
            // Its other arguments remain owned by the enclosing synchronous C call.
            usize::try_from(unsafe {
                formatter(output.as_mut_ptr().cast(), output.len(), format, arguments)
            })
            .ok()
        };
        if fatal {
            let mut buffer = LogBuffer::new();
            let _ = buffer.format(format_message, |message| {
                Logger::fatal(
                    context.as_deref(),
                    function.as_deref(),
                    line,
                    message,
                    newline,
                )
            });
            Logger::fatal(
                context.as_deref(),
                function.as_deref(),
                line,
                "failed to format fatal log message",
                newline,
            );
        }
        let Some(guard) = reserve(
            logger,
            Some(level),
            function.as_deref(),
            context.as_deref(),
            line,
        ) else {
            return false;
        };
        let Some(metadata) = metadata() else {
            return false;
        };
        let source = function
            .as_deref()
            .filter(|_| line != -1)
            .map(|name| (name, line));
        guard
            .format_message(format_message, |message| {
                guard
                    .write(
                        &metadata,
                        &LogMessage {
                            level,
                            context: context.as_deref(),
                            source,
                            message,
                            newline,
                        },
                    )
                    .is_ok()
            })
            .unwrap_or(false)
    })
    .unwrap_or_else(|| {
        skipped();
        false
    })
}

/// Transfers a duplicate file handle to the native FILE API required by libuv.
///
/// The caller must close the returned stream with fclose.
fn native_file(file: &File) -> io::Result<*mut libc::FILE> {
    let copy = file.try_clone()?;
    #[cfg(unix)]
    let descriptor = {
        use std::os::fd::IntoRawFd;
        copy.into_raw_fd()
    };
    #[cfg(windows)]
    let descriptor = {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::io::IntoRawHandle;
        // SAFETY: The cloned handle is valid and transfers to the CRT on success.
        let descriptor =
            unsafe { libc::open_osfhandle(copy.as_raw_handle() as libc::intptr_t, libc::O_APPEND) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        let _ = copy.into_raw_handle();
        descriptor
    };
    // SAFETY: descriptor is an owned duplicate of an append-mode file.
    let stream = unsafe { libc::fdopen(descriptor, c"a".as_ptr()) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopen failed without consuming descriptor.
        unsafe { libc::close(descriptor) };
        return Err(error);
    }
    Ok(stream)
}

/// Writes libuv's native diagnostic while the safe logger holds its output lock.
///
/// # Safety
///
/// loop_pointer must be NULL or a live uv_loop_t. standard_error must be the
/// live native stderr stream, writable for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustvim_log_uv_handles(
    loop_pointer: *mut libc::c_void,
    standard_error: *mut libc::FILE,
) {
    if standard_error.is_null() {
        return;
    }
    let _ = layer::with_logger(|logger| {
        let Some(guard) = reserve(logger, None, Some("log_uv_handles"), None, -1) else {
            return;
        };
        let Some(metadata) = metadata() else { return };
        let _ = guard.with_output(&metadata, |output| {
            let (stream, close) = match output {
                LogOutput::File(file) => (native_file(file)?, true),
                LogOutput::Stderr(_) => (standard_error, false),
            };
            // SAFETY: The loop is supplied by C and the stream is live. libuv
            // prints ASCII flags, fixed handle names, and pointer addresses.
            unsafe { uv_print_all_handles(loop_pointer, stream) };
            // SAFETY: The duplicate is owned here; native stderr is only flushed.
            let result = unsafe {
                if close {
                    libc::fclose(stream)
                } else {
                    libc::fflush(stream)
                }
            };
            if result == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    });
}

/// Captures a mixed C/Rust stack and writes it through the existing safe backend.
///
/// # Safety
///
/// function must be NULL or a readable NUL-terminated string. A non-null output
/// must be exclusively writable for this call and transfers ownership when
/// close_output is true. NULL output selects the logger's normal destination.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rustvim_log_callstack(
    output: *mut libc::FILE,
    close_output: bool,
    function: *const libc::c_char,
    line: libc::c_int,
) {
    let _ = layer::with_logger(|logger| {
        // SAFETY: The caller guarantees the optional name is readable throughout this call.
        let function = unsafe { text(function) };
        let Some(guard) = reserve(logger, None, function.as_deref(), None, line) else {
            return;
        };
        let Some(metadata) = metadata() else { return };
        let source = function
            .as_deref()
            .filter(|_| line != -1)
            .map(|name| (name, line));
        if output.is_null() {
            let _ = guard.callstack(&metadata, source);
        } else {
            let mut trace = Vec::new();
            if guard.callstack_to(&mut trace, &metadata, source).is_ok() {
                // SAFETY: C lends this writable stream; trace contains initialized UTF-8 bytes.
                unsafe {
                    libc::fwrite(trace.as_ptr().cast(), 1, trace.len(), output);
                    libc::fflush(output);
                }
            }
        }
    });
    if close_output && !output.is_null() {
        // SAFETY: C explicitly transfers ownership of this nonstandard stream.
        unsafe { libc::fclose(output) };
    }
}
