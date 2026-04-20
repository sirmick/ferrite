//! `WsIqSource` — browser-side source that consumes IQ frames arriving
//! over a multiplexed WebSocket and emits them onto the flowgraph's
//! `iq_f32` output.
//!
//! Mirrors the TS `WsIqSource` in `web/src/lib/ws/wsIqSourceBlock.ts`
//! — same name, same param shape, same `buffer_floats` default — so
//! presets authored against the TS registry parse unchanged once the
//! browser loads this crate as WASM.
//!
//! ### Transport split
//!
//! The block does **not** own the WebSocket. The browser runner keeps
//! a single multiplexed `FrameClient` on the JS side and pushes
//! incoming IQ frames into this block via
//! `RuntimeHandle::pushIq(blockId, Float32Array)`. The block's job is
//! just the ring-buffer: accept pushed samples, emit them onto the
//! output port at tick time.
//!
//! Keeping the transport on JS keeps the Rust code portable — no
//! `web_sys` / `js_sys` dependency creeps into `ferrite-blocks`, and
//! native unit tests drive the block by calling [`push`] directly.

use anyhow::Result;
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, OutputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};
use crate::spsc_ring::IqRing;

/// Ring capacity default in **samples** (complex IQ pairs). Matches the
/// TS default of 131 072 *floats* — each complex sample is two floats,
/// so 65 536 samples ≈ 650 ms at 100 kS/s narrowband. Plenty of
/// headroom for scheduler jitter without adding meaningful latency.
const DEFAULT_BUFFER_SAMPLES: usize = 65_536;

/// `f64` mirror of [`DEFAULT_BUFFER_SAMPLES`] for the param schema.
const DEFAULT_BUFFER_SAMPLES_F64: f64 = 65_536.0;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct WsIqSourceParams {
    /// `stream_id` the browser-side transport demultiplexes for this
    /// block. The runner sends a subscription for this id on start and
    /// drops it on stop; the block itself just consumes samples.
    pub stream_id: u32,
    /// Ring capacity in complex samples. Power of two.
    pub buffer_samples: usize,
}

impl Default for WsIqSourceParams {
    fn default() -> Self {
        Self {
            stream_id: 0,
            buffer_samples: DEFAULT_BUFFER_SAMPLES,
        }
    }
}

const STREAM_ID_PARAM: ParamSpec = ParamSpec {
    key: "stream_id",
    label: "Stream id",
    kind: ParamKind::Range {
        min: 0.0,
        max: u32::MAX as f64,
        step: 1.0,
        default: 0.0,
        unit: "",
    },
    // Wired into the FrameClient subscription at load time — the
    // subscription would have to be rebuilt against the WS.
    reconfig_scope: ReconfigureScope::SourceRestart,
};

const BUFFER_SAMPLES_PARAM: ParamSpec = ParamSpec {
    key: "buffer_samples",
    label: "Buffer capacity",
    kind: ParamKind::Range {
        min: 1_024.0,
        max: 1_048_576.0,
        step: 1.0,
        default: DEFAULT_BUFFER_SAMPLES_F64,
        unit: "samples",
    },
    reconfig_scope: ReconfigureScope::SourceRestart,
};

pub struct WsIqSource {
    params: WsIqSourceParams,
    ring: IqRing,
    dropped_samples: u64,
}

impl WsIqSource {
    #[must_use]
    pub fn new(params: WsIqSourceParams) -> Self {
        let ring = IqRing::new(params.buffer_samples);
        Self {
            params,
            ring,
            dropped_samples: 0,
        }
    }

    #[must_use]
    pub fn params(&self) -> &WsIqSourceParams {
        &self.params
    }

    /// Cumulative count of samples dropped because the ring was full.
    /// Matches the TS `droppedFloats` counter semantically, but counts
    /// complex samples rather than raw floats.
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
    /// Samples that don't fit are counted into `dropped_samples` — the
    /// network clock wins over the flowgraph clock (losing IQ samples
    /// beats stalling the WS reader, same policy as the TS side).
    pub fn push(&mut self, samples: &[Complex<f32>]) {
        let written = self.ring.write(samples);
        if written < samples.len() {
            self.dropped_samples += (samples.len() - written) as u64;
        }
    }

    /// Push a batch of samples from an interleaved `[i0, q0, i1, q1, …]`
    /// float slice. Convenience over [`push`] for callers that receive
    /// WS frames as raw `Float32Array` payloads. Odd-length inputs drop
    /// the trailing half-sample.
    pub fn push_interleaved(&mut self, floats: &[f32]) {
        let pair_count = floats.len() / 2;
        let mut tmp = Vec::with_capacity(pair_count);
        for chunk in floats[..pair_count * 2].chunks_exact(2) {
            tmp.push(Complex::new(chunk[0], chunk[1]));
        }
        self.push(&tmp);
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for WsIqSource {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "WsIqSource",
            placement: Placement::WasmOnly,
            inputs: &[],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[STREAM_ID_PARAM, BUFFER_SAMPLES_PARAM],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        self.ring.reset();
        self.dropped_samples = 0;
        Ok(())
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

impl BlockFactory for WsIqSource {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: WsIqSourceParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(WsIqSource::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{WsIqSource, WsIqSourceParams, DEFAULT_BUFFER_SAMPLES};
    use crate::block::{Block, BlockIo, InitCtx, OutBuf, OutputPort, Placement, PortMeta};
    use num_complex::Complex;

    fn spec_checks() {
        // pin the spec shape — this is a source block, wasm-only, one
        // IqF32 output port, two params.
        let s = WsIqSource::spec();
        assert_eq!(s.type_name, "WsIqSource");
        assert!(matches!(s.placement, Placement::WasmOnly));
        assert_eq!(s.inputs.len(), 0);
        assert_eq!(s.outputs.len(), 1);
        assert_eq!(s.outputs[0].port_type, crate::block::PortType::IqF32);
        assert_eq!(s.params.len(), 2);
    }

    #[test]
    fn spec_is_wasm_only_iq_out() {
        spec_checks();
    }

    #[test]
    fn defaults_match_ts_shape() {
        let p = WsIqSourceParams::default();
        assert_eq!(p.stream_id, 0);
        assert_eq!(p.buffer_samples, DEFAULT_BUFFER_SAMPLES);
    }

    #[test]
    fn params_round_trip_through_json() {
        let value = serde_json::json!({ "stream_id": 1000, "buffer_samples": 1024 });
        let p: WsIqSourceParams = serde_json::from_value(value).unwrap();
        assert_eq!(p.stream_id, 1000);
        assert_eq!(p.buffer_samples, 1024);
    }

    #[test]
    fn pushed_samples_surface_on_the_output_port() {
        let mut src = WsIqSource::new(WsIqSourceParams {
            stream_id: 0,
            buffer_samples: 8,
        });
        src.push(&[
            Complex::new(1.0, 2.0),
            Complex::new(3.0, 4.0),
            Complex::new(5.0, 6.0),
        ]);
        assert_eq!(src.buffered_samples(), 3);

        let mut out_buf = [Complex::new(0.0, 0.0); 8];
        let mut outputs = vec![OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut out_buf),
        }];
        let mut inputs: Vec<crate::block::InputPort<'_>> = Vec::new();
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let work = src.process(&mut io).unwrap();
        assert_eq!(work.produced[0], 3);
        assert_eq!(
            out_buf[..3],
            [
                Complex::new(1.0, 2.0),
                Complex::new(3.0, 4.0),
                Complex::new(5.0, 6.0),
            ]
        );
        assert_eq!(src.buffered_samples(), 0);
    }

    #[test]
    fn push_tallies_overflow_dropped_samples() {
        let mut src = WsIqSource::new(WsIqSourceParams {
            stream_id: 0,
            buffer_samples: 2,
        });
        src.push(&[
            Complex::new(1.0, 0.0),
            Complex::new(2.0, 0.0),
            Complex::new(3.0, 0.0),
        ]);
        assert_eq!(src.dropped_samples(), 1);
        assert_eq!(src.buffered_samples(), 2);
    }

    #[test]
    fn push_interleaved_converts_floats_to_complex_pairs() {
        let mut src = WsIqSource::new(WsIqSourceParams {
            stream_id: 0,
            buffer_samples: 4,
        });
        // 4 floats → 2 complex samples.
        src.push_interleaved(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(src.buffered_samples(), 2);

        let mut out_buf = [Complex::new(0.0, 0.0); 2];
        let mut outputs = vec![OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut out_buf),
        }];
        let mut inputs: Vec<crate::block::InputPort<'_>> = Vec::new();
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        src.process(&mut io).unwrap();
        assert_eq!(out_buf, [Complex::new(1.0, 2.0), Complex::new(3.0, 4.0)]);
    }

    #[test]
    fn push_interleaved_drops_trailing_half_sample() {
        let mut src = WsIqSource::new(WsIqSourceParams {
            stream_id: 0,
            buffer_samples: 4,
        });
        // 3 floats = 1 pair + a dangling 0.3.
        src.push_interleaved(&[0.1, 0.2, 0.3]);
        assert_eq!(src.buffered_samples(), 1);
    }

    #[test]
    fn init_resets_ring_and_dropped_count() {
        let mut src = WsIqSource::new(WsIqSourceParams {
            stream_id: 0,
            buffer_samples: 2,
        });
        src.push(&[
            Complex::new(1.0, 0.0),
            Complex::new(2.0, 0.0),
            Complex::new(3.0, 0.0),
        ]);
        assert_eq!(src.dropped_samples(), 1);
        let mut ctx = InitCtx {
            input_meta: &[],
            output_meta: &[],
            frames_hint: 1024,
        };
        src.init(&mut ctx).unwrap();
        assert_eq!(src.dropped_samples(), 0);
        assert_eq!(src.buffered_samples(), 0);
    }

    #[test]
    fn stop_resets_ring() {
        let mut src = WsIqSource::new(WsIqSourceParams {
            stream_id: 0,
            buffer_samples: 4,
        });
        src.push(&[Complex::new(1.0, 2.0)]);
        src.stop().unwrap();
        assert_eq!(src.buffered_samples(), 0);
    }
}
