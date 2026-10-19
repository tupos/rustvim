//! Reusable storage for one native formatting operation at a time.
#![forbid(unsafe_code)]

/// Initial writable size, including the byte reserved for the terminating NUL.
const INITIAL_SIZE: usize = 1024;

/// Rust-owned storage that includes room for a terminating NUL byte.
///
/// Completed messages are borrowed synchronously, allowing the allocation to
/// be reused after the consumer returns.
#[derive(Default)]
pub struct LogBuffer {
    /// Initialized bytes retained across formatting operations.
    storage: Vec<u8>,
    /// Whether a formatter owns the storage until consumption or cancellation.
    in_use: bool,
}

impl LogBuffer {
    /// Creates an empty buffer without allocating.
    pub const fn new() -> Self {
        Self {
            storage: Vec::new(),
            in_use: false,
        }
    }

    /// Prepares storage for formatting, rejecting another active operation.
    ///
    /// Every returned byte is initialized. The slice includes space for NUL.
    pub fn prepare_for_formatting(&mut self) -> Option<&mut [u8]> {
        if self.in_use {
            return None;
        }
        if self.storage.is_empty() {
            self.storage.resize(INITIAL_SIZE, 0);
        }
        self.in_use = true;
        Some(&mut self.storage)
    }

    /// Ensures an active formatter has at least `minimum` bytes, including NUL.
    ///
    /// Storage never shrinks, so later messages reuse the larger allocation.
    /// Returns `None` when no formatter is active or `minimum` is zero.
    pub fn grow(&mut self, minimum: usize) -> Option<&mut [u8]> {
        if !self.in_use || minimum == 0 {
            return None;
        }
        if minimum > self.storage.len() {
            self.storage.resize(minimum, 0);
        }
        Some(&mut self.storage)
    }

    /// Consumes exactly `written` message bytes, excluding the terminating NUL.
    ///
    /// Invalid lengths or a missing terminator reject the message. The buffer
    /// becomes available again even if the consumer unwinds.
    /// Returns `None` if no formatter is active or validation fails.
    ///
    /// The borrowed bytes may contain embedded NULs or invalid UTF-8. The
    /// consumer must decode them before constructing a text log record.
    pub fn consume_message<R>(
        &mut self,
        written: usize,
        consume: impl FnOnce(&[u8]) -> R,
    ) -> Option<R> {
        if !self.in_use {
            return None;
        }
        let _release = Release(&mut self.in_use);
        if self.storage.get(written) != Some(&0) {
            return None;
        }
        Some(consume(&self.storage[..written]))
    }

    /// Formats into reusable storage, growing and retrying when it does not fit.
    ///
    /// The formatter returns the required byte count excluding NUL, or None on
    /// failure. It must terminate a completed message with NUL. The consumer
    /// borrows UTF-8; invalid native bytes become replacement characters.
    /// The reservation is released on errors and panic unwinding.
    pub fn format<R>(
        &mut self,
        mut formatter: impl FnMut(&mut [u8]) -> Option<usize>,
        consume: impl FnOnce(&str) -> R,
    ) -> Option<R> {
        self.prepare_for_formatting()?;
        let Self { storage, in_use } = self;
        let _release = Release(in_use);
        loop {
            let written = formatter(storage)?;
            if written < storage.len() {
                if storage[written] != 0 {
                    return None;
                }
                return Some(consume(&String::from_utf8_lossy(&storage[..written])));
            }
            storage.resize(written.checked_add(1)?, 0);
        }
    }

    /// Cancels formatting while retaining the allocation.
    pub fn cancel(&mut self) {
        self.in_use = false;
    }
}

/// Releases the formatting reservation on return or panic unwinding.
struct Release<'a>(
    /// Reservation flag cleared when this guard is dropped.
    &'a mut bool,
);

impl Drop for Release<'_> {
    /// Makes the retained buffer available for the next formatting operation.
    fn drop(&mut self) {
        *self.0 = false;
    }
}

#[cfg(test)]
mod tests {
    //! Checks buffer ownership, allocation reuse, and release on every exit path.

    use std::panic::AssertUnwindSafe;
    use std::panic::catch_unwind;

    use super::INITIAL_SIZE;
    use super::LogBuffer;

    /// Verifies storage preserves bytes for decoding by the message consumer.
    #[test]
    fn accepts_raw_message_bytes() {
        let mut buffer = LogBuffer::new();
        buffer.prepare_for_formatting().unwrap()[..5].copy_from_slice(&[b'a', 0, 0xff, b'z', 0]);

        assert_eq!(
            buffer.consume_message(4, |message| message.to_vec()),
            Some(vec![b'a', 0, 0xff, b'z'])
        );
    }

    /// Checks empty messages and rejection of consumption without a reservation.
    #[test]
    fn allows_empty_messages() {
        let mut buffer = LogBuffer::new();
        assert_eq!(buffer.consume_message(0, |_| ()), None);
        assert!(buffer.grow(16).is_none());

        buffer.prepare_for_formatting().unwrap()[0] = 0;
        assert_eq!(buffer.consume_message(0, |message| message.len()), Some(0));
    }

    /// Checks that growth is retained and later messages reuse the same allocation.
    #[test]
    fn reuses_storage_after_growth() {
        let mut buffer = LogBuffer::new();
        let initial_pointer = buffer.prepare_for_formatting().unwrap().as_ptr();
        assert_eq!(buffer.grow(INITIAL_SIZE).unwrap().as_ptr(), initial_pointer);

        let storage = buffer.grow(INITIAL_SIZE * 4).unwrap();
        let grown_pointer = storage.as_ptr();
        let grown_size = storage.len();
        storage[..6].copy_from_slice(b"hello\0");
        assert_eq!(
            buffer.consume_message(5, |message| message.to_vec()),
            Some(b"hello".to_vec())
        );

        let storage = buffer.prepare_for_formatting().unwrap();
        assert_eq!(storage.as_ptr(), grown_pointer);
        assert_eq!(storage.len(), grown_size);
        storage[..2].copy_from_slice(b"x\0");
        assert_eq!(
            buffer.consume_message(1, |message| message.to_vec()),
            Some(b"x".to_vec())
        );
    }

    /// Verifies a rejected nested operation leaves the active message intact.
    #[test]
    fn rejects_nested_formatting_without_disturbing_the_message() {
        let mut buffer = LogBuffer::new();
        buffer.prepare_for_formatting().unwrap()[..6].copy_from_slice(b"outer\0");

        assert!(buffer.prepare_for_formatting().is_none());
        assert_eq!(
            buffer.consume_message(5, |message| message.to_vec()),
            Some(b"outer".to_vec())
        );
        assert!(buffer.prepare_for_formatting().is_some());
        buffer.cancel();
    }

    /// Checks validation failures release the reservation without calling the consumer.
    #[test]
    fn rejects_invalid_lengths_and_missing_terminators() {
        let mut buffer = LogBuffer::new();
        for written in [INITIAL_SIZE, usize::MAX] {
            buffer.prepare_for_formatting().unwrap();
            assert_eq!(
                buffer.consume_message(written, |_| panic!("invalid message consumed")),
                None
            );
            assert!(buffer.prepare_for_formatting().is_some());
            buffer.cancel();
        }

        buffer.prepare_for_formatting().unwrap()[0] = b'x';
        assert_eq!(
            buffer.consume_message(0, |_| panic!("unterminated message consumed")),
            None
        );
        assert!(buffer.prepare_for_formatting().is_some());
        buffer.cancel();
    }

    /// Checks cancellation releases ownership while retaining the allocation.
    #[test]
    fn cancellation_releases_the_buffer_without_reallocating() {
        let mut buffer = LogBuffer::new();
        let pointer = buffer.prepare_for_formatting().unwrap().as_ptr();
        buffer.cancel();

        assert_eq!(buffer.consume_message(0, |_| ()), None);
        assert!(buffer.grow(INITIAL_SIZE * 2).is_none());
        assert_eq!(buffer.prepare_for_formatting().unwrap().as_ptr(), pointer);
        buffer.cancel();
    }

    /// Verifies a panicking consumer cannot leave the formatting buffer reserved.
    #[test]
    fn releases_the_buffer_during_unwinding() {
        let mut buffer = LogBuffer::new();
        buffer.prepare_for_formatting().unwrap()[0] = 0;

        let result = catch_unwind(AssertUnwindSafe(|| {
            buffer.consume_message(0, |_| panic!("consumer failed"))
        }));

        assert!(result.is_err());
        assert!(buffer.prepare_for_formatting().is_some());
        buffer.cancel();
    }

    /// Checks retries preserve a full UTF-8 message and reuse the grown allocation.
    #[test]
    fn formatting_retries_and_reuses_the_buffer() {
        let mut buffer = LogBuffer::new();
        let payload = "é".repeat(INITIAL_SIZE);
        let mut calls = 0;
        let mut grown_pointer = std::ptr::null();
        assert_eq!(
            buffer.format(
                |output| {
                    calls += 1;
                    if output.len() > payload.len() {
                        output[..payload.len()].copy_from_slice(payload.as_bytes());
                        output[payload.len()] = 0;
                        grown_pointer = output.as_ptr();
                    }
                    Some(payload.len())
                },
                |message| message == payload
            ),
            Some(true)
        );
        assert_eq!(calls, 2);
        assert_eq!(
            buffer.format(
                |output| {
                    assert_eq!(output.as_ptr(), grown_pointer);
                    output[..2].copy_from_slice(b"x\0");
                    Some(1)
                },
                |message| message == "x"
            ),
            Some(true)
        );
    }

    /// Checks native encoding errors become valid UTF-8 without truncating the text.
    #[test]
    fn formatting_replaces_invalid_utf8() {
        let mut buffer = LogBuffer::new();
        assert_eq!(
            buffer.format(
                |output| {
                    output[..4].copy_from_slice(&[b'a', 0xff, b'b', 0]);
                    Some(3)
                },
                str::to_owned
            ),
            Some("a\u{fffd}b".to_owned())
        );
    }

    /// Checks formatter errors and malformed output release storage for another call.
    #[test]
    fn formatting_failures_release_the_buffer() {
        let mut buffer = LogBuffer::new();
        assert_eq!(buffer.format(|_| None, |_| ()), None);
        assert_eq!(
            buffer.format(
                |output| {
                    output[0] = b'x';
                    Some(0)
                },
                |_| ()
            ),
            None
        );
        assert_eq!(buffer.format(|_| Some(usize::MAX), |_| ()), None);
        assert!(buffer.prepare_for_formatting().is_some());
    }
}
