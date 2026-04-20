//! `WsBridge` — paired blocks for wires that cross the environment
//! boundary.
//!
//! A flowgraph is one doc with per-block `placement`. Wires whose
//! endpoints straddle that boundary (e.g. server-side `Channelizer →`
//! browser-side `FmDemod`) are auto-split by the scheduler into a
//! `WsBridgeTx{…}` on the producing side and a `WsBridgeRx` on the
//! consuming side, joined by a WebSocket stream that carries the
//! sample frames. Both ends share a `stream_id` so the transport
//! knows which WS channel belongs to which bridge.
//!
//! There is one Tx block per port type ([`WsBridgeTx`] for `IqF32`,
//! [`WsBridgeTxFftU8`] for `FftU8`) because the block framework is
//! statically typed on ports. They all share the **same**
//! [`BridgeSink`] trait — each Tx encodes its input into bytes, tags
//! it with a [`BridgePayloadType`], and pushes through the sink. The
//! transport implementation (framing, seq counters, the WS hop)
//! doesn't care which port type the bytes came from.

use std::sync::Arc;

use anyhow::Result;
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InBuf, InitCtx, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

/// Payload tag carried on every bridge push. The transport uses this
/// to pick the wire `payload_type` byte; the `#[repr(u8)]` discriminants
/// match the wire values in `docs/02-protocol.md` so the server-side
/// sink doesn't need a translation table. Block crate owns this enum
/// because blocks must not depend on the server crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BridgePayloadType {
    FftU8 = 0x01,
    IqF32 = 0x11,
}

/// Transport contract shared by every `WsBridgeTx*` block. The runtime
/// constructs one sink per preset (today that's the WS frame encoder +
/// broadcast channel in `ferrited::session`) and hands the same
/// `Arc<dyn BridgeSink>` to every Tx block via `attach_sink`. Each Tx
/// block then encodes its input into bytes and pushes through this
/// handle, tagged with the appropriate [`BridgePayloadType`].
///
/// Framing, `seq` counters, and the WS send all live on the
/// implementation side — blocks stay ignorant of the wire protocol.
pub trait BridgeSink: Send + Sync {
    /// Push one block of pre-encoded bytes tagged with a bridge-pair
    /// `stream_id` and the payload type. Implementations are expected
    /// to be lossy-latest or backpressure-free — the block itself does
    /// not await.
    fn push(&self, stream_id: u32, payload_type: BridgePayloadType, bytes: &[u8]);
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
    // Bridge wiring is fixed per graph load — re-plumbing the WS pair
    // is a source-level restart.
    reconfig_scope: ReconfigureScope::SourceRestart,
};

// ---------------------------------------------------------------------------
// Tx (IqF32) — server-side egress for baseband IQ samples. Accepts
// `Complex<f32>`, encodes as interleaved little-endian floats, pushes.
// Native-only: a WASM block never sends over a WS to itself.
// ---------------------------------------------------------------------------

pub struct WsBridgeTx {
    params: WsBridgeParams,
    sink: Option<Arc<dyn BridgeSink>>,
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
    pub fn attach_sink(&mut self, sink: Arc<dyn BridgeSink>) {
        self.sink = Some(sink);
    }

    /// Encode a slice of complex samples as interleaved little-endian
    /// `f32` bytes — the wire format for `IqF32`. Allocates fresh; the
    /// hot path could reuse a scratch buffer if profiling shows it
    /// matters.
    fn encode_iq_f32(samples: &[Complex<f32>]) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples.len() * 8);
        for c in samples {
            out.extend_from_slice(&c.re.to_le_bytes());
            out.extend_from_slice(&c.im.to_le_bytes());
        }
        out
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
                    let bytes = Self::encode_iq_f32(slice);
                    sink.push(self.params.stream_id, BridgePayloadType::IqF32, &bytes);
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

// ---------------------------------------------------------------------------
// Tx (FftU8) — server-side egress for log-magnitude spectrum bytes.
// The payload is already `u8` on the wire, so the block just forwards
// the slice to the same `BridgeSink` with the `FftU8` tag.
// ---------------------------------------------------------------------------

pub struct WsBridgeTxFftU8 {
    params: WsBridgeParams,
    sink: Option<Arc<dyn BridgeSink>>,
}

impl WsBridgeTxFftU8 {
    #[must_use]
    pub const fn new(params: WsBridgeParams) -> Self {
        Self { params, sink: None }
    }

    #[must_use]
    pub const fn stream_id(&self) -> u32 {
        self.params.stream_id
    }

    /// Wire a transport sink — mirrors [`WsBridgeTx::attach_sink`]. The
    /// runtime calls this once after construction and before `init`;
    /// `process` forwards every arriving FftU8 slice through the sink.
    pub fn attach_sink(&mut self, sink: Arc<dyn BridgeSink>) {
        self.sink = Some(sink);
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for WsBridgeTxFftU8 {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "WsBridgeTxFftU8",
            placement: Placement::NativeOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::FftU8,
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
            if let InBuf::FftU8(slice) = &port.buf {
                if let Some(sink) = &self.sink {
                    sink.push(self.params.stream_id, BridgePayloadType::FftU8, slice);
                }
                w.consumed[0] = slice.len();
            }
        }
        Ok(w)
    }
}

impl BlockFactory for WsBridgeTxFftU8 {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: WsBridgeParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(WsBridgeTxFftU8::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BridgePayloadType, BridgeSink, WsBridgeParams, WsBridgeRx, WsBridgeTx, WsBridgeTxFftU8,
    };
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta, Work};
    use num_complex::Complex;
    use std::sync::{Arc, Mutex};

    /// Captures every `push` call so tests can assert the block
    /// forwarded the right `(stream_id, payload_type, bytes)` tuples.
    #[derive(Default)]
    struct CapturingSink {
        calls: Mutex<Vec<(u32, BridgePayloadType, Vec<u8>)>>,
    }

    impl BridgeSink for CapturingSink {
        fn push(&self, stream_id: u32, payload_type: BridgePayloadType, bytes: &[u8]) {
            self.calls
                .lock()
                .unwrap()
                .push((stream_id, payload_type, bytes.to_vec()));
        }
    }

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

    #[test]
    fn iq_tx_encodes_complex_as_le_interleaved_floats() {
        let sink = Arc::new(CapturingSink::default());
        let mut tx = WsBridgeTx::new(WsBridgeParams { stream_id: 1234 });
        tx.attach_sink(sink.clone());

        let input = vec![Complex::new(1.0_f32, 2.0), Complex::new(3.0, 4.0)];
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
        assert_eq!(w.consumed[0], 2);

        let calls = sink.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (id, pt, bytes) = &calls[0];
        assert_eq!(*id, 1234);
        assert_eq!(*pt, BridgePayloadType::IqF32);
        assert_eq!(bytes.len(), 16);
        assert_eq!(&bytes[0..4], &1.0_f32.to_le_bytes());
        assert_eq!(&bytes[4..8], &2.0_f32.to_le_bytes());
        assert_eq!(&bytes[8..12], &3.0_f32.to_le_bytes());
        assert_eq!(&bytes[12..16], &4.0_f32.to_le_bytes());
    }

    #[test]
    fn iq_tx_without_sink_still_drains_upstream() {
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

    #[test]
    fn fft_tx_spec_is_native_only_fft_u8_in() {
        let s = WsBridgeTxFftU8::spec();
        assert_eq!(s.type_name, "WsBridgeTxFftU8");
        assert!(matches!(s.placement, crate::block::Placement::NativeOnly));
        assert_eq!(s.inputs.len(), 1);
        assert!(matches!(
            s.inputs[0].port_type,
            crate::block::PortType::FftU8
        ));
        assert_eq!(s.outputs.len(), 0);
    }

    #[test]
    fn fft_tx_forwards_bytes_as_fft_u8_tag() {
        let sink = Arc::new(CapturingSink::default());
        let mut tx = WsBridgeTxFftU8::new(WsBridgeParams { stream_id: 1 });
        tx.attach_sink(sink.clone());

        let input: Vec<u8> = (0..32).collect();
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::FftU8(&input),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut [],
        };
        let w: Work = tx.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], 32);

        let calls = sink.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let (id, pt, bytes) = &calls[0];
        assert_eq!(*id, 1);
        assert_eq!(*pt, BridgePayloadType::FftU8);
        assert_eq!(bytes, &input);
    }

    #[test]
    fn one_sink_instance_serves_both_tx_block_types() {
        // The point of the unified trait: a single `Arc<dyn BridgeSink>`
        // can be attached to every Tx block in a preset regardless of
        // port type, and the sink tells them apart by the payload_type
        // tag on each push.
        let sink = Arc::new(CapturingSink::default());
        let sink_dyn: Arc<dyn BridgeSink> = sink.clone();

        let mut iq_tx = WsBridgeTx::new(WsBridgeParams { stream_id: 1000 });
        iq_tx.attach_sink(Arc::clone(&sink_dyn));
        let mut fft_tx = WsBridgeTxFftU8::new(WsBridgeParams { stream_id: 1 });
        fft_tx.attach_sink(Arc::clone(&sink_dyn));

        let iq_input = vec![Complex::new(0.5_f32, -0.5)];
        let mut iq_inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&iq_input),
        }];
        iq_tx
            .process(&mut BlockIo {
                inputs: &mut iq_inputs,
                outputs: &mut [],
            })
            .unwrap();

        let fft_input: Vec<u8> = vec![1, 2, 3];
        let mut fft_inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::FftU8(&fft_input),
        }];
        fft_tx
            .process(&mut BlockIo {
                inputs: &mut fft_inputs,
                outputs: &mut [],
            })
            .unwrap();

        let calls = sink.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, 1000);
        assert_eq!(calls[0].1, BridgePayloadType::IqF32);
        assert_eq!(calls[1].0, 1);
        assert_eq!(calls[1].1, BridgePayloadType::FftU8);
    }
}
