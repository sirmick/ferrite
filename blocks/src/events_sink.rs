//! `EventsSink` — terminal for an `events` port.
//!
//! Accepts newline-delimited JSON on its input and buffers one event
//! per entry. Tests and harnesses drain the buffer via
//! [`EventsSink::drain_events`]; a real-browser host will layer a
//! `postMessage` forwarder on top in a later commit.
//!
//! The decoupled "buffer then drain" design keeps this block transport-
//! agnostic: native CLI, vitest Node host, and the future browser
//! runtime all see the same Rust API. The runtime downcasts via
//! [`crate::block::AsAny`] to reach `drain_events` after a tick.

use anyhow::Result;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InBuf, InitCtx, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct EventsSinkParams {
    /// Maximum buffered events before the oldest are dropped.
    /// Zero means unbounded; tests stick to defaults.
    pub capacity: u32,
}

impl Default for EventsSinkParams {
    fn default() -> Self {
        Self { capacity: 1_024 }
    }
}

pub struct EventsSink {
    params: EventsSinkParams,
    /// Bytes consumed so far but not yet terminated by `\n`. Lets events
    /// that straddle `process()` call boundaries reassemble cleanly.
    partial: Vec<u8>,
    /// Complete JSON event lines (no trailing newline), oldest first.
    ready: Vec<Vec<u8>>,
    /// Count of events dropped due to capacity — surfaced to tests as a
    /// health signal.
    dropped: u64,
}

impl EventsSink {
    #[must_use]
    pub fn new(params: EventsSinkParams) -> Self {
        Self {
            params,
            partial: Vec::new(),
            ready: Vec::new(),
            dropped: 0,
        }
    }

    /// Drain every buffered event. Each returned `Vec<u8>` is one
    /// UTF-8 JSON object without its trailing newline.
    #[must_use]
    pub fn drain_events(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.ready)
    }

    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.ready.len()
    }

    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }

    fn push_ready(&mut self, line: Vec<u8>) {
        let cap = self.params.capacity as usize;
        if cap > 0 && self.ready.len() >= cap {
            // Drop oldest to make room — bounded memory even if nobody
            // ever drains.
            let _ = self.ready.remove(0);
            self.dropped = self.dropped.saturating_add(1);
        }
        self.ready.push(line);
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for EventsSink {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "EventsSink",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::Events,
            }],
            outputs: &[],
            params: &[ParamSpec {
                key: "capacity",
                label: "Buffer capacity",
                kind: ParamKind::Range {
                    min: 0.0,
                    max: 1_000_000.0,
                    step: 1.0,
                    default: 1_024.0,
                    unit: "events",
                },
                reconfig_scope: ReconfigureScope::SelfBlock,
                ai_notes: "Ring capacity in events. Larger = more headroom if the consumer falls behind; smaller = lower memory.",
            }],
            ai_notes: "Terminal sink for `events` ports — buffers structured events (lock status, decoder messages, frame metadata) into a ring the runtime drains and surfaces via `tail decoder` / decoder logs.",
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
                    self.push_ready(done);
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

impl BlockFactory for EventsSink {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: EventsSinkParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(EventsSink::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{EventsSink, EventsSinkParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, PortMeta};

    fn feed(sink: &mut EventsSink, bytes: &[u8]) {
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::Events(bytes),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut [],
        };
        let _ = sink.process(&mut io).unwrap();
    }

    #[test]
    fn collects_complete_lines() {
        let mut s = EventsSink::new(EventsSinkParams::default());
        feed(&mut s, b"{\"a\":1}\n{\"b\":2}\n");
        let got = s.drain_events();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], b"{\"a\":1}");
        assert_eq!(got[1], b"{\"b\":2}");
    }

    #[test]
    fn reassembles_split_across_calls() {
        let mut s = EventsSink::new(EventsSinkParams::default());
        feed(&mut s, b"{\"a\"");
        assert_eq!(s.pending_count(), 0);
        feed(&mut s, b":1}\n");
        let got = s.drain_events();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0], b"{\"a\":1}");
    }

    #[test]
    fn capacity_zero_is_unbounded() {
        let mut s = EventsSink::new(EventsSinkParams { capacity: 0 });
        let payload = b"{}\n".repeat(10_000);
        feed(&mut s, &payload);
        assert_eq!(s.pending_count(), 10_000);
        assert_eq!(s.dropped(), 0);
    }

    #[test]
    fn capacity_drops_oldest() {
        let mut s = EventsSink::new(EventsSinkParams { capacity: 3 });
        feed(
            &mut s,
            b"{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n{\"n\":4}\n{\"n\":5}\n",
        );
        let got = s.drain_events();
        assert_eq!(got.len(), 3);
        // Oldest two dropped — we have 3, 4, 5.
        assert_eq!(got[0], b"{\"n\":3}");
        assert_eq!(got[2], b"{\"n\":5}");
        assert_eq!(s.dropped(), 2);
    }

    #[test]
    fn empty_lines_dont_produce_events() {
        let mut s = EventsSink::new(EventsSinkParams::default());
        feed(&mut s, b"\n\n\n");
        assert_eq!(s.pending_count(), 0);
    }
}
