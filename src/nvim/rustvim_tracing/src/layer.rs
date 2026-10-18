//! Tracing subscriber layer that formats events for Neovim's logging bridge.
#![forbid(unsafe_code)]

use std::fmt::Write;

use tracing::Event;
use tracing::Metadata;
use tracing::Subscriber;
use tracing::field::Field;
use tracing::field::Visit;
use tracing::subscriber::Interest;
use tracing::subscriber::SetGlobalDefaultError;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::layer::SubscriberExt;

use crate::ffi;

/// Subscriber layer that forwards enabled events to Neovim's logger.
pub(super) struct NvimLogLayer;

impl<S> Layer<S> for NvimLogLayer
where
    S: Subscriber,
{
    /// Keeps callsites dynamically filtered by Neovim's current log level.
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::sometimes()
    }

    /// Consults Neovim before allocating or formatting event fields.
    fn enabled(&self, metadata: &Metadata<'_>, _context: Context<'_, S>) -> bool {
        ffi::enabled(metadata.level())
    }

    /// Formats and forwards one enabled structured event.
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        ffi::emit(event.metadata(), &format_event(event));
    }
}

/// Separate message and structured-field buffers for one tracing event.
#[derive(Default)]
struct EventFields {
    /// Human-readable `message` field produced by tracing macros.
    message: Option<String>,
    /// Remaining structured fields in declaration order.
    fields: String,
}

impl EventFields {
    /// Joins the optional message and structured fields into one log payload.
    fn finish(self) -> String {
        let Some(mut message) = self.message else {
            return self.fields;
        };
        if !self.fields.is_empty() {
            message.push(' ');
            message.push_str(&self.fields);
        }
        message
    }
}

impl Visit for EventFields {
    /// Records values using their tracing-provided debug representation.
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let mut message = String::new();
            write!(&mut message, "{value:?}").expect("writing to a String cannot fail");
            self.message = Some(message);
            return;
        }

        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        write!(&mut self.fields, "{}={value:?}", field.name())
            .expect("writing to a String cannot fail");
    }
}

/// Formats one event without timestamps, levels, or source metadata.
fn format_event(event: &Event<'_>) -> String {
    let mut fields = EventFields::default();
    event.record(&mut fields);
    fields.finish()
}

/// Installs the Neovim layer as the process-wide tracing subscriber.
pub(super) fn install() -> Result<(), SetGlobalDefaultError> {
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(NvimLogLayer))
}

#[cfg(test)]
mod tests {
    //! Checks event formatting through a capturing subscriber layer.

    use std::sync::Arc;
    use std::sync::Mutex;

    use super::*;

    /// Test layer retaining one formatted event.
    struct CaptureLayer {
        /// Shared slot populated by `on_event`.
        event: Arc<Mutex<Option<String>>>,
    }

    impl<S> Layer<S> for CaptureLayer
    where
        S: Subscriber,
    {
        /// Captures one event through the production formatter.
        fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
            *self
                .event
                .lock()
                .expect("capture lock must not be poisoned") = Some(format_event(event));
        }
    }

    /// Marker used to verify debug-field formatting.
    #[derive(Debug)]
    struct TestFailure;

    /// Verifies messages and structured debug fields share one formatted payload.
    #[test]
    fn formats_messages_and_structured_fields_without_subscriber_fmt() {
        let event = Arc::new(Mutex::new(None));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer {
            event: Arc::clone(&event),
        });

        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(
                row = 3,
                error = ?TestFailure,
                "window-line rendering failed"
            );
        });

        let event = event
            .lock()
            .expect("capture lock must not be poisoned")
            .take()
            .expect("one event must be captured");
        assert!(event.starts_with("window-line rendering failed"));
        assert!(event.contains("row=3"));
        assert!(event.contains("error=TestFailure"));
    }
}
