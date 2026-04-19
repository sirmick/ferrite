//! Block trait and static descriptors.
//!
//! A block is the unit of DSP. It declares typed ports, typed params, a
//! placement constraint, and implements a narrow lifecycle: `init` →
//! `process` (many times) → `stop`. Blocks know nothing about transport,
//! filesystems, UI, or the network.
//!
//! This module is the **contract** between block authors and the
//! scheduler. The scheduler (Rust-side in `ferrited`, TS-side in the
//! browser/Node runtime) hands each block its negotiated rates and buffers
//! and drives its `process` method; the block only touches what arrives
//! through [`BlockIo`].
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

/// A block parameter schema entry.
#[derive(Debug, Clone, Copy)]
pub struct ParamSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: ParamKind,
    pub mutable_while_streaming: bool,
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
/// is built — e.g. to hand a `WsBridgeTx` its `IqBridgeSink` — without
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

    /// Flush and release. Must be idempotent.
    fn stop(&mut self) -> Result<()> {
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::{
        Block, BlockIo, BlockSpec, InBuf, InitCtx, InputPort, OutBuf, OutputPort, ParamKind,
        ParamSpec, Placement, PortMeta, PortSpec, PortType, Work,
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
                    mutable_while_streaming: true,
                }],
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
}
