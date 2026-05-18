//! Tracing → broadcast channel tee + recent-history ring, consumed by
//! `/ws/logs` (live stream) and `/api/decoder/recent` (polling lookback).
//!
//! The browser's `LogPanel` connects to `/ws/logs` and receives every
//! log line ferrited would otherwise only write to stderr. Keeps
//! debugging a headless daemon practical when the UI is the only view
//! the user has.
//!
//! `ferrite-ctl` (and AI agents driving via the API) wants a different
//! shape: poll "what got decoded since I last asked," structured by
//! category (`decoder::pocsag`, `decoder::packet`, …). The broadcast
//! channel doesn't keep history — once a line is sent, late
//! subscribers miss it. So we maintain a parallel ring buffer of
//! recent `(timestamp, line)` entries and let HTTP handlers query it
//! by category + watermark.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;
use tracing::{field::Visit, Event, Subscriber};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

const CHANNEL_CAPACITY: usize = 256;
/// Ring buffer cap for historical `recent()` lookups. ~30 s at the
/// upper bound of decode chatter we've seen (a busy POCSAG carrier
/// emits up to ~50 lines/s); 4096 is enough headroom that an AI agent
/// polling every couple seconds never misses a line.
const HISTORY_CAPACITY: usize = 4096;

#[derive(Debug, Clone)]
pub struct LogEntry {
    /// Tracing target — `"decoder::pocsag"`, `"ferrited::routes"`, etc.
    pub target: String,
    /// Tracing level — `"INFO"` / `"WARN"` / etc.
    pub level: String,
    /// Already-rendered message body (without `[LEVEL] target:` prefix).
    pub message: String,
    /// `Date.now()`-shaped milliseconds since the unix epoch, used as
    /// the watermark a polling client passes to `recent(since=…)`.
    pub at_ms: u64,
}

#[derive(Clone)]
pub struct LogBroadcast {
    tx: broadcast::Sender<String>,
    /// Parallel ring buffer of structured entries, kept under a Mutex
    /// because the tracing layer pushes from many threads.
    history: Arc<Mutex<History>>,
}

impl LogBroadcast {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            tx,
            history: Arc::new(Mutex::new(History::new(HISTORY_CAPACITY))),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    pub fn layer(&self) -> BroadcastLayer {
        BroadcastLayer {
            tx: self.tx.clone(),
            history: Arc::clone(&self.history),
        }
    }

    /// Return entries with `at_ms > since_ms`, optionally filtered to a
    /// target prefix. Cheap copy — one Mutex lock + a single pass.
    pub fn recent(&self, since_ms: u64, target_prefix: Option<&str>) -> Vec<LogEntry> {
        // Recover the guard on poison: a panicked writer leaves the
        // history ring valid enough to read, and losing the log API
        // because something else panicked would hide the cause.
        let g = self.history.lock().unwrap_or_else(|e| e.into_inner());
        g.iter()
            .filter(|e| e.at_ms > since_ms)
            .filter(|e| target_prefix.is_none_or(|p| e.target.starts_with(p)))
            .cloned()
            .collect()
    }
}

/// Bounded ring of LogEntry. We don't need O(1) by-timestamp lookup —
/// callers do a linear scan filtered by the watermark. Total cost at
/// 4096 entries × small message bodies is well under a millisecond.
struct History {
    buf: Vec<LogEntry>,
    cap: usize,
    write: usize,
    full: bool,
}

impl History {
    fn new(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            cap,
            write: 0,
            full: false,
        }
    }

    fn push(&mut self, entry: LogEntry) {
        if self.full {
            self.buf[self.write] = entry;
            self.write = (self.write + 1) % self.cap;
        } else {
            self.buf.push(entry);
            if self.buf.len() == self.cap {
                self.full = true;
                self.write = 0;
            }
        }
    }

    fn iter(&self) -> impl Iterator<Item = &LogEntry> {
        // Yield in insertion order regardless of where the ring head sits.
        if self.full {
            let (a, b) = self.buf.split_at(self.write);
            b.iter().chain(a.iter())
        } else {
            // Empty second half; chain to satisfy the unified return type.
            let (a, b) = self.buf.split_at(self.buf.len());
            a.iter().chain(b.iter())
        }
    }
}

pub struct BroadcastLayer {
    tx: broadcast::Sender<String>,
    history: Arc<Mutex<History>>,
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
        let mut message = String::new();
        let mut visitor = MessageVisitor(&mut message);
        event.record(&mut visitor);
        line.push_str(&message);
        let _ = self.tx.send(line);

        // Push a structured copy to the history ring. The Mutex
        // serialises pushes across threads but is held for only the
        // O(1) push body.
        let at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        {
            let mut g = self.history.lock().unwrap_or_else(|e| e.into_inner());
            g.push(LogEntry {
                target: meta.target().to_string(),
                level: meta.level().to_string(),
                message,
                at_ms,
            });
        }
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
