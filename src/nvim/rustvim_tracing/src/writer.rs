//! UTF-8 output shared by native messages and tracing events.
#![forbid(unsafe_code)]

use std::io;
use std::io::Write;

use tracing::Level;

use crate::logger::LogRecord;

/// Returns Neovim's header label, using `DBG` for both debug and trace.
fn level_label(level: Level) -> &'static str {
    match level {
        Level::ERROR => "ERR",
        Level::WARN => "WRN",
        Level::INFO => "INF",
        _ => "DBG",
    }
}

/// Writes the existing Neovim log format and flushes the destination.
///
/// Strings are valid UTF-8 and are borrowed only for this synchronous call.
/// Native byte buffers must be decoded before constructing a record.
/// Instance names are padded to a minimum of ten bytes without truncation.
/// Missing source metadata uses the supplied context or `?:` as a fallback.
/// Partial and interrupted writes are retried; write and flush errors propagate.
pub fn write_record<W: Write + ?Sized>(output: &mut W, record: &LogRecord<'_>) -> io::Result<()> {
    write!(
        output,
        "{} {} ",
        level_label(record.level),
        record.timestamp
    )?;
    output.write_all(record.instance.as_bytes())?;
    for _ in record.instance.len()..10 {
        output.write_all(b" ")?;
    }
    output.write_all(b" ")?;

    if let Some((function, line)) = record.source {
        output.write_all(record.context.unwrap_or_default().as_bytes())?;
        output.write_all(function.as_bytes())?;
        write!(output, ":{line}: ")?;
    } else {
        output.write_all(record.context.unwrap_or("?:").as_bytes())?;
    }

    output.write_all(record.message.as_bytes())?;
    if record.newline {
        output.write_all(b"\n")?;
    }
    output.flush()
}

#[cfg(test)]
mod tests {
    //! Checks native record formatting, UTF-8 conversion, and I/O behavior.

    use std::io;
    use std::io::Cursor;
    use std::io::Write;

    use tracing::Level;

    use super::write_record;
    use crate::buffer::LogBuffer;
    use crate::logger::LogRecord;

    /// Builds a record with deterministic metadata and the supplied payload.
    fn record(message: &str) -> LogRecord<'_> {
        LogRecord {
            level: Level::WARN,
            timestamp: "2026-10-18T00:00:00.123",
            instance: "nvim",
            context: None,
            source: Some(("caller", 42)),
            message,
            newline: true,
        }
    }

    /// Checks the header, explicit context, and source location match the native format.
    #[test]
    fn preserves_the_native_header_and_source_context() {
        let mut output = Vec::new();
        let mut entry = record("message");
        entry.context = Some("RPC: ");

        write_record(&mut output, &entry).unwrap();

        assert_eq!(
            output,
            b"WRN 2026-10-18T00:00:00.123 nvim       RPC: caller:42: message\n"
        );
    }

    /// Checks absent source metadata uses an explicit context or the default marker.
    #[test]
    fn preserves_context_fallback_without_a_source_location() {
        let mut output = Vec::new();
        let mut entry = record("message");
        entry.source = None;

        write_record(&mut output, &entry).unwrap();
        assert_eq!(
            output,
            b"WRN 2026-10-18T00:00:00.123 nvim       ?:message\n"
        );

        output.clear();
        entry.context = Some("RPC: ");
        write_record(&mut output, &entry).unwrap();
        assert_eq!(
            output,
            b"WRN 2026-10-18T00:00:00.123 nvim       RPC: message\n"
        );
    }

    /// Verifies long instance names are emitted in full.
    #[test]
    fn treats_instance_width_as_a_minimum() {
        let mut output = Vec::new();
        let mut entry = record("message");
        entry.instance = "a-long-instance";

        write_record(&mut output, &entry).unwrap();

        assert_eq!(
            output,
            b"WRN 2026-10-18T00:00:00.123 a-long-instance caller:42: message\n"
        );
    }

    /// Checks native byte decoding preserves NULs and replaces invalid UTF-8.
    #[test]
    fn converts_native_buffer_bytes_to_utf8_without_adding_a_linefeed() {
        let mut buffer = LogBuffer::new();
        buffer.prepare_for_formatting().unwrap()[..4].copy_from_slice(&[b'a', 0, 0xff, 0]);
        let mut output = Vec::new();

        buffer
            .consume_message(3, |message| {
                let message = String::from_utf8_lossy(message);
                let mut entry = record(&message);
                entry.newline = false;
                write_record(&mut output, &entry)
            })
            .unwrap()
            .unwrap();

        assert_eq!(
            String::from_utf8(output).unwrap(),
            "WRN 2026-10-18T00:00:00.123 nvim       caller:42: a\0�"
        );
        assert!(buffer.prepare_for_formatting().is_some());
        buffer.cancel();
    }

    /// Verifies every tracing severity maps to the expected native label.
    #[test]
    fn maps_tracing_levels_to_native_labels() {
        for (level, expected) in [
            (Level::TRACE, b"DBG"),
            (Level::DEBUG, b"DBG"),
            (Level::INFO, b"INF"),
            (Level::WARN, b"WRN"),
            (Level::ERROR, b"ERR"),
        ] {
            let mut output = Vec::new();
            let mut entry = record("message");
            entry.level = level;

            write_record(&mut output, &entry).unwrap();

            assert_eq!(&output[..3], expected);
        }
    }

    /// Test output that interrupts once and accepts at most two bytes per write.
    struct ShortWriter {
        /// Bytes accepted by successful writes.
        bytes: Vec<u8>,
        /// Whether the next write should report an interruption.
        interrupt_next: bool,
        /// Number of flush calls received.
        flushes: usize,
    }

    impl Write for ShortWriter {
        /// Injects an interruption or appends up to two input bytes.
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.interrupt_next {
                self.interrupt_next = false;
                return Err(io::ErrorKind::Interrupted.into());
            }
            let count = bytes.len().min(2);
            self.bytes.extend_from_slice(&bytes[..count]);
            Ok(count)
        }

        /// Counts flush requests without changing the stored bytes.
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    /// Checks writes retry interruptions and short writes before one final flush.
    #[test]
    fn retries_partial_and_interrupted_writes_then_flushes_once() {
        let mut output = ShortWriter {
            bytes: Vec::new(),
            interrupt_next: true,
            flushes: 0,
        };

        write_record(&mut output, &record("message")).unwrap();

        assert_eq!(
            output.bytes,
            b"WRN 2026-10-18T00:00:00.123 nvim       caller:42: message\n"
        );
        assert_eq!(output.flushes, 1);
    }

    /// Checks failures at different record positions propagate without extra output.
    #[test]
    fn reports_write_failures_in_headers_messages_and_linefeeds() {
        let expected = b"WRN 2026-10-18T00:00:00.123 nvim       caller:42: message\n";
        for limit in [0, 2, expected.len() - 5, expected.len() - 1] {
            let mut storage = vec![0; limit];
            let error = {
                let mut output = Cursor::new(storage.as_mut_slice());
                write_record(&mut output, &record("message")).unwrap_err()
            };

            assert_eq!(error.kind(), io::ErrorKind::WriteZero);
            assert_eq!(storage, expected[..limit]);
        }
    }

    /// Test output that accepts all bytes but fails when flushed.
    struct FlushFailure(
        /// Bytes accepted before the flush failure.
        Vec<u8>,
    );

    impl Write for FlushFailure {
        /// Appends input bytes successfully so only flushing can fail.
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.write(bytes)
        }

        /// Reports the injected flush error.
        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }

    /// Verifies a final flush failure propagates after the complete record is written.
    #[test]
    fn reports_flush_failures_after_writing_the_record() {
        let mut output = FlushFailure(Vec::new());

        let error = write_record(&mut output, &record("message")).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            output.0,
            b"WRN 2026-10-18T00:00:00.123 nvim       caller:42: message\n"
        );
    }
}
