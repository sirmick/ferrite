//! Block trait and static descriptors.
//!
//! A block is the unit of DSP. It declares typed ports, typed params, a
//! placement constraint, and implements a narrow lifecycle: `init` →
//! `process` (many times) → `stop`. Blocks know nothing about transport,
//! filesystems, UI, or the network.
//!
//! This module is the **contract** between block authors and the
//! scheduler. The scheduler — `ferrite-runtime`, used both in
//! `ferrited` and (via wasm-pack) in the browser — hands each block
//! its negotiated rates and buffers and drives its `process` method;
//! the block only touches what arrives through [`BlockIo`].
//!
//! See `docs/03-blocks.md` for the design rationale and
//! `docs/02-protocol.md` for how port types map to wire payload types
//! when a block edge crosses the network.

use std::any::Any;

use anyhow::Result;
use num_complex::Complex;

/// Where a block instance may run.
///
/// Most blocks are [`Placement::Either`]: the flowgraph scheduler picks a
/// side per deployment. Blocks that touch hardware (`SoapySDR`) or a
/// browser-only API (`AudioWorklet`) declare a concrete side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    Either,
    NativeOnly,
    WasmOnly,
}

/// The sample-stream kind a port carries.
///
/// Extending this set is a breaking change to the wire protocol — every
/// value here maps 1:1 to a `payload_type` in `docs/02-protocol.md` when
/// the edge crosses the network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    /// Complex 32-bit float, interleaved I,Q.
    IqF32,
    /// Complex 16-bit int, interleaved I,Q.
    IqS16,
    /// Real 32-bit float — audio, envelope, magnitude.
    RealF32,
    /// Real 16-bit int — PCM audio.
    RealI16,
    /// FFT magnitude bins, 32-bit float (log-magnitude dB).
    FftF32,
    /// FFT magnitude bins, `u8` 0..=255 (display-ready).
    FftU8,
    /// Packed bit stream.
    Bits,
    /// Discrete framed packets (opaque bytes).
    Frames,
    /// Structured JSON events.
    Events,
}

impl PortType {
    /// Bytes per sample element on this port.
    #[must_use]
    pub const fn element_size(self) -> usize {
        match self {
            Self::IqF32 => 8,
            Self::IqS16 | Self::RealF32 | Self::FftF32 => 4,
            Self::RealI16 => 2,
            Self::FftU8 | Self::Bits | Self::Frames | Self::Events => 1,
        }
    }
}

/// One input or output port on a block.
#[derive(Debug, Clone, Copy)]
pub struct PortSpec {
    pub name: &'static str,
    pub port_type: PortType,
}

/// Scope of runtime action required when a parameter changes while the
/// graph is running.
///
/// Replaces the binary `mutable_while_streaming` flag from the early
/// runtime prototype (see D19 in `docs/09-decisions.md`). Each param
/// declares the smallest unit the runtime must touch to pick up the new
/// value, so preset edits produce a minimum reconfigure plan instead of
/// restarting the whole graph on every knob tweak.
///
/// The wire-format variants are `camelCase` (`self` / `downstream` /
/// `sourceRestart`) to match the rest of the capability schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReconfigureScope {
    /// Apply in-place inside the owning block. No scheduler-visible
    /// action — e.g. a gain or mixer-frequency that the block swaps on
    /// the next `process` call.
    SelfBlock,
    /// Re-`init` the owning block and every block downstream on the
    /// new rate / frame size before resuming. Intra-graph — the source
    /// end stays running.
    Downstream,
    /// The source end of the graph is torn down and re-opened — any
    /// hardware or transport resource is re-acquired. Centre-frequency
    /// on a fixed-rate SDR typically needs this.
    SourceRestart,
}

impl ReconfigureScope {
    /// Wire-format tag emitted in the capability schema.
    #[must_use]
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::SelfBlock => "self",
            Self::Downstream => "downstream",
            Self::SourceRestart => "sourceRestart",
        }
    }

    /// Parse the wire-format tag. Returns `None` on unknown input so
    /// the caller can surface a schema error rather than panicking.
    #[must_use]
    pub fn from_wire_str(s: &str) -> Option<Self> {
        match s {
            "self" => Some(Self::SelfBlock),
            "downstream" => Some(Self::Downstream),
            "sourceRestart" => Some(Self::SourceRestart),
            _ => None,
        }
    }

    /// Ordering of scopes from cheapest to most disruptive. `merge`
    /// picks the larger of two scopes when a reconfigure plan touches
    /// several params at once.
    #[must_use]
    pub const fn cost(self) -> u8 {
        match self {
            Self::SelfBlock => 0,
            Self::Downstream => 1,
            Self::SourceRestart => 2,
        }
    }

    /// Combine two scopes into the one that covers both. Used by the
    /// runtime's diff engine when a single preset edit changes params
    /// with mixed scopes.
    #[must_use]
    pub const fn merge(self, other: Self) -> Self {
        if self.cost() >= other.cost() {
            self
        } else {
            other
        }
    }
}

impl Default for ReconfigureScope {
    /// Conservative default — assume a new param value invalidates
    /// the whole graph until the block author narrows it down.
    fn default() -> Self {
        Self::SourceRestart
    }
}

/// A block parameter schema entry.
///
/// `reconfig_scope` tells the runtime's diff engine how much it needs to
/// touch when this param changes while the graph is running. Scope
/// `SelfBlock` means "mutable while streaming" in practice — a strict
/// superset of the old binary flag.
#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: ParamKind,
    pub reconfig_scope: ReconfigureScope,
    /// Plain-prose note about this param, dual-purpose: injected into
    /// the ferrite-ai system prompt for the active pipeline, and
    /// surfaced by the UI as the param-control tooltip. One sentence
    /// describing what the knob does and its useful range; two if
    /// there's a meaningful interaction with another param. Empty
    /// string is a temporary placeholder while the schema rolls out
    /// (see `docs/13-ai-notes-migration.md`); a unit test will start
    /// asserting non-empty once the backfill finishes.
    pub ai_notes: &'static str,
}

/// Type-level shape of one param. Rendered 1:1 by the generic options
/// dialog; serialized to the JSON capability schema as the `kind` field
/// documented in `docs/02-protocol.md`.
#[derive(Debug, Clone, Copy)]
pub enum ParamKind {
    Range {
        min: f64,
        max: f64,
        step: f64,
        default: f64,
        unit: &'static str,
    },
    EnumNumeric {
        values: &'static [f64],
        default: f64,
        unit: &'static str,
    },
    EnumString {
        values: &'static [&'static str],
        default: &'static str,
    },
    Toggle {
        default: bool,
    },
    Text {
        default: &'static str,
    },
}

/// Static descriptor for a block type. Introspectable at registry time
/// without constructing an instance.
#[derive(Debug, Clone, Copy)]
pub struct BlockSpec {
    pub type_name: &'static str,
    pub placement: Placement,
    pub inputs: &'static [PortSpec],
    pub outputs: &'static [PortSpec],
    pub params: &'static [ParamSpec],
    /// Plain-prose note about this block, dual-purpose like
    /// [`ParamSpec::ai_notes`]: injected into the ferrite-ai system
    /// prompt for blocks in the active pipeline, and available for
    /// UI surfacing (block-card tooltip / catalog entry). ~50–100
    /// words: what the block does, when you'd reach for it in a
    /// chain, one key gotcha. Even utility blocks (`TeeIqF32`,
    /// `LogMagU8`) get a one-line note — universal discipline, no
    /// judgement calls about what's "user-facing". Empty during the
    /// schema rollout (see `docs/13-ai-notes-migration.md`).
    pub ai_notes: &'static str,
}

/// Metadata carried alongside sample buffers on a port.
#[derive(Debug, Clone, Copy, Default)]
pub struct PortMeta {
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
}

/// Context passed to [`Block::init`]. The scheduler tells the block its
/// negotiated port rates and its per-call sample budget here.
pub struct InitCtx<'a> {
    pub input_meta: &'a [(&'a str, PortMeta)],
    pub output_meta: &'a [(&'a str, PortMeta)],
    /// Nominal frames-per-process-call the scheduler is sizing buffers for.
    pub frames_hint: usize,
}

impl InitCtx<'_> {
    #[must_use]
    pub fn input_rate(&self, name: &str) -> Option<f64> {
        self.input_meta
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, m)| m.sample_rate_hz)
    }

    #[must_use]
    pub fn output_rate(&self, name: &str) -> Option<f64> {
        self.output_meta
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, m)| m.sample_rate_hz)
    }
}

/// Maximum input/output ports a single block may declare.
/// Every block in the v0.1 plan has ≤ 2 of each; 8 is generous headroom.
pub const MAX_PORTS: usize = 8;

/// Items consumed on each input port and produced on each output port
/// during one [`Block::process`] call. Ports are indexed in declaration
/// order — the same order as [`BlockSpec::inputs`] / [`BlockSpec::outputs`].
#[derive(Debug, Clone, Copy, Default)]
pub struct Work {
    pub consumed: [usize; MAX_PORTS],
    pub produced: [usize; MAX_PORTS],
}

impl Work {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            consumed: [0; MAX_PORTS],
            produced: [0; MAX_PORTS],
        }
    }
}

/// Typed view into one input buffer during [`Block::process`].
pub enum InBuf<'a> {
    IqF32(&'a [Complex<f32>]),
    IqS16(&'a [Complex<i16>]),
    RealF32(&'a [f32]),
    RealI16(&'a [i16]),
    FftF32(&'a [f32]),
    FftU8(&'a [u8]),
    Bits(&'a [u8]),
    Frames(&'a [u8]),
    Events(&'a [u8]),
}

/// Typed view into one output buffer during [`Block::process`]. The block
/// writes into the slice and reports how many items were produced via
/// [`Work`].
pub enum OutBuf<'a> {
    IqF32(&'a mut [Complex<f32>]),
    IqS16(&'a mut [Complex<i16>]),
    RealF32(&'a mut [f32]),
    RealI16(&'a mut [i16]),
    FftF32(&'a mut [f32]),
    FftU8(&'a mut [u8]),
    Bits(&'a mut [u8]),
    Frames(&'a mut [u8]),
    Events(&'a mut [u8]),
}

/// One input port's buffer slice plus its metadata for the duration of a
/// [`Block::process`] call.
pub struct InputPort<'a> {
    pub name: &'a str,
    pub meta: PortMeta,
    pub buf: InBuf<'a>,
}

impl InputPort<'_> {
    #[must_use]
    pub fn as_iq_f32(&self) -> Option<&[Complex<f32>]> {
        if let InBuf::IqF32(s) = &self.buf {
            Some(s)
        } else {
            None
        }
    }
    #[must_use]
    pub fn as_real_f32(&self) -> Option<&[f32]> {
        if let InBuf::RealF32(s) = &self.buf {
            Some(s)
        } else {
            None
        }
    }
    #[must_use]
    pub fn as_fft_f32(&self) -> Option<&[f32]> {
        if let InBuf::FftF32(s) = &self.buf {
            Some(s)
        } else {
            None
        }
    }
    #[must_use]
    pub fn as_fft_u8(&self) -> Option<&[u8]> {
        if let InBuf::FftU8(s) = &self.buf {
            Some(s)
        } else {
            None
        }
    }
}

/// One output port's buffer slice plus its metadata for the duration of a
/// [`Block::process`] call.
pub struct OutputPort<'a> {
    pub name: &'a str,
    pub meta: PortMeta,
    pub buf: OutBuf<'a>,
}

impl OutputPort<'_> {
    pub fn as_iq_f32_mut(&mut self) -> Option<&mut [Complex<f32>]> {
        if let OutBuf::IqF32(s) = &mut self.buf {
            Some(&mut **s)
        } else {
            None
        }
    }
    pub fn as_real_f32_mut(&mut self) -> Option<&mut [f32]> {
        if let OutBuf::RealF32(s) = &mut self.buf {
            Some(&mut **s)
        } else {
            None
        }
    }
    pub fn as_fft_f32_mut(&mut self) -> Option<&mut [f32]> {
        if let OutBuf::FftF32(s) = &mut self.buf {
            Some(&mut **s)
        } else {
            None
        }
    }
    pub fn as_fft_u8_mut(&mut self) -> Option<&mut [u8]> {
        if let OutBuf::FftU8(s) = &mut self.buf {
            Some(&mut **s)
        } else {
            None
        }
    }
}

/// Borrowed typed I/O handed to a block for one `process` call.
///
/// Ports are addressed by name; both inputs and outputs are also available
/// as slices in declaration order, matching the [`Work`] return value.
pub struct BlockIo<'a> {
    pub inputs: &'a mut [InputPort<'a>],
    pub outputs: &'a mut [OutputPort<'a>],
}

impl<'a> BlockIo<'a> {
    #[must_use]
    pub fn input(&self, name: &str) -> Option<&InputPort<'a>> {
        self.inputs.iter().find(|p| p.name == name)
    }

    pub fn output_mut(&mut self, name: &str) -> Option<&mut OutputPort<'a>> {
        self.outputs.iter_mut().find(|p| p.name == name)
    }
}

/// Supertrait glue: lets a `dyn Block` be downcast back to a concrete
/// block type. Implemented for every `'static` type via a blanket impl,
/// so existing `impl Block for Foo` blocks pick it up automatically.
///
/// The runtime uses this to reach into a specific block after the graph
/// is built — e.g. to hand a `WsBridgeTx` its `BridgeSink` — without
/// needing a type-erased "attach" method on `Block` itself.
pub trait AsAny {
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T: Any> AsAny for T {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// The DSP block trait.
///
/// Implementations are [`Send`] so the scheduler may move them between
/// tokio tasks or Web Workers. They are not [`Sync`]; `process` takes
/// `&mut self`. Blocks are `'static` (carried via the [`AsAny`]
/// supertrait) so the runtime can downcast a `dyn Block` back to the
/// concrete type when it needs to hand the block transport handles or
/// other non-JSON configuration.
pub trait Block: Send + AsAny {
    /// Static type metadata. Available without an instance so a registry
    /// can enumerate block types at startup.
    fn spec() -> BlockSpec
    where
        Self: Sized;

    /// Called once, after construction, before any samples flow. The
    /// scheduler supplies negotiated rates and the nominal per-call frame
    /// budget via `ctx`. FIR filters etc. precompute coefficients here.
    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()>;

    /// One scheduling tick. Reads from declared inputs, writes to declared
    /// outputs, reports what moved via [`Work`]. Must not allocate on the
    /// hot path.
    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work>;

    /// Per-output minimum capacity the block needs to make forward
    /// progress in one `process` call. Index-parallel to
    /// [`BlockSpec::outputs`]; unused slots are 0. The runtime treats the
    /// return value as a *floor* on each output buffer's capacity and
    /// allocates `max(frames_hint, hints[i])`.
    ///
    /// Default is all zeros, which is correct for blocks that produce
    /// one output sample per input sample or whose output volume is
    /// bounded by `frames_hint`. Blocks that emit in fixed-size frames
    /// larger than `frames_hint` (FFT, LogMagU8) must override so the
    /// runtime provisions buffers big enough to hold one frame.
    ///
    /// Retired by the D23 scheduler rewrite: once every block declares
    /// [`relative_rate`](Self::relative_rate) and overrides
    /// [`forecast`](Self::forecast) where variable, the ring-sizing
    /// math derives from those declarations instead. Kept here until
    /// the migration is complete.
    fn output_capacity_hints(&self) -> [usize; MAX_PORTS] {
        [0; MAX_PORTS]
    }

    /// Declare the block's per-step rate relationship between one
    /// input port and one output port as an `(out_samples,
    /// in_samples)` ratio. Consulted by the scheduler when sizing
    /// wire rings and when deciding how many output slots a block can
    /// legally fill.
    ///
    /// - `(1, 1)` — sync 1:1. The default, and right for most DSP
    ///   blocks (FmDemod, AmDemod, Channelizer at rate-matched
    ///   configurations, TeeIqF32, …). Per-sample work.
    /// - `(1, N)` — sync decimator. For every `N` input samples the
    ///   block produces `1` output. `Decimator`, `Channelizer` (with
    ///   decimation factor).
    /// - `(L, 1)` — sync interpolator. For every `1` input sample the
    ///   block produces `L` outputs. `AmModulator` (with
    ///   input_rate:output_rate = 1:L), resamplers.
    /// - `(1, 0)` — source. The input-less side of a source block;
    ///   output rate is decoupled from any input.
    /// - `(0, 0)` — unconstrained / variable-rate. The scheduler
    ///   falls back entirely on [`forecast`](Self::forecast) for this
    ///   pair. `DtmfDecoder` emits events only on tone transitions;
    ///   `FFT` emits one bin block per `size` inputs (hybrid — can
    ///   also be expressed as `(size, size)` with `forecast`
    ///   overriding).
    ///
    /// `in_port` and `out_port` index into [`BlockSpec::inputs`] and
    /// [`BlockSpec::outputs`] respectively. For single-input /
    /// single-output blocks both are 0.
    ///
    /// Default is `(1, 1)` — matches today's scheduler behavior for
    /// 1:1 DSP, so unmigrated blocks keep working.
    fn relative_rate(&self, _in_port: usize, _out_port: usize) -> (u32, u32) {
        (1, 1)
    }

    /// How many input samples, per input port, does this block need
    /// to produce `noutput_items` on each output? Index-parallel to
    /// [`BlockSpec::inputs`]; unused slots are 0.
    ///
    /// Returning `None` tells the scheduler "use the default derived
    /// from [`relative_rate`](Self::relative_rate)" — i.e.
    /// `in_samples = ceil(noutput_items * in_ratio / out_ratio)` for
    /// each input port. The default is correct for every sync block
    /// and most rate-changing blocks.
    ///
    /// Variable-rate blocks override with `Some(...)` to report a
    /// fixed minimum that doesn't scale with `noutput_items`:
    ///
    /// - `DtmfDecoder` needs one block-size chunk regardless of
    ///   output count (it emits events per tone transition, not per
    ///   sample).
    /// - `FFT` needs one `size`-sample window to produce one bin
    ///   block.
    ///
    /// Called by the scheduler to decide whether a starved block has
    /// enough upstream samples to run. A block forecasting
    /// `[200, 0, ...]` will not be called until its `in_port=0` ring
    /// has ≥ 200 available samples.
    fn forecast(&self, _noutput_items: usize) -> Option<[usize; MAX_PORTS]> {
        None
    }

    /// Absolute output sample rate (Hz) on the given output port, when
    /// known. Used by the runtime to size wire rings so a pure source
    /// can absorb one scheduler tick's worth of samples without
    /// stalling — `frames_hint` alone caps throughput at
    /// `frames_hint / tick_period` MSPS, which for the default 1024 /
    /// 400µs ceiling is ~2.56 MSPS.
    ///
    /// Only pure sources (no input ports) need to implement this —
    /// downstream blocks' rates are derivable from
    /// [`relative_rate`](Self::relative_rate). Returning `None` (the
    /// default) keeps the legacy `frames_hint`-only sizing.
    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        None
    }

    /// Per-output RF centre frequency (Hz). Propagated through the
    /// pipeline alongside [`output_rate_hz`](Self::output_rate_hz)
    /// so downstream blocks (and sidecars like `LogMagU8.record` /
    /// `Channelizer.record`) can stamp the metadata onto recorded
    /// files. Default `None` makes the runtime fall back to the
    /// input[0] producer's value — that's the right answer for any
    /// block that doesn't change the centre frequency (`FmDemod`,
    /// `Tee*`, `RealF32Resamp`, `AudioNr*`, etc.).
    ///
    /// Override on:
    /// - **pure sources** (no input ports) to return their tuned
    ///   centre — `SoapySource` returns its `center_freq_hz` param;
    ///   `SineSource` returns the synth centre.
    /// - **frequency translators** like `Channelizer`, which returns
    ///   `input_centre + freq_shift_hz` (the VFO moves the spectrum).
    ///
    /// Returning `None` from a non-source block is fine and means
    /// "I leave the centre frequency alone."
    fn output_center_freq_hz(&self, _port: usize) -> Option<f64> {
        None
    }

    /// React to an updated input-rate negotiation without going through
    /// the full `init()` path. The runtime calls this during
    /// reconfigure for carried-over blocks (instances preserved across
    /// preset swap) when the scheduler's propagation pass reports that
    /// an input port's rate changed upstream — typically because the
    /// SoapySDR driver snapped a new rate, or the user retuned the
    /// source from the UI.
    ///
    /// Default is a no-op, which is right for rate-insensitive blocks
    /// (`TeeIqF32`, `Squelch`, etc.). Rate-changing blocks override:
    /// `Channelizer` rebuilds its FIR + decim for a new factor;
    /// `RealF32Resamp` rebuilds the liquid `msresamp` for the new
    /// ratio. Unlike `init()`, this MUST NOT set up external state
    /// (reader threads, device streams) — those were created on the
    /// prior runtime and carried into this one.
    fn update_rates(&mut self, _ctx: &InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    /// Flush and release. Must be idempotent.
    fn stop(&mut self) -> Result<()> {
        Ok(())
    }

    /// Try to absorb a params delta in-place, without going through the
    /// runtime's stop-rebuild-start path. Returns `Ok(true)` when the
    /// delta was fully applied live (no further action needed),
    /// `Ok(false)` when the block has no live path for any key in
    /// `delta` (the runtime falls back to a full reconfigure), and
    /// `Err(_)` for hard failures the caller should surface.
    ///
    /// Implementors should treat this as best-effort: if any one key in
    /// `delta` can't be applied live, return `Ok(false)` and let the
    /// rebuild handle the whole change atomically. Don't apply some
    /// keys and reject others — partial application breaks the contract
    /// that `applied_doc` either matches the live state or doesn't.
    ///
    /// Default returns `Ok(false)`, which preserves today's behavior:
    /// every block change rebuilds the flowgraph.
    fn apply_live_params(&mut self, _delta: &serde_json::Value) -> Result<bool> {
        Ok(false)
    }
}

/// Construct an instance from a JSON params value. The runtime calls this
/// through the registry's `construct_fn` when instantiating a flowgraph —
/// every `#[ferrite_block]`-annotated type must implement it.
///
/// Conventionally, blocks derive [`serde::Deserialize`] on their `…Params`
/// struct with `#[serde(default)]` at the container level. That lets an
/// absent or partial JSON object deserialize against the block's `Default`
/// params, so flowgraphs can omit fields they're happy with.
///
/// `serde_json::Value::Null` is accepted and treated as "use defaults".
pub trait BlockFactory: Block + Sized {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>>;
}

/// Helper used by [`BlockFactory`] impls: deserialize a JSON value into a
/// `Params` struct, treating `Value::Null` as "use defaults". Keeps the
/// null-handling convention single-sourced so every block's factory is
/// identical except for the type names.
pub(crate) fn deserialize_params<T>(value: &serde_json::Value) -> Result<T>
where
    T: serde::de::DeserializeOwned + Default,
{
    if value.is_null() {
        Ok(T::default())
    } else {
        Ok(serde_json::from_value(value.clone())?)
    }
}

/// Queue a JSON value as a newline-delimited record onto a decoder's
/// `events` buffer. The `EventStore` sink splits on `\n`, so one record
/// per line. Shared by the decoder blocks that feed `ui:<kind>` → the
/// `DecoderStore`.
pub fn push_event_json(out: &mut Vec<u8>, value: &serde_json::Value) {
    if let Ok(mut bytes) = serde_json::to_vec(value) {
        out.append(&mut bytes);
        out.push(b'\n');
    }
}

/// Drain a queued events buffer into the block's `events` output port,
/// recording the byte count in `w.produced[0]`. Anything that doesn't fit
/// this tick rides to the next. No-op when the queue is empty or the
/// block has no `events` port wired this split.
pub fn drain_events_port(io: &mut BlockIo<'_>, queue: &mut Vec<u8>, w: &mut Work) {
    if queue.is_empty() {
        return;
    }
    for port in io.outputs.iter_mut() {
        if port.name == "events" {
            if let OutBuf::Events(dst) = &mut port.buf {
                let take = queue.len().min(dst.len());
                if take > 0 {
                    dst[..take].copy_from_slice(&queue[..take]);
                    queue.drain(..take);
                    w.produced[0] = take;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Block, BlockIo, BlockSpec, InBuf, InitCtx, InputPort, OutBuf, OutputPort, ParamKind,
        ParamSpec, Placement, PortMeta, PortSpec, PortType, ReconfigureScope, Work,
    };

    struct Passthru;

    impl Block for Passthru {
        fn spec() -> BlockSpec {
            BlockSpec {
                type_name: "Passthru",
                placement: Placement::Either,
                inputs: &[PortSpec {
                    name: "in",
                    port_type: PortType::RealF32,
                }],
                outputs: &[PortSpec {
                    name: "out",
                    port_type: PortType::RealF32,
                }],
                params: &[ParamSpec {
                    key: "gain",
                    label: "Gain",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 2.0,
                        step: 0.01,
                        default: 1.0,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "",
                }],
                ai_notes: "",
            }
        }
        fn init(&mut self, _ctx: &mut InitCtx<'_>) -> anyhow::Result<()> {
            Ok(())
        }
        fn process(&mut self, io: &mut BlockIo<'_>) -> anyhow::Result<Work> {
            // Field-level split borrow: inputs and outputs are disjoint,
            // so taking `&io.inputs[..]` and `&mut io.outputs[..]` is safe.
            let src = io
                .inputs
                .iter()
                .find(|p| p.name == "in")
                .and_then(InputPort::as_real_f32);
            let Some(src) = src else {
                return Ok(Work::new());
            };
            let dst = io
                .outputs
                .iter_mut()
                .find(|p| p.name == "out")
                .and_then(OutputPort::as_real_f32_mut)
                .expect("out port declared");
            let k = src.len().min(dst.len());
            dst[..k].copy_from_slice(&src[..k]);
            let mut w = Work::new();
            w.consumed[0] = k;
            w.produced[0] = k;
            Ok(w)
        }
    }

    #[test]
    fn spec_is_static() {
        let s = Passthru::spec();
        assert_eq!(s.type_name, "Passthru");
        assert_eq!(s.placement, Placement::Either);
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.outputs.len(), 1);
        assert_eq!(s.params.len(), 1);
    }

    #[test]
    fn process_moves_samples() {
        let mut b = Passthru;
        let inputs_meta = [(
            "in",
            PortMeta {
                sample_rate_hz: 48_000.0,
                center_freq_hz: 0.0,
            },
        )];
        let mut ctx = InitCtx {
            input_meta: &inputs_meta[..],
            output_meta: &[],
            frames_hint: 128,
        };
        b.init(&mut ctx).unwrap();

        let input_samples: [f32; 4] = [1.0, 2.0, 3.0, 4.0];
        let mut output_samples = [0.0f32; 4];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(&input_samples),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut output_samples),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = b.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], 4);
        assert_eq!(w.produced[0], 4);
        let expect = [1.0f32, 2.0, 3.0, 4.0];
        for (got, want) in output_samples.iter().zip(expect.iter()) {
            assert!((got - want).abs() < f32::EPSILON);
        }
    }

    #[test]
    fn port_type_element_sizes() {
        assert_eq!(PortType::IqF32.element_size(), 8);
        assert_eq!(PortType::FftF32.element_size(), 4);
        assert_eq!(PortType::FftU8.element_size(), 1);
    }

    #[test]
    fn reconfigure_scope_wire_strings_round_trip() {
        use super::ReconfigureScope;
        for scope in [
            ReconfigureScope::SelfBlock,
            ReconfigureScope::Downstream,
            ReconfigureScope::SourceRestart,
        ] {
            let wire = scope.as_wire_str();
            assert_eq!(ReconfigureScope::from_wire_str(wire), Some(scope));
        }
        assert_eq!(ReconfigureScope::from_wire_str("nope"), None);
    }

    #[test]
    fn reconfigure_scope_default_is_most_disruptive() {
        use super::ReconfigureScope;
        // Conservative default — block authors must explicitly opt into
        // a cheaper scope.
        assert_eq!(ReconfigureScope::default(), ReconfigureScope::SourceRestart);
    }

    #[test]
    fn reconfigure_scope_merge_picks_larger() {
        use super::ReconfigureScope::{Downstream, SelfBlock, SourceRestart};
        assert_eq!(SelfBlock.merge(Downstream), Downstream);
        assert_eq!(Downstream.merge(SelfBlock), Downstream);
        assert_eq!(Downstream.merge(SourceRestart), SourceRestart);
        assert_eq!(SourceRestart.merge(SelfBlock), SourceRestart);
        assert_eq!(SelfBlock.merge(SelfBlock), SelfBlock);
    }

    #[test]
    fn reconfigure_scope_cost_is_ordered() {
        use super::ReconfigureScope::{Downstream, SelfBlock, SourceRestart};
        assert!(SelfBlock.cost() < Downstream.cost());
        assert!(Downstream.cost() < SourceRestart.cost());
    }
}
