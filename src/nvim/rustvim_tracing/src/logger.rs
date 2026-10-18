//! Safe logging shared by native callers and tracing events.
#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fmt::Write as _;
use std::fs::DirBuilder;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::marker::PhantomData;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;
use std::thread::ThreadId;

use tracing::Level;

use crate::writer::write_record;

/// Runtime context used to construct a log record.
///
/// Holds the timestamp, UI status, parent/server names, and process ID.
/// These are inputs for choosing the final instance name; for example,
/// a UI client with parent "server" is named "ui/c/server".
///
/// Native values are converted to owned UTF-8 strings by the FFI adapter.
/// The logger combines this metadata with a [`LogMessage`] to form a
/// [`LogRecord`].
pub struct LogMetadata {
    /// Local timestamp already formatted for the log header.
    pub timestamp: String,
    /// Whether the process is acting as a remote UI client.
    pub ui: bool,
    /// Parent instance name; an empty string means no parent is known.
    pub parent: String,
    /// This instance's server name; empty before a name is assigned.
    pub server: String,
    /// Process ID used when neither parent nor server name is available.
    pub pid: u32,
}

/// Complete log entry ready for synchronous output.
///
/// Combines the timestamp and final instance name with the level, context,
/// source, message, and newline flag. The logger builds it from
/// [`LogMetadata`] and [`LogMessage`], and the writer formats it.
///
/// All text is borrowed UTF-8 and remains valid for the duration of the write.
pub struct LogRecord<'a> {
    /// Severity mapped to Neovim's three-letter log label.
    pub level: Level,
    /// Formatted timestamp copied from the runtime metadata.
    pub timestamp: &'a str,
    /// Final instance name selected by the logger.
    pub instance: &'a str,
    /// Prefix before the source or message, including any desired punctuation.
    pub context: Option<&'a str>,
    /// Optional function or file name and line number.
    pub source: Option<(&'a str, i32)>,
    /// UTF-8 message payload, which may contain embedded NULs.
    pub message: &'a str,
    /// Whether to append a newline after the payload.
    pub newline: bool,
}

/// One message, borrowed until the synchronous write returns.
pub struct LogMessage<'a> {
    /// Severity used for the completed record's header.
    pub level: Level,
    /// Prefix before the source or message, including any desired punctuation.
    pub context: Option<&'a str>,
    /// Optional function or file name and line number supplied by the caller.
    pub source: Option<(&'a str, i32)>,
    /// UTF-8 payload decoded or formatted before writing.
    pub message: &'a str,
    /// Whether the writer should append a newline.
    pub newline: bool,
}

/// Reasons to reject a message before formatting its payload.
#[derive(Debug, Eq, PartialEq)]
pub enum Rejected {
    /// The message severity is below the current minimum level.
    Filtered,
    /// This thread already has an active guard for the same logger.
    Recursive {
        /// Whether this is the first recursion rejection for this logger.
        notify: bool,
    },
}

/// A standard Rust destination. Native stream adaptation belongs in the FFI.
pub enum LogOutput {
    /// Log file opened for appending one diagnostic or message.
    File(
        /// Owned file closed when this output is dropped.
        File,
    ),
    /// Standard error used when no log file is available.
    Stderr(
        /// Handle to the process stream; dropping it does not close stderr.
        io::Stderr,
    ),
}

impl Write for LogOutput {
    /// Delegates a possibly partial write to the selected output.
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.write(bytes),
            Self::Stderr(stderr) => stderr.write(bytes),
        }
    }

    /// Flushes the selected output and propagates any I/O error.
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::Stderr(stderr) => stderr.flush(),
        }
    }
}

/// Owns the destination policy, cached name, and output synchronization.
pub struct Logger {
    /// Selected append path, or `None` to write to standard error.
    destination: Option<PathBuf>,
    /// Failed preferred path retained for Neovim's startup diagnostic.
    wanted_path: Option<PathBuf>,
    /// Selected fallback path to publish through `NVIM_LOG_FILE`.
    environment_path: Option<PathBuf>,
    /// Failure to create the default log directory, if any.
    directory_error: Option<io::Error>,
    /// Threads holding guards; locked only while checking or changing membership.
    active_threads: Mutex<HashSet<ThreadId>>,
    /// Cached name and reusable record storage, locked throughout output.
    state: Mutex<LogState>,
    /// Whether a recursion rejection has already requested a diagnostic.
    recursion_reported: AtomicBool,
}

/// Mutable output state protected by the owning logger's state mutex.
#[derive(Default)]
struct LogState {
    /// Instance name cached between writes and refreshed according to metadata.
    name: String,
    /// Reusable UTF-8 record bytes, cleared before assembling each message.
    record: Vec<u8>,
}

impl LogState {
    /// Refreshes UI and provisional names while retaining assigned non-UI names.
    ///
    /// Parent names take precedence over server names. When neither is known,
    /// the process ID supplies a provisional name. Names are never truncated.
    fn update_name(&mut self, metadata: &LogMetadata) {
        if !metadata.ui && !self.name.is_empty() && !self.name.starts_with('?') {
            return;
        }

        self.name.clear();
        if !metadata.parent.is_empty() {
            self.name.push_str(if metadata.ui { "ui/c/" } else { "c/" });
            self.name.push_str(&metadata.parent);
        } else if !metadata.server.is_empty() {
            if metadata.ui {
                self.name.push_str("ui/");
            }
            self.name.push_str(&metadata.server);
        } else {
            self.name.push_str(if metadata.ui { "ui." } else { "?." });
            write!(&mut self.name, "{:<5}", metadata.pid)
                .expect("writing to a String cannot fail");
        }
    }

    /// Assembles a complete record in reusable storage, then writes and flushes it.
    ///
    /// The caller must update the cached instance name before calling this method.
    /// Formatting and destination I/O errors are returned to the caller.
    fn write_record(
        &mut self,
        output: &mut dyn Write,
        metadata: &LogMetadata,
        message: &LogMessage<'_>,
    ) -> io::Result<()> {
        self.record.clear();
        write_record(
            &mut self.record,
            &LogRecord {
                level: message.level,
                timestamp: &metadata.timestamp,
                instance: &self.name,
                context: message.context,
                source: message.source,
                message: message.message,
                newline: message.newline,
            },
        )?;
        output.write_all(&self.record)?;
        output.flush()
    }

    /// Writes a trace header, then symbolizes native frame addresses with `addr2line`.
    ///
    /// This preserves the existing C stack-trace behavior. The first frame is
    /// skipped to omit the capture helper. Arguments are passed without a shell,
    /// and invalid UTF-8 in symbolizer output is replaced before writing.
    /// Process, symbolization, and output failures are returned as I/O errors.
    fn write_callstack(
        &mut self,
        output: &mut dyn Write,
        metadata: &LogMetadata,
        source: (&str, i32),
        frames: &[usize],
    ) -> io::Result<()> {
        let message = LogMessage {
            level: Level::DEBUG,
            context: None,
            source: Some(source),
            message: "trace:",
            newline: true,
        };
        self.write_record(output, metadata, &message)?;

        let symbols = Command::new("addr2line")
            .arg("-e")
            .arg(std::env::current_exe()?)
            .args(["-f", "-p"])
            .args(frames.iter().skip(1).map(|frame| format!("0x{frame:x}")))
            .output()?;
        if !symbols.status.success() {
            return Err(io::Error::other(format!(
                "addr2line failed: {}",
                String::from_utf8_lossy(&symbols.stderr).trim_end()
            )));
        }
        Logger::write_stack_lines(output, &String::from_utf8_lossy(&symbols.stdout))?;
        output.flush()
    }
}

impl Logger {
    /// Selects the requested, state-directory, current-directory, or stderr
    /// destination. Paths have already been expanded by the Neovim adapter.
    ///
    /// Probing a path may create its log file. If the requested path cannot be
    /// opened, creates the default path's parent directories and tries that
    /// path, then `nvim.log` in the current directory. Retains startup diagnostic
    /// information and any fallback path for the adapter to publish.
    pub fn initialize(requested: Option<PathBuf>, default: PathBuf) -> Self {
        if let Some(path) = requested.as_ref().filter(|path| Self::can_open(path)) {
            return Self::new(Some(path.clone()), None, None, None);
        }

        let mut wanted = requested;
        let directory_error = default.parent().and_then(|parent| {
            let mut builder = DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(parent).err()
        });

        let destination = if Self::can_open(&default) {
            Some(default)
        } else {
            if wanted.is_none() {
                wanted = Some(default);
            }
            let fallback = PathBuf::from("nvim.log");
            Self::can_open(&fallback).then_some(fallback)
        };
        let environment_path = destination.clone();
        Self::new(destination, wanted, environment_path, directory_error)
    }

    /// Constructs a logger from an already selected destination and diagnostics.
    ///
    /// Performs no filesystem I/O. Initialization selects these values; tests can
    /// use this constructor directly without creating files.
    fn new(
        destination: Option<PathBuf>,
        wanted_path: Option<PathBuf>,
        environment_path: Option<PathBuf>,
        directory_error: Option<io::Error>,
    ) -> Self {
        Self {
            destination,
            wanted_path,
            environment_path,
            directory_error,
            active_threads: Mutex::new(HashSet::new()),
            state: Mutex::new(LogState::default()),
            recursion_reported: AtomicBool::new(false),
        }
    }

    /// Value for Neovim's __NVIM_LOG_FILE_WANT startup diagnostic.
    pub fn wanted_path(&self) -> Option<&Path> {
        self.wanted_path.as_deref()
    }

    /// Replacement NVIM_LOG_FILE value when a fallback path was selected.
    pub fn environment_path(&self) -> Option<&Path> {
        self.environment_path.as_deref()
    }

    /// Directory creation failure retained for the startup adapter.
    pub fn directory_error(&self) -> Option<&io::Error> {
        self.directory_error.as_ref()
    }

    /// Filters the level and reserves this thread before native formatting.
    ///
    /// The guard must stay alive while C writes into its Rust-owned buffer.
    /// Returns [`Rejected::Filtered`] below the native minimum level, or
    /// [`Rejected::Recursive`] if this thread already holds a guard.
    pub fn prepare(&self, level: Level, minimum: i32) -> Result<LogGuard<'_>, Rejected> {
        if Self::level_number(level) < minimum {
            return Err(Rejected::Filtered);
        }
        self.prepare_unfiltered()
    }

    /// Reserves output for unfiltered native diagnostics such as callstacks.
    ///
    /// Rejects re-entry on this thread while allowing other threads to prepare
    /// messages. The returned guard does not hold either logger mutex.
    pub fn prepare_unfiltered(&self) -> Result<LogGuard<'_>, Rejected> {
        let thread_id = thread::current().id();
        let inserted = self.lock_active_threads().insert(thread_id);
        if !inserted {
            return Err(Rejected::Recursive {
                notify: !self.recursion_reported.swap(true, Ordering::Relaxed),
            });
        }
        Ok(LogGuard {
            logger: self,
            thread_id,
            _not_send: PhantomData,
        })
    }

    /// Locks recursion tracking, recovering its contents after a previous panic.
    fn lock_active_threads(&self) -> MutexGuard<'_, HashSet<ThreadId>> {
        self.active_threads
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Locks reusable output state, recovering its contents after a previous panic.
    fn lock_state(&self) -> MutexGuard<'_, LogState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    /// Reopens the selected path for append, or returns standard error.
    ///
    /// On an open failure, attempts to report the error directly to stderr without
    /// re-entering the logger. Failure to write that diagnostic is ignored.
    fn open_output(&self, state: &LogState, metadata: &LogMetadata) -> LogOutput {
        let Some(path) = self.destination.as_ref() else {
            return LogOutput::Stderr(io::stderr());
        };
        match Self::open_append(path) {
            Ok(file) => LogOutput::File(file),
            Err(error) => {
                let mut output = LogOutput::Stderr(io::stderr());
                let message = format!(
                    "failed to open $NVIM_LOG_FILE ({error}): {}",
                    path.display()
                );
                let diagnostic = LogRecord {
                    level: Level::ERROR,
                    timestamp: &metadata.timestamp,
                    instance: &state.name,
                    context: None,
                    source: Some(("open_log_file", line!() as i32)),
                    message: &message,
                    newline: true,
                };
                let _ = write_record(&mut output, &diagnostic);
                output
            }
        }
    }

    /// Emits the EXITFREE fatal diagnostic using UTF-8, then terminates.
    ///
    /// Bypasses normal filtering, destination selection, and logger state.
    /// Output errors are ignored so they cannot prevent the process abort.
    pub fn fatal(
        context: Option<&str>,
        function: Option<&str>,
        line: i32,
        message: &str,
        newline: bool,
    ) -> ! {
        let mut output = io::stderr().lock();
        let _ = write!(
            output,
            "FATAL: error in free_all_mem\n {} {} {line}: ",
            context.unwrap_or("(null)"),
            function.unwrap_or("(null)")
        );
        let _ = output.write_all(message.as_bytes());
        if newline {
            let _ = output.write_all(b"\n");
        }
        let _ = output.flush();
        std::process::abort()
    }

    /// Maps tracing severity to native levels 1 through 4; trace shares debug.
    fn level_number(level: Level) -> i32 {
        match level {
            Level::ERROR => 4,
            Level::WARN => 3,
            Level::INFO => 2,
            _ => 1,
        }
    }

    /// Opens a file for appending, creating it if absent, and returns any I/O error.
    fn open_append(path: &Path) -> io::Result<File> {
        OpenOptions::new().create(true).append(true).open(path)
    }

    /// Tests a nonempty append path, creating its file if needed and closing it.
    fn can_open(path: &Path) -> bool {
        !path.as_os_str().is_empty() && Self::open_append(path).is_ok()
    }

    /// Indents each UTF-8 symbol line, preserving existing newline boundaries.
    ///
    /// Does not add a final newline or flush the destination.
    fn write_stack_lines(output: &mut dyn Write, symbols: &str) -> io::Result<()> {
        for line in symbols.split_inclusive('\n') {
            output.write_all(b"  ")?;
            output.write_all(line.as_bytes())?;
        }
        Ok(())
    }
}

/// Prevents recursive logging throughout formatting and output.
///
/// Formatting arguments, resolving metadata, or writing a record can invoke
/// code that logs again. The logger records its active thread IDs so re-entry
/// is rejected before it can reuse the message buffer or deadlock on output.
///
/// Dropping this guard removes its thread ID on normal returns, errors, and
/// panic unwinding. Neither logger mutex remains locked while C formats.
///
/// The Rc marker keeps the guard on its creating thread so recursion tracking
/// also covers code invoked during output.
pub struct LogGuard<'a> {
    /// Logger whose active-thread entry this guard owns.
    logger: &'a Logger,
    /// Creating thread, removed from recursion tracking when the guard drops.
    thread_id: ThreadId,
    /// Prevents moving or sharing the reservation across threads.
    _not_send: PhantomData<Rc<()>>,
}

impl LogGuard<'_> {
    /// Writes a message, opening the destination afresh to support log rotation.
    pub fn write(&self, metadata: &LogMetadata, message: &LogMessage<'_>) -> io::Result<()> {
        let mut state = self.logger.lock_state();
        state.update_name(metadata);
        let mut output = self.logger.open_output(&state, metadata);
        state.write_record(&mut output, metadata, message)
    }

    /// Writes to a borrowed destination supplied by a native FILE adapter.
    pub fn write_to(
        &self,
        output: &mut dyn Write,
        metadata: &LogMetadata,
        message: &LogMessage<'_>,
    ) -> io::Result<()> {
        let mut state = self.logger.lock_state();
        state.update_name(metadata);
        state.write_record(output, metadata, message)
    }

    /// Keeps the output lock while a native diagnostic writes its output.
    ///
    /// Returns the callback's result without adding a record or flushing.
    /// The callback must not invoke output methods on this guard because the
    /// state mutex remains locked until the callback returns.
    pub fn with_output<R>(
        &self,
        metadata: &LogMetadata,
        write: impl FnOnce(&mut LogOutput) -> io::Result<R>,
    ) -> io::Result<R> {
        let mut state = self.logger.lock_state();
        state.update_name(metadata);
        let mut output = self.logger.open_output(&state, metadata);
        write(&mut output)
    }

    /// Writes symbolized native frames without constructing a shell command.
    pub fn callstack(
        &self,
        metadata: &LogMetadata,
        source: (&str, i32),
        frames: &[usize],
    ) -> io::Result<()> {
        let mut state = self.logger.lock_state();
        state.update_name(metadata);
        let mut output = self.logger.open_output(&state, metadata);
        state.write_callstack(&mut output, metadata, source, frames)
    }

    /// Writes symbolized native frames to a borrowed FILE adapter.
    pub fn callstack_to(
        &self,
        output: &mut dyn Write,
        metadata: &LogMetadata,
        source: (&str, i32),
        frames: &[usize],
    ) -> io::Result<()> {
        let mut state = self.logger.lock_state();
        state.update_name(metadata);
        state.write_callstack(output, metadata, source, frames)
    }
}

impl Drop for LogGuard<'_> {
    /// Releases this thread's reservation, including during panic unwinding.
    fn drop(&mut self) {
        self.logger.lock_active_threads().remove(&self.thread_id);
    }
}

#[cfg(test)]
mod tests {
    //! Checks UTF-8 output, instance naming, filtering, and recursion guards.

    use std::panic::AssertUnwindSafe;
    use std::panic::catch_unwind;
    use std::sync::Arc;

    use super::*;

    /// Constructs a fresh logger without probing paths or creating files.
    fn logger() -> Logger {
        Logger::new(None, None, None, None)
    }

    /// Provides deterministic runtime metadata for logger tests.
    fn metadata() -> LogMetadata {
        LogMetadata {
            timestamp: "2026-10-18T00:00:00.123".into(),
            ui: false,
            parent: String::new(),
            server: "server".into(),
            pid: 42,
        }
    }

    /// Checks long Unicode names and payloads are emitted without truncation.
    #[test]
    fn keeps_complete_unicode_names() {
        let logger = logger();
        let guard = logger.prepare(Level::WARN, 3).unwrap();
        let mut metadata = metadata();
        metadata.parent = "é".repeat(40);
        let message = LogMessage {
            level: Level::WARN,
            context: None,
            source: Some(("caller", 42)),
            message: "héllo",
            newline: true,
        };
        let mut output = Vec::new();

        guard.write_to(&mut output, &metadata, &message).unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!(
                "WRN 2026-10-18T00:00:00.123 c/{} caller:42: héllo\n",
                "é".repeat(40)
            )
        );
    }

    /// Verifies provisional and UI names refresh while assigned server names persist.
    #[test]
    fn refreshes_startup_and_ui_names_but_keeps_assigned_names() {
        let mut state = LogState::default();
        let mut metadata = metadata();
        metadata.server.clear();
        state.update_name(&metadata);
        assert_eq!(state.name, "?.42   ");

        metadata.server = "server".into();
        state.update_name(&metadata);
        assert_eq!(state.name, "server");

        metadata.parent = "parent".into();
        state.update_name(&metadata);
        assert_eq!(state.name, "server");

        metadata.ui = true;
        state.update_name(&metadata);
        assert_eq!(state.name, "ui/c/parent");

        metadata.parent.clear();
        state.update_name(&metadata);
        assert_eq!(state.name, "ui/server");
    }

    /// Checks a filtered message does not reserve the thread.
    #[test]
    fn filters_before_reserving_the_thread() {
        let logger = logger();
        assert!(matches!(
            logger.prepare(Level::INFO, 3),
            Err(Rejected::Filtered)
        ));
        assert!(logger.prepare(Level::ERROR, 3).is_ok());
    }

    /// Checks recursion requests one diagnostic and dropping the guard permits reuse.
    #[test]
    fn reports_recursion_once_and_releases_the_reservation() {
        let logger = logger();
        let guard = logger.prepare(Level::WARN, 3).unwrap();
        assert!(matches!(
            logger.prepare(Level::ERROR, 3),
            Err(Rejected::Recursive { notify: true })
        ));
        assert!(matches!(
            logger.prepare(Level::ERROR, 3),
            Err(Rejected::Recursive { notify: false })
        ));
        drop(guard);
        assert!(logger.prepare(Level::WARN, 3).is_ok());
    }

    /// Verifies one logger's active guard does not reserve a different logger.
    #[test]
    fn keeps_recursion_tracking_separate_for_each_logger() {
        let first = logger();
        let second = logger();
        let _first_guard = first.prepare(Level::WARN, 3).unwrap();
        let _second_guard = second.prepare(Level::WARN, 3).unwrap();

        assert!(matches!(
            first.prepare(Level::ERROR, 3),
            Err(Rejected::Recursive { notify: true })
        ));
    }

    /// Checks recursion tracking rejects only the thread already holding a guard.
    #[test]
    fn allows_other_threads_to_log_to_the_same_logger() {
        let logger = Arc::new(logger());
        let guard = logger.prepare(Level::WARN, 3).unwrap();
        let other = Arc::clone(&logger);

        thread::spawn(move || {
            let _guard = other.prepare(Level::WARN, 3).unwrap();
        })
        .join()
        .unwrap();

        assert!(matches!(
            logger.prepare(Level::ERROR, 3),
            Err(Rejected::Recursive { notify: true })
        ));
        drop(guard);
        assert!(logger.prepare(Level::WARN, 3).is_ok());
    }

    /// Verifies panic unwinding releases the active-thread reservation.
    #[test]
    fn releases_the_reservation_during_unwinding() {
        let logger = logger();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _guard = logger.prepare(Level::WARN, 3).unwrap();
            panic!("formatting failed");
        }));

        assert!(result.is_err());
        assert!(logger.prepare(Level::WARN, 3).is_ok());
    }

    /// Checks stack indentation preserves Unicode and a missing final newline.
    #[test]
    fn indents_stack_lines_without_splitting_utf8() {
        let mut output = Vec::new();

        Logger::write_stack_lines(&mut output, "é:1\n東京:2").unwrap();

        assert_eq!(String::from_utf8(output).unwrap(), "  é:1\n  東京:2");
    }
}
