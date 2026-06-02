//! `EventStore` — terminal sink for `events` ports that feeds the
//! server-side decoder store (the WS-replicated `DecoderStore`).
//!
//! Replaces `WsBridgeTxEvents` for decoder `ui:<kind>` event sinks. Each
//! complete newline-delimited JSON line on the input is parsed and pushed
//! to the attached [`DecoderSink`] under this block's `kind`. The server
//! implements `DecoderSink` against its `DecoderStore`; the block stays
//! free of any server/transport dependency — exactly the decoupling
//! [`BridgeSink`](crate::ws_bridge::BridgeSink) gives the WS bridges.
//!
//! `Placement::Either`:
//! - **node** — `attach_store` wires the real sink; `process` emits to it.
//! - **browser** (browser-demod under the channelizer cut) — no sink is
//!   attached; lines buffer and the browser runtime drains them via
//!   [`EventStore::drain_events`] and POSTs to `/api/decodes/:kind`. The
//!   block being `Either` is what lets it travel with the decode chain.
//!
//! The block owns the record *schema* (it forwards each event's JSON
//! verbatim); the store owns the *fold policy* (append / upsert / replace)
//! keyed by `kind`. `reset_on_init` clears the kind when a fresh sink is
//! attached (preset load / graph rebuild) so stale state never lingers.

use std::sync::Arc;

use anyhow::Result;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InBuf, InitCtx, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

/// Transport-free hook the [`EventStore`] block pushes decoded records
/// through. Implemented server-side against the `DecoderStore`; mirrors
/// [`BridgeSink`](crate::ws_bridge::BridgeSink).
pub trait DecoderSink: Send + Sync {
    /// Ingest one decoded record (already-parsed JSON) under `kind`. The
    /// store applies the per-kind fold policy (append / upsert / replace).
    fn emit(&self, kind: &str, data: serde_json::Value);
    /// Clear all state for `kind` (e.g. on a fresh decode session).
    fn reset(&self, kind: &str);
}

/// Construction-time params.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EventStoreParams {
    /// Store kind this sink feeds (e.g. `adsb`, `aprs`, `ft8`, `signals`).
    /// Normally set by env_split from the `ui:<kind>` sink name.
    pub kind: String,
    /// Clear the kind when a sink is (re)attached — i.e. on preset load /
    /// graph rebuild — so a new decode session starts clean. Default true.
    pub reset_on_init: bool,
}

impl Default for EventStoreParams {
    fn default() -> Self {
        Self {
            kind: String::new(),
            reset_on_init: true,
        }
    }
}

pub struct EventStore {
    params: EventStoreParams,
    sink: Option<Arc<dyn DecoderSink>>,
    /// Bytes consumed but not yet `\n`-terminated — carries a partial line
    /// across ticks.
    partial: Vec<u8>,
    /// Complete lines awaiting drain on the browser (no-sink) path.
    ready: Vec<Vec<u8>>,
}

impl EventStore {
    #[must_use]
    pub fn new(params: EventStoreParams) -> Self {
        Self {
            params,
            sink: None,
            partial: Vec::new(),
            ready: Vec::new(),
        }
    }

    /// Wire the server-side store sink. Resets the kind first (when
    /// `reset_on_init`) so a freshly loaded/rebuilt graph starts clean —
    /// this is the natural "fresh decode session" point, since the runtime
    /// attaches sinks right after load.
    pub fn attach_store(&mut self, sink: Arc<dyn DecoderSink>) {
        if self.params.reset_on_init {
            sink.reset(&self.params.kind);
        }
        self.sink = Some(sink);
    }

    /// Drain complete JSON lines buffered on the no-sink (browser) path,
    /// for the host to POST to `/api/decodes/:kind`.
    pub fn drain_events(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.ready)
    }

    /// Forward one complete line: parse as JSON (lenient — non-JSON wraps
    /// as a string so nothing is dropped) and emit, or buffer for drain.
    fn forward(&mut self, line: Vec<u8>) {
        if let Some(sink) = &self.sink {
            let data = std::str::from_utf8(&line)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                .unwrap_or_else(|| {
                    serde_json::Value::String(String::from_utf8_lossy(&line).into())
                });
            sink.emit(&self.params.kind, data);
        } else {
            self.ready.push(line);
        }
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for EventStore {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "EventStore",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::Events,
            }],
            outputs: &[],
            params: &[
                ParamSpec {
                    key: "kind",
                    label: "Store kind",
                    kind: ParamKind::Text { default: "" },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Decoder-store kind this sink feeds (adsb / aprs / ft8 / signals / …). Set by env_split from the ui:<kind> sink name; the store folds records under this key per its policy.",
                },
                ParamSpec {
                    key: "reset_on_init",
                    label: "Reset on load",
                    kind: ParamKind::Toggle { default: true },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Clear this kind in the store when the graph (re)loads, so a new decode session starts clean.",
                },
            ],
            ai_notes: "Terminal sink for decoder `events` ports — feeds the server-side DecoderStore (WS-replicated to the browser, REST snapshot for MCP). Replaces WsBridgeTxEvents for decoder ui:<kind> sinks. Placement::Either: node writes the store directly; browser buffers for the host to POST.",
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(src_port) = io.inputs.iter().find(|p| p.name == "in") else {
            return Ok(Work::new());
        };
        let InBuf::Events(bytes) = src_port.buf else {
            return Ok(Work::new());
        };

        let n = bytes.len();
        for &b in bytes {
            if b == b'\n' {
                let done = std::mem::take(&mut self.partial);
                if !done.is_empty() {
                    self.forward(done);
                }
            } else {
                self.partial.push(b);
            }
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        Ok(w)
    }
}

impl BlockFactory for EventStore {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: EventStoreParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(EventStore::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{DecoderSink, EventStore, EventStoreParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutputPort, PortMeta};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct CapturingSink {
        emits: Mutex<Vec<(String, serde_json::Value)>>,
        resets: Mutex<Vec<String>>,
    }
    impl DecoderSink for CapturingSink {
        fn emit(&self, kind: &str, data: serde_json::Value) {
            self.emits.lock().unwrap().push((kind.to_string(), data));
        }
        fn reset(&self, kind: &str) {
            self.resets.lock().unwrap().push(kind.to_string());
        }
    }

    fn block(kind: &str, reset_on_init: bool) -> EventStore {
        EventStore::new(EventStoreParams {
            kind: kind.to_string(),
            reset_on_init,
        })
    }

    fn feed(b: &mut EventStore, bytes: &[u8]) {
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::Events(bytes),
        }];
        let mut out: [OutputPort; 0] = [];
        let _ = &mut out;
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut [] as &mut [OutputPort],
        };
        let w = b.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], bytes.len());
    }

    #[test]
    fn attach_resets_then_emits_parsed_json() {
        let sink = Arc::new(CapturingSink::default());
        let mut b = block("adsb", true);
        b.attach_store(sink.clone());
        // reset fired on attach (fresh session).
        assert_eq!(*sink.resets.lock().unwrap(), vec!["adsb".to_string()]);

        feed(&mut b, b"{\"icao\":\"abc123\",\"alt\":1000}\n");
        let emits = sink.emits.lock().unwrap();
        assert_eq!(emits.len(), 1);
        assert_eq!(emits[0].0, "adsb");
        assert_eq!(emits[0].1["icao"], "abc123");
        assert_eq!(emits[0].1["alt"], 1000);
    }

    #[test]
    fn no_reset_when_disabled() {
        let sink = Arc::new(CapturingSink::default());
        let mut b = block("ft8", false);
        b.attach_store(sink.clone());
        assert!(sink.resets.lock().unwrap().is_empty());
    }

    #[test]
    fn partial_line_carries_across_ticks() {
        let sink = Arc::new(CapturingSink::default());
        let mut b = block("aprs", false);
        b.attach_store(sink.clone());
        feed(&mut b, b"{\"call\":\"N0");
        assert!(sink.emits.lock().unwrap().is_empty(), "no newline yet");
        feed(&mut b, b"CALL\"}\n");
        let emits = sink.emits.lock().unwrap();
        assert_eq!(emits.len(), 1);
        assert_eq!(emits[0].1["call"], "N0CALL");
    }

    #[test]
    fn non_json_line_wraps_as_string_not_dropped() {
        let sink = Arc::new(CapturingSink::default());
        let mut b = block("packet", false);
        b.attach_store(sink.clone());
        feed(&mut b, b"AFSK1200: raw frame\n");
        let emits = sink.emits.lock().unwrap();
        assert_eq!(emits.len(), 1);
        assert_eq!(emits[0].1, "AFSK1200: raw frame");
    }

    #[test]
    fn without_sink_lines_buffer_for_drain() {
        let mut b = block("adsb", true);
        feed(&mut b, b"{\"a\":1}\n{\"b\":2}\n");
        let drained = b.drain_events();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0], b"{\"a\":1}");
        assert!(b.drain_events().is_empty(), "drained once");
    }
}
