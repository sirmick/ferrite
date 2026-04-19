//! `WsBridge` — placeholder pair for wires that cross the
//! environment boundary.
//!
//! A flowgraph is one doc with per-block `placement`. Wires whose
//! endpoints straddle that boundary (e.g. server-side `Channelizer →`
//! browser-side `FmDemod`) are auto-split by the scheduler into a
//! `WsBridgeTx` on the producing side and a `WsBridgeRx` on the
//! consuming side, joined by a WebSocket stream that carries the
//! sample frames. Both ends share a `stream_id` so the transport
//! knows which WS channel belongs to which bridge.
//!
//! ### Scope of this commit
//!
//! This is the **block-type placeholder only** — the schedules and the
//! validator need to know these block types exist before auto-insertion
//! lands in M2. `process()` is a no-op: [`WsBridgeTx`] drops its input,
//! [`WsBridgeRx`] produces nothing. Real encode/send/recv/decode logic
//! arrives with the server preset-load commit in M2.
//!
//! Only `IqF32` is wired up today — it's the port type every preset in
//! the repo currently uses at the env boundary (`Channelizer.out`,
//! `FmDemod.in`). `RealF32`, `FftU8`, etc. land as siblings when a
//! preset needs them.

use std::sync::Arc;

use anyhow::Result;
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, ParamKind, ParamSpec, Placement, PortSpec,
    PortType, Work,
};

/// Transport contract for [`WsBridgeTx`]. The runtime constructs a sink
/// on the session side (today that's the WS frame encoder + broadcast
/// channel in `ferrited::session`) and hands it to the block via
/// [`WsBridgeTx::attach_sink`] after construction but before `init`
/// runs. Every [`WsBridgeTx::process`] call then forwards its input
/// slice through this handle.
///
/// Framing, `seq` counters, payload-type tagging, and the actual WS
/// send all live on the implementation side — the block stays ignorant
/// of the protocol so wire-format changes don't touch the DSP crate.
pub trait IqBridgeSink: Send + Sync {
    /// Push one block of `IqF32` samples tagged with a bridge-pair
    /// `stream_id`. Implementations are expected to be lossy-latest or
    /// backpressure-free — the block itself does not await.
    fn push_iq_f32(&self, stream_id: u32, samples: &[Complex<f32>]);
}

/// Stream identifier carried on both ends of a bridge pair. Unique
/// within a graph; the server allocates it at preset-load time.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct WsBridgeParams {
    pub stream_id: u32,
}

/// Stream-id param schema — inlined into both bridge specs below. The
/// transport id is fixed once a bridge pair is wired; a reconfigure
/// that changes it is really a re-plumb, not a knob.
const STREAM_ID_PARAM: ParamSpec = ParamSpec {
    key: "stream_id",
    label: "Stream ID",
    kind: ParamKind::Range {
        min: 0.0,
        max: 4_294_967_295.0,
        step: 1.0,
        default: 0.0,
        unit: "",
    },
    mutable_while_streaming: false,
};

// ---------------------------------------------------------------------------
// Tx — server-side egress. Accepts samples, will marshal to the WS
// stream. Native-only: a WASM block never sends over a WS to itself.
// ---------------------------------------------------------------------------

pub struct WsBridgeTx {
    params: WsBridgeParams,
    sink: Option<Arc<dyn IqBridgeSink>>,
}

impl WsBridgeTx {
    #[must_use]
    pub const fn new(params: WsBridgeParams) -> Self {
        Self { params, sink: None }
    }

    #[must_use]
    pub const fn stream_id(&self) -> u32 {
        self.params.stream_id
    }

    /// Wire a transport sink. The runtime calls this after constructing
    /// the block but before `init`; `process` pushes every arriving IQ
    /// block through the sink. Called once — later calls overwrite.
    pub fn attach_sink(&mut self, sink: Arc<dyn IqBridgeSink>) {
        self.sink = Some(sink);
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for WsBridgeTx {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "WsBridgeTx",
            placement: Placement::NativeOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[],
            params: &[STREAM_ID_PARAM],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let mut w = Work::new();
        if let Some(port) = io.inputs.iter().find(|p| p.name == "in") {
            if let Some(slice) = port.as_iq_f32() {
                if let Some(sink) = &self.sink {
                    sink.push_iq_f32(self.params.stream_id, slice);
                }
                // Consume whatever arrived regardless of sink presence:
                // an unwired bridge still needs to relieve upstream
                // back-pressure, and tests without a sink must not
                // deadlock the scheduler.
                w.consumed[0] = slice.len();
            }
        }
        Ok(w)
    }
}

impl BlockFactory for WsBridgeTx {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: WsBridgeParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(WsBridgeTx::new(p)))
    }
}

// ---------------------------------------------------------------------------
// Rx — browser-side ingress. Emits samples it received from the WS
// stream. WASM-only.
// ---------------------------------------------------------------------------

pub struct WsBridgeRx {
    params: WsBridgeParams,
}

impl WsBridgeRx {
    #[must_use]
    pub const fn new(params: WsBridgeParams) -> Self {
        Self { params }
    }

    #[must_use]
    pub const fn stream_id(&self) -> u32 {
        self.params.stream_id
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for WsBridgeRx {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "WsBridgeRx",
            placement: Placement::WasmOnly,
            inputs: &[],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[STREAM_ID_PARAM],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, _io: &mut BlockIo<'_>) -> Result<Work> {
        // Placeholder: no transport yet, so produce nothing. Once M2
        // lands, this fills the output buffer from decoded WS frames.
        Ok(Work::new())
    }
}

impl BlockFactory for WsBridgeRx {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: WsBridgeParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(WsBridgeRx::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{IqBridgeSink, WsBridgeParams, WsBridgeRx, WsBridgeTx};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta, Work};
    use num_complex::Complex;
    use std::sync::{Arc, Mutex};

    #[test]
    fn tx_spec_is_native_only_iq_in() {
        let s = WsBridgeTx::spec();
        assert_eq!(s.type_name, "WsBridgeTx");
        assert!(matches!(s.placement, crate::block::Placement::NativeOnly));
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.outputs.len(), 0);
    }

    #[test]
    fn rx_spec_is_wasm_only_iq_out() {
        let s = WsBridgeRx::spec();
        assert_eq!(s.type_name, "WsBridgeRx");
        assert!(matches!(s.placement, crate::block::Placement::WasmOnly));
        assert_eq!(s.inputs.len(), 0);
        assert_eq!(s.outputs.len(), 1);
    }

    #[test]
    fn tx_consumes_all_input_samples() {
        let mut tx = WsBridgeTx::new(WsBridgeParams { stream_id: 7 });
        let input = vec![Complex::new(1.0_f32, 0.0); 32];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&input),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut [],
        };
        let w: Work = tx.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], 32);
        assert_eq!(tx.stream_id(), 7);
    }

    #[test]
    fn rx_produces_nothing_until_transport_lands() {
        let mut rx = WsBridgeRx::new(WsBridgeParams { stream_id: 3 });
        let mut out = vec![Complex::new(0.0_f32, 0.0); 16];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut [],
            outputs: &mut outputs,
        };
        let w: Work = rx.process(&mut io).unwrap();
        assert_eq!(w.produced[0], 0);
    }

    /// Captures every `push_iq_f32` call so the test can assert the
    /// block forwarded the right `(stream_id, sample_count)` tuples.
    #[derive(Default)]
    struct CapturingSink {
        calls: Mutex<Vec<(u32, usize)>>,
    }

    impl IqBridgeSink for CapturingSink {
        fn push_iq_f32(&self, stream_id: u32, samples: &[Complex<f32>]) {
            self.calls.lock().unwrap().push((stream_id, samples.len()));
        }
    }

    #[test]
    fn tx_forwards_samples_to_attached_sink() {
        let sink = Arc::new(CapturingSink::default());
        let mut tx = WsBridgeTx::new(WsBridgeParams { stream_id: 1234 });
        tx.attach_sink(sink.clone());

        let input = vec![Complex::new(0.5_f32, -0.25); 48];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&input),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut [],
        };
        let w: Work = tx.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], 48);
        let calls = sink.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], (1234, 48));
    }

    #[test]
    fn tx_without_sink_still_drains_upstream() {
        let mut tx = WsBridgeTx::new(WsBridgeParams { stream_id: 9 });
        let input = vec![Complex::new(1.0_f32, 0.0); 16];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&input),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut [],
        };
        let w: Work = tx.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], 16);
    }
}
