//! Tracing → broadcast channel tee, consumed by `/ws/logs`.
//!
//! The browser's `LogPanel` connects to `/ws/logs` and receives every log
//! line ferrited would otherwise only write to stderr. Keeps debugging a
//! headless daemon practical when the UI is the only view the user has.

use std::fmt::Write as _;

use tokio::sync::broadcast;
use tracing::{field::Visit, Event, Subscriber};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

const CHANNEL_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct LogBroadcast {
    tx: broadcast::Sender<String>,
}

impl LogBroadcast {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    pub fn layer(&self) -> BroadcastLayer {
        BroadcastLayer {
            tx: self.tx.clone(),
        }
    }
}

pub struct BroadcastLayer {
    tx: broadcast::Sender<String>,
}

impl<S> Layer<S> for BroadcastLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut line = String::with_capacity(96);
        let _ = write!(line, "[{}] ", meta.level());
        // Prefer the explicit `target` — it's what
        // `tracing::info!(target: "decoder::pocsag", …)` sets, and it
        // defaults to the module_path when the caller didn't override.
        // Choosing target over module_path means decoder/log lines can
        // declare their own category (`decoder::pocsag`,
        // `decoder::flex`, …) instead of being filed under whatever
        // file they happen to live in.
        let _ = write!(line, "{}: ", meta.target());
        let mut visitor = MessageVisitor(&mut line);
        event.record(&mut visitor);
        let _ = self.tx.send(line);
    }
}

struct MessageVisitor<'a>(&'a mut String);

impl Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        } else {
            let _ = write!(self.0, " {}={value}", field.name());
        }
    }
}
