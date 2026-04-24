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
//! [`BridgeSink`] trait — each Tx encodes its input as bytes, wraps it
//! in the appropriate [`Frame`] variant with `seq = 0` and
//! `timestamp_ns = 0`, and pushes through the sink. The sink stamps the
//! envelope and serializes with postcard; the block stays ignorant of
//! the wire protocol.
//!
//! ### Rx-side transport split
//!
//! [`WsBridgeRx`] does **not** own the WebSocket. The browser runner
//! keeps a single multiplexed `FrameClient` on the JS side; at preset
//! load it subscribes each Rx block's `stream_id` and routes decoded
//! `IqF32` payloads into the block's internal ring via the
//! wasm-bindgen `pushIq(blockId, floats)` method. The block's
//! `process` drains that ring onto its typed output at tick time.
//!
//! Keeping the transport on JS keeps the Rust code portable — no
//! `web_sys` / `js_sys` dependency creeps into `ferrite-blocks`, and
//! native tests drive the block by calling [`WsBridgeRx::push`]
//! directly. See D22 for the history: this Rx absorbed the old
//! `WsIqSource` block when the bridge transport was unified.

use std::sync::Arc;

use anyhow::Result;
use num_complex::Complex;
use serde::Deserialize;

use crate::{
    block::{
        Block, BlockFactory, BlockIo, BlockSpec, InBuf, InitCtx, OutputPort, ParamKind, ParamSpec,
        Placement, PortSpec, PortType, ReconfigureScope, Work,
    },
    frame::Frame,
    spsc_ring::IqRing,
};

/// One `Frame::JsonEvent` per complete line — see `WsBridgeTxEvents`.
const EVENTS_DELIMITER: u8 = b'\n';

/// Transport contract shared by every `WsBridgeTx*` block. The runtime
/// constructs one sink per preset (today that's the postcard encoder +
/// broadcast channel in `ferrited::session`) and hands the same
/// `Arc<dyn BridgeSink>` to every Tx block via `attach_sink`.
///
/// `seq` and `timestamp_ns` are sink-owned: the block emits a
/// zero-filled envelope and the sink overwrites both before
/// serializing. Framing and the WS send live on the implementation
/// side — blocks stay ignorant of the wire protocol.
pub trait BridgeSink: Send + Sync {
    /// Submit a frame for transport. Implementations are expected to be
    /// lossy-latest or backpressure-free — the block itself does not
    /// await. `seq`/`timestamp_ns` on the incoming frame are ignored
    /// and rewritten by the sink.
    fn push(&self, frame: Frame);
}

/// Stream identifier carried on both ends of a bridge pair. Unique
/// within a graph; the server allocates it at preset-load time.
///
/// `min_samples_per_frame` batches per-tick emissions so the wire sees
/// ~30–60 frames/sec rather than one per scheduler tick (2.5 kHz).
/// Each WS message has fixed overhead (framing + postcard + dispatch);
/// at 2500 fps the browser can't drain fast enough and frames pile up
/// in the subscriber queue. Setting this to, e.g., 4096 IQ samples at
/// 250 kS/s gives ~60 fps on the wire, 2–3 kB per message — plenty of
/// headroom for the browser runner to consume.
///
/// Zero (the default) disables batching: every tick emits whatever
/// samples arrived, preserving the original behaviour for tests that
/// tick by hand.
#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct WsBridgeParams {
    pub stream_id: u32,
    pub min_samples_per_frame: usize,
}

/// `WsBridgeTxFftU8` params — adds a `frame_size` on top of the shared
/// stream id. One `Frame::FftU8` is pushed per `frame_size` bytes of
/// input; partial trailing bytes stay on the wire for the next tick.
///
/// The default matches [`crate::log_mag_u8::LogMagU8`]'s default size;
/// `env_split` reads the upstream producer's `size` param at insertion
/// time so the usual zero-config case "just works".
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct WsBridgeFftU8Params {
    pub stream_id: u32,
    pub frame_size: usize,
}

impl Default for WsBridgeFftU8Params {
    fn default() -> Self {
        Self {
            stream_id: 0,
            frame_size: 4096,
        }
    }
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

/// Narrow the `u32` param back to the wire's `u16` stream id. `u16` is
/// plenty (64k streams) and matches the `Frame` schema; the `u32` on
/// the params struct just avoids a JSON-schema oddity around max.
#[allow(clippy::cast_possible_truncation)]
const fn stream_id_u16(stream_id: u32) -> u16 {
    if stream_id > u16::MAX as u32 {
        u16::MAX
    } else {
        stream_id as u16
    }
}

// ---------------------------------------------------------------------------
// Tx (IqF32) — server-side egress for baseband IQ samples. Accepts
// `Complex<f32>`, encodes as interleaved little-endian floats, pushes.
// Native-only: a WASM block never sends over a WS to itself.
// ---------------------------------------------------------------------------

pub struct WsBridgeTx {
    params: WsBridgeParams,
    sink: Option<Arc<dyn BridgeSink>>,
    /// Accumulator for `min_samples_per_frame` batching. Filled on each
    /// `process` call; flushed when it reaches the threshold. Empty when
    /// batching is disabled (`min_samples_per_frame == 0`).
    batch: Vec<Complex<f32>>,
    /// Last input-port rate observed via `InitCtx.input_rate`. Stamped
    /// onto every emitted `Frame::IqF32` so the receiving `WsBridgeRx`
    /// on the other side of the WS boundary learns the actual runtime
    /// rate rather than relying on a preset-time guess.
    input_rate_hz: f64,
}

impl WsBridgeTx {
    #[must_use]
    pub fn new(params: WsBridgeParams) -> Self {
        Self {
            params,
            sink: None,
            batch: Vec::with_capacity(params.min_samples_per_frame),
            input_rate_hz: 0.0,
        }
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

    /// Emit the buffered samples as one frame. Clears the batch.
    fn flush(&mut self, samples: &[Complex<f32>]) {
        let Some(sink) = &self.sink else {
            return;
        };
        if samples.is_empty() {
            return;
        }
        let payload = Self::encode_iq_f32(samples);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let sample_rate_hz = self.input_rate_hz.round().max(0.0) as u32;
        sink.push(Frame::IqF32 {
            stream_id: stream_id_u16(self.params.stream_id),
            seq: 0,
            timestamp_ns: 0,
            sample_rate_hz,
            payload,
        });
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

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 {
                self.input_rate_hz = rate;
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        // Source retuned — pick up the new rate so outgoing frames
        // advertise the current runtime value to the far-side Rx.
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 {
                self.input_rate_hz = rate;
            }
        }
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let mut w = Work::new();
        if let Some(port) = io.inputs.iter().find(|p| p.name == "in") {
            if let Some(slice) = port.as_iq_f32() {
                if self.params.min_samples_per_frame == 0 {
                    // No batching: emit every tick.
                    self.flush(slice);
                } else {
                    self.batch.extend_from_slice(slice);
                    if self.batch.len() >= self.params.min_samples_per_frame {
                        // Take ownership of the batch buffer, emit, reset
                        // with the same capacity. One allocation per
                        // flush — negligible compared to the WS overhead
                        // it saves.
                        let drained = std::mem::replace(
                            &mut self.batch,
                            Vec::with_capacity(self.params.min_samples_per_frame),
                        );
                        self.flush(&drained);
                    }
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
// Rx — browser-side ingress. Holds an internal [`IqRing`]; the browser
// runner pushes decoded frames into it over the wasm-bindgen
// `pushIq` method, and `process` drains the ring onto the typed
// `IqF32` output at tick time. WASM-only. See module doc for the
// transport split.
// ---------------------------------------------------------------------------

/// Default ring capacity in **samples** (complex IQ pairs). 65 536
/// samples ≈ 650 ms at 100 kS/s narrowband — plenty of headroom for
/// scheduler jitter without adding meaningful latency.
const DEFAULT_RX_BUFFER_SAMPLES: usize = 65_536;
const DEFAULT_RX_BUFFER_SAMPLES_F64: f64 = 65_536.0;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct WsBridgeRxParams {
    /// `stream_id` the browser-side transport demultiplexes for this
    /// block. The runner registers a [`FrameClient`] subscription for
    /// this id on start and drops it on stop; the block itself just
    /// consumes samples pushed in via [`WsBridgeRx::push_interleaved`].
    ///
    /// [`FrameClient`]: https://ferrite.example/frame-client
    pub stream_id: u32,
    /// Ring capacity in complex samples. Power of two recommended.
    pub buffer_samples: usize,
    /// Nominal sample rate of the IQ stream arriving on `stream_id`.
    /// Populated by `env_split` from the producing block's declared
    /// output rate when the bridge is inserted — lets downstream
    /// rate-aware blocks (e.g. `RealF32Resamp`) read this through
    /// `InitCtx.input_rate("in")` and snap their ratio to the actual
    /// wire rate. Zero when the producer didn't declare a rate.
    pub sample_rate_hz: f64,
}

impl Default for WsBridgeRxParams {
    fn default() -> Self {
        Self {
            stream_id: 0,
            buffer_samples: DEFAULT_RX_BUFFER_SAMPLES,
            sample_rate_hz: 0.0,
        }
    }
}

const RX_BUFFER_SAMPLES_PARAM: ParamSpec = ParamSpec {
    key: "buffer_samples",
    label: "Buffer capacity",
    kind: ParamKind::Range {
        min: 1_024.0,
        max: 1_048_576.0,
        step: 1.0,
        default: DEFAULT_RX_BUFFER_SAMPLES_F64,
        unit: "samples",
    },
    reconfig_scope: ReconfigureScope::SourceRestart,
};

pub struct WsBridgeRx {
    params: WsBridgeRxParams,
    ring: IqRing,
    dropped_samples: u64,
}

impl WsBridgeRx {
    #[must_use]
    pub fn new(params: WsBridgeRxParams) -> Self {
        let ring = IqRing::new(params.buffer_samples);
        Self {
            params,
            ring,
            dropped_samples: 0,
        }
    }

    #[must_use]
    pub const fn stream_id(&self) -> u32 {
        self.params.stream_id
    }

    #[must_use]
    pub fn params(&self) -> &WsBridgeRxParams {
        &self.params
    }

    /// Cumulative count of samples dropped because the ring was full.
    /// The network clock wins over the flowgraph clock (losing IQ
    /// samples beats stalling the WS reader).
    #[must_use]
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples
    }

    /// Samples currently buffered (written but not yet emitted).
    #[must_use]
    pub fn buffered_samples(&self) -> usize {
        self.ring.available_read()
    }

    /// Push a batch of complex samples received from the transport.
    /// Samples that don't fit are counted into `dropped_samples`.
    pub fn push(&mut self, samples: &[Complex<f32>]) {
        let written = self.ring.write(samples);
        if written < samples.len() {
            self.dropped_samples += (samples.len() - written) as u64;
        }
    }

    /// Push a batch from an interleaved `[i0, q0, i1, q1, …]` float
    /// slice. Convenience over [`push`] for callers that receive WS
    /// frames as raw `Float32Array` payloads. Odd-length inputs drop
    /// the trailing half-sample.
    pub fn push_interleaved(&mut self, floats: &[f32]) {
        let pair_count = floats.len() / 2;
        let mut tmp = Vec::with_capacity(pair_count);
        for chunk in floats[..pair_count * 2].chunks_exact(2) {
            tmp.push(Complex::new(chunk[0], chunk[1]));
        }
        self.push(&tmp);
    }

    /// Record the sample rate advertised by the most recent incoming
    /// `Frame::IqF32`. Runner glue (server transport decoder, browser
    /// `FrameClient`) calls this on each frame that carries a non-zero
    /// `sample_rate_hz`. The stored rate feeds `output_rate_hz()`, which
    /// the runtime's `refresh_rates()` sweep uses to re-advertise the
    /// live rate to downstream blocks (resamp, demod) — so a runtime
    /// source-rate change propagates across the WS boundary without a
    /// full preset reconfigure.
    pub fn set_advertised_rate(&mut self, rate_hz: f64) {
        if rate_hz > 0.0 && rate_hz.is_finite() {
            self.params.sample_rate_hz = rate_hz;
        }
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
            params: &[STREAM_ID_PARAM, RX_BUFFER_SAMPLES_PARAM],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        self.ring.reset();
        self.dropped_samples = 0;
        Ok(())
    }

    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        // Stamped by `env_split::producer_output_rate_hz` at split
        // time — the rate the other half's producer declares. `None`
        // when that walk couldn't resolve a rate from the doc.
        if self.params.sample_rate_hz > 0.0 {
            Some(self.params.sample_rate_hz)
        } else {
            None
        }
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(out) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_iq_f32_mut)
        else {
            return Ok(Work::new());
        };
        let got = self.ring.read(out);
        let mut w = Work::new();
        w.produced[0] = got;
        Ok(w)
    }

    fn stop(&mut self) -> Result<()> {
        self.ring.reset();
        Ok(())
    }
}

impl BlockFactory for WsBridgeRx {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: WsBridgeRxParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(WsBridgeRx::new(p)))
    }
}

// ---------------------------------------------------------------------------
// Tx (FftU8) — server-side egress for log-magnitude spectrum bytes.
// The payload is already `u8` on the wire, so the block just forwards
// the slice to the same `BridgeSink` wrapped in an `FftU8` frame.
// ---------------------------------------------------------------------------

pub struct WsBridgeTxFftU8 {
    params: WsBridgeFftU8Params,
    sink: Option<Arc<dyn BridgeSink>>,
}

impl WsBridgeTxFftU8 {
    #[must_use]
    pub const fn new(params: WsBridgeFftU8Params) -> Self {
        Self { params, sink: None }
    }

    #[must_use]
    pub const fn stream_id(&self) -> u32 {
        self.params.stream_id
    }

    #[must_use]
    pub const fn frame_size(&self) -> usize {
        self.params.frame_size
    }

    /// Wire a transport sink — mirrors [`WsBridgeTx::attach_sink`]. The
    /// runtime calls this once after construction and before `init`;
    /// `process` forwards every arriving `FftU8` slice through the sink.
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
            params: &[
                STREAM_ID_PARAM,
                ParamSpec {
                    key: "frame_size",
                    label: "FFT frame size",
                    kind: ParamKind::EnumNumeric {
                        values: &[1024.0, 2048.0, 4096.0, 8192.0, 16384.0],
                        default: 4096.0,
                        unit: "bins",
                    },
                    // Changing the chunk size changes the wire framing —
                    // downstream consumers that pin the FFT size must
                    // re-init.
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
            ],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let mut w = Work::new();
        let frame_size = self.params.frame_size.max(1);
        if let Some(port) = io.inputs.iter().find(|p| p.name == "in") {
            if let InBuf::FftU8(slice) = &port.buf {
                let whole = slice.len() / frame_size;
                if whole > 0 {
                    if let Some(sink) = &self.sink {
                        for i in 0..whole {
                            let start = i * frame_size;
                            let end = start + frame_size;
                            sink.push(Frame::FftU8 {
                                stream_id: stream_id_u16(self.params.stream_id),
                                seq: 0,
                                timestamp_ns: 0,
                                payload: slice[start..end].to_vec(),
                            });
                        }
                    }
                    w.consumed[0] = whole * frame_size;
                }
            }
        }
        Ok(w)
    }
}

impl BlockFactory for WsBridgeTxFftU8 {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: WsBridgeFftU8Params = crate::block::deserialize_params(params)?;
        Ok(Box::new(WsBridgeTxFftU8::new(p)))
    }
}

// ---------------------------------------------------------------------------
// Tx (JsonEvent) — server-side egress for decoder `events` output. Accepts
// newline-delimited JSON on an `Events` input port and emits one
// `Frame::JsonEvent` per complete line, so each browser-side event is a
// clean parseable JSON object with its own `seq`.
// ---------------------------------------------------------------------------

pub struct WsBridgeTxEvents {
    params: WsBridgeParams,
    sink: Option<Arc<dyn BridgeSink>>,
    /// Bytes consumed so far but not yet terminated by `\n`. Events that
    /// straddle `process()` call boundaries reassemble here.
    partial: Vec<u8>,
}

impl WsBridgeTxEvents {
    #[must_use]
    pub const fn new(params: WsBridgeParams) -> Self {
        Self {
            params,
            sink: None,
            partial: Vec::new(),
        }
    }

    #[must_use]
    pub const fn stream_id(&self) -> u32 {
        self.params.stream_id
    }

    /// Wire a transport sink — mirrors [`WsBridgeTx::attach_sink`]. The
    /// runtime calls this once after construction and before `init`;
    /// `process` pushes one `JsonEvent` per complete line.
    pub fn attach_sink(&mut self, sink: Arc<dyn BridgeSink>) {
        self.sink = Some(sink);
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for WsBridgeTxEvents {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "WsBridgeTxEvents",
            placement: Placement::NativeOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::Events,
            }],
            outputs: &[],
            params: &[STREAM_ID_PARAM],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        self.partial.clear();
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let mut w = Work::new();
        let Some(port) = io.inputs.iter().find(|p| p.name == "in") else {
            return Ok(w);
        };
        let InBuf::Events(bytes) = port.buf else {
            return Ok(w);
        };
        let n = bytes.len();
        for &b in bytes {
            if b == EVENTS_DELIMITER {
                let done = std::mem::take(&mut self.partial);
                if !done.is_empty() {
                    if let Some(sink) = &self.sink {
                        sink.push(Frame::JsonEvent {
                            stream_id: stream_id_u16(self.params.stream_id),
                            seq: 0,
                            timestamp_ns: 0,
                            payload: done,
                        });
                    }
                }
            } else {
                self.partial.push(b);
            }
        }
        w.consumed[0] = n;
        Ok(w)
    }

    fn stop(&mut self) -> Result<()> {
        self.partial.clear();
        Ok(())
    }
}

impl BlockFactory for WsBridgeTxEvents {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: WsBridgeParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(WsBridgeTxEvents::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BridgeSink, WsBridgeFftU8Params, WsBridgeParams, WsBridgeRx, WsBridgeRxParams, WsBridgeTx,
        WsBridgeTxEvents, WsBridgeTxFftU8, DEFAULT_RX_BUFFER_SAMPLES,
    };
    use crate::{
        block::{Block, BlockIo, InBuf, InitCtx, InputPort, OutBuf, OutputPort, PortMeta, Work},
        frame::Frame,
    };
    use num_complex::Complex;
    use std::sync::{Arc, Mutex};

    /// Captures every `push` call so tests can assert the block
    /// forwarded the right Frame variants.
    #[derive(Default)]
    struct CapturingSink {
        calls: Mutex<Vec<Frame>>,
    }

    impl BridgeSink for CapturingSink {
        fn push(&self, frame: Frame) {
            self.calls.lock().unwrap().push(frame);
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
        let mut tx = WsBridgeTx::new(WsBridgeParams {
            stream_id: 7,
            ..Default::default()
        });
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
    fn rx_produces_nothing_when_ring_empty() {
        let mut rx = WsBridgeRx::new(WsBridgeRxParams {
            stream_id: 3,
            buffer_samples: 64,
            sample_rate_hz: 0.0,
        });
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
    fn rx_defaults_match_canonical_shape() {
        let p = WsBridgeRxParams::default();
        assert_eq!(p.stream_id, 0);
        assert_eq!(p.buffer_samples, DEFAULT_RX_BUFFER_SAMPLES);
    }

    #[test]
    fn rx_params_round_trip_through_json() {
        let value = serde_json::json!({ "stream_id": 1000, "buffer_samples": 1024 });
        let p: WsBridgeRxParams = serde_json::from_value(value).unwrap();
        assert_eq!(p.stream_id, 1000);
        assert_eq!(p.buffer_samples, 1024);
    }

    #[test]
    fn rx_pushed_samples_surface_on_output_port() {
        let mut rx = WsBridgeRx::new(WsBridgeRxParams {
            stream_id: 0,
            buffer_samples: 8,
            sample_rate_hz: 0.0,
        });
        rx.push(&[
            Complex::new(1.0, 2.0),
            Complex::new(3.0, 4.0),
            Complex::new(5.0, 6.0),
        ]);
        assert_eq!(rx.buffered_samples(), 3);

        let mut out_buf = [Complex::new(0.0, 0.0); 8];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut out_buf),
        }];
        let mut io = BlockIo {
            inputs: &mut [],
            outputs: &mut outputs,
        };
        let work = rx.process(&mut io).unwrap();
        assert_eq!(work.produced[0], 3);
        assert_eq!(
            out_buf[..3],
            [
                Complex::new(1.0, 2.0),
                Complex::new(3.0, 4.0),
                Complex::new(5.0, 6.0),
            ],
        );
        assert_eq!(rx.buffered_samples(), 0);
    }

    #[test]
    fn rx_push_tallies_overflow_as_dropped_samples() {
        let mut rx = WsBridgeRx::new(WsBridgeRxParams {
            stream_id: 0,
            buffer_samples: 2,
            sample_rate_hz: 0.0,
        });
        rx.push(&[
            Complex::new(1.0, 0.0),
            Complex::new(2.0, 0.0),
            Complex::new(3.0, 0.0),
        ]);
        assert_eq!(rx.dropped_samples(), 1);
        assert_eq!(rx.buffered_samples(), 2);
    }

    #[test]
    fn rx_push_interleaved_converts_floats_to_complex_pairs() {
        let mut rx = WsBridgeRx::new(WsBridgeRxParams {
            stream_id: 0,
            buffer_samples: 4,
            sample_rate_hz: 0.0,
        });
        rx.push_interleaved(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(rx.buffered_samples(), 2);

        let mut out_buf = [Complex::new(0.0, 0.0); 2];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut out_buf),
        }];
        let mut io = BlockIo {
            inputs: &mut [],
            outputs: &mut outputs,
        };
        rx.process(&mut io).unwrap();
        assert_eq!(out_buf, [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)]);
    }

    #[test]
    fn rx_push_interleaved_drops_trailing_half_sample() {
        let mut rx = WsBridgeRx::new(WsBridgeRxParams {
            stream_id: 0,
            buffer_samples: 4,
            sample_rate_hz: 0.0,
        });
        rx.push_interleaved(&[0.1, 0.2, 0.3]);
        assert_eq!(rx.buffered_samples(), 1);
    }

    #[test]
    fn rx_init_resets_ring_and_dropped_count() {
        let mut rx = WsBridgeRx::new(WsBridgeRxParams {
            stream_id: 0,
            buffer_samples: 2,
            sample_rate_hz: 0.0,
        });
        rx.push(&[
            Complex::new(1.0, 0.0),
            Complex::new(2.0, 0.0),
            Complex::new(3.0, 0.0),
        ]);
        assert_eq!(rx.dropped_samples(), 1);
        let mut ctx = InitCtx {
            input_meta: &[],
            output_meta: &[],
            frames_hint: 1024,
        };
        rx.init(&mut ctx).unwrap();
        assert_eq!(rx.dropped_samples(), 0);
        assert_eq!(rx.buffered_samples(), 0);
    }

    #[test]
    fn rx_stop_resets_ring() {
        let mut rx = WsBridgeRx::new(WsBridgeRxParams {
            stream_id: 0,
            buffer_samples: 4,
            sample_rate_hz: 0.0,
        });
        rx.push(&[Complex::new(1.0, 2.0)]);
        rx.stop().unwrap();
        assert_eq!(rx.buffered_samples(), 0);
    }

    #[test]
    fn iq_tx_pushes_iq_f32_frame_with_le_interleaved_floats() {
        let sink = Arc::new(CapturingSink::default());
        let mut tx = WsBridgeTx::new(WsBridgeParams {
            stream_id: 1234,
            ..Default::default()
        });
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
        match &calls[0] {
            Frame::IqF32 {
                stream_id, payload, ..
            } => {
                assert_eq!(*stream_id, 1234);
                assert_eq!(payload.len(), 16);
                assert_eq!(&payload[0..4], &1.0_f32.to_le_bytes());
                assert_eq!(&payload[4..8], &2.0_f32.to_le_bytes());
                assert_eq!(&payload[8..12], &3.0_f32.to_le_bytes());
                assert_eq!(&payload[12..16], &4.0_f32.to_le_bytes());
            }
            other => panic!("expected IqF32 frame, got {other:?}"),
        }
    }

    #[test]
    fn iq_tx_without_sink_still_drains_upstream() {
        let mut tx = WsBridgeTx::new(WsBridgeParams {
            stream_id: 9,
            ..Default::default()
        });
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
    fn fft_tx_pushes_fft_u8_frame() {
        // One window fits the input exactly — one frame out.
        let sink = Arc::new(CapturingSink::default());
        let mut tx = WsBridgeTxFftU8::new(WsBridgeFftU8Params {
            stream_id: 1,
            frame_size: 32,
        });
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
        match &calls[0] {
            Frame::FftU8 {
                stream_id, payload, ..
            } => {
                assert_eq!(*stream_id, 1);
                assert_eq!(payload, &input);
            }
            other => panic!("expected FftU8 frame, got {other:?}"),
        }
    }

    #[test]
    fn fft_tx_chunks_by_frame_size_one_frame_per_window() {
        // Three full windows + a partial remainder: expect three frames,
        // each of `frame_size` bytes, and the remainder left on the wire
        // for a later tick.
        let sink = Arc::new(CapturingSink::default());
        let mut tx = WsBridgeTxFftU8::new(WsBridgeFftU8Params {
            stream_id: 7,
            frame_size: 8,
        });
        tx.attach_sink(sink.clone());

        let input: Vec<u8> = (0..28).collect();
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

        // 3 × 8 = 24 consumed; 4 bytes of partial window stay on wire.
        assert_eq!(w.consumed[0], 24);

        let calls = sink.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        for (i, call) in calls.iter().enumerate() {
            let Frame::FftU8 {
                stream_id, payload, ..
            } = call
            else {
                panic!("expected FftU8 frame, got {call:?}");
            };
            assert_eq!(*stream_id, 7);
            assert_eq!(payload.len(), 8);
            let expected: Vec<u8> = (i as u8 * 8..i as u8 * 8 + 8).collect();
            assert_eq!(payload, &expected);
        }
    }

    #[test]
    fn fft_tx_with_partial_window_alone_consumes_nothing() {
        // A tick where only half a window has accumulated — the Tx should
        // wait. Under the old behavior this byte slice would be flushed
        // as a runt frame and the UI would render garbage.
        let sink = Arc::new(CapturingSink::default());
        let mut tx = WsBridgeTxFftU8::new(WsBridgeFftU8Params {
            stream_id: 1,
            frame_size: 16,
        });
        tx.attach_sink(sink.clone());

        let input: Vec<u8> = (0..10).collect();
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
        assert_eq!(w.consumed[0], 0);
        assert!(sink.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn events_tx_spec_is_native_only_events_in() {
        let s = WsBridgeTxEvents::spec();
        assert_eq!(s.type_name, "WsBridgeTxEvents");
        assert!(matches!(s.placement, crate::block::Placement::NativeOnly));
        assert_eq!(s.inputs.len(), 1);
        assert!(matches!(
            s.inputs[0].port_type,
            crate::block::PortType::Events
        ));
        assert_eq!(s.outputs.len(), 0);
    }

    #[test]
    fn events_tx_emits_one_json_event_per_complete_line() {
        let sink = Arc::new(CapturingSink::default());
        let mut tx = WsBridgeTxEvents::new(WsBridgeParams {
            stream_id: 2000,
            ..Default::default()
        });
        tx.attach_sink(sink.clone());

        let input = b"{\"digit\":\"1\"}\n{\"digit\":\"2\"}\n".to_vec();
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::Events(&input),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut [],
        };
        let w: Work = tx.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], input.len());

        let calls = sink.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for (i, expected) in [
            b"{\"digit\":\"1\"}".as_slice(),
            b"{\"digit\":\"2\"}".as_slice(),
        ]
        .iter()
        .enumerate()
        {
            let Frame::JsonEvent {
                stream_id, payload, ..
            } = &calls[i]
            else {
                panic!("expected JsonEvent, got {:?}", calls[i]);
            };
            assert_eq!(*stream_id, 2000);
            assert_eq!(payload.as_slice(), *expected);
        }
    }

    #[test]
    fn events_tx_reassembles_events_across_process_calls() {
        let sink = Arc::new(CapturingSink::default());
        let mut tx = WsBridgeTxEvents::new(WsBridgeParams {
            stream_id: 7,
            ..Default::default()
        });
        tx.attach_sink(sink.clone());

        // First call: partial line, no frame emitted.
        let part1 = b"{\"a\"".to_vec();
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::Events(&part1),
        }];
        tx.process(&mut BlockIo {
            inputs: &mut inputs,
            outputs: &mut [],
        })
        .unwrap();
        assert!(sink.calls.lock().unwrap().is_empty());

        // Second call: completes the line.
        let part2 = b":1}\n".to_vec();
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::Events(&part2),
        }];
        tx.process(&mut BlockIo {
            inputs: &mut inputs,
            outputs: &mut [],
        })
        .unwrap();

        let calls = sink.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let Frame::JsonEvent { payload, .. } = &calls[0] else {
            panic!("expected JsonEvent");
        };
        assert_eq!(payload.as_slice(), b"{\"a\":1}");
    }

    #[test]
    fn events_tx_without_sink_still_drains_upstream() {
        let mut tx = WsBridgeTxEvents::new(WsBridgeParams {
            stream_id: 1,
            ..Default::default()
        });
        let input = b"{\"x\":1}\n".to_vec();
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::Events(&input),
        }];
        let w: Work = tx
            .process(&mut BlockIo {
                inputs: &mut inputs,
                outputs: &mut [],
            })
            .unwrap();
        assert_eq!(w.consumed[0], input.len());
    }

    #[test]
    fn one_sink_instance_serves_both_tx_block_types() {
        // The point of the unified trait: a single `Arc<dyn BridgeSink>`
        // can be attached to every Tx block in a preset regardless of
        // port type, and the sink tells them apart by the Frame variant
        // it receives.
        let sink = Arc::new(CapturingSink::default());
        let sink_dyn: Arc<dyn BridgeSink> = sink.clone();

        let mut iq_tx = WsBridgeTx::new(WsBridgeParams {
            stream_id: 1000,
            ..Default::default()
        });
        iq_tx.attach_sink(Arc::clone(&sink_dyn));
        let mut fft_tx = WsBridgeTxFftU8::new(WsBridgeFftU8Params {
            stream_id: 1,
            frame_size: 4,
        });
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

        let fft_input: Vec<u8> = vec![1, 2, 3, 4];
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
        assert!(matches!(
            &calls[0],
            Frame::IqF32 {
                stream_id: 1000,
                ..
            }
        ));
        assert!(matches!(&calls[1], Frame::FftU8 { stream_id: 1, .. }));
    }
}
