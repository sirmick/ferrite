//! `AudioSink` — browser-side terminal for demodulated audio.
//!
//! Receives `RealF32` samples and, in the browser, hands them to a
//! `SharedArrayBuffer` ring that an `AudioWorklet` drains on its own
//! 48 kHz clock. Mirrors the TS `AudioSink` in
//! `web/src/lib/audio/audioSinkBlock.ts` byte-for-byte at the params /
//! port level so a preset authored against the Rust registry parses
//! today and runs natively once the M4 browser WASM host lands.
//!
//! ### Scope of this commit
//!
//! `Placement::WasmOnly` means `env_split` carves this block out of the
//! node-side doc entirely, so `ferrited` never instantiates it. The
//! `process` impl now writes into an internal [`AudioRing`] and tracks
//! overflow via `dropped_samples` — byte-compatible with the TS
//! `AudioSink`. The SAB/`AudioWorklet` hand-off still needs a
//! wasm-bindgen bridge; that lands in the next M4 slice.

use anyhow::Result;
use serde::Deserialize;

use crate::audio_ring::AudioRing;
use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, ParamKind, ParamSpec, Placement, PortSpec,
    PortType, Work,
};

/// Ring capacity default — ~170 ms at 48 kHz. Matches the TS
/// `AudioSink` default so presets carry the same numbers across the
/// TS→Rust cut-over.
const DEFAULT_BUFFER_SAMPLES: usize = 8192;

/// `f64` mirror of [`DEFAULT_BUFFER_SAMPLES`] for the param schema.
/// Kept as its own constant so the cast is explicit and exact rather
/// than recomputed at schema-construction time.
const DEFAULT_BUFFER_SAMPLES_F64: f64 = 8192.0;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct AudioSinkParams {
    /// Ring capacity in samples. Must be a power of two when the real
    /// SAB ring lands; the placeholder does not enforce it today.
    pub buffer_samples: usize,
}

impl Default for AudioSinkParams {
    fn default() -> Self {
        Self {
            buffer_samples: DEFAULT_BUFFER_SAMPLES,
        }
    }
}

const BUFFER_SAMPLES_PARAM: ParamSpec = ParamSpec {
    key: "buffer_samples",
    label: "Ring capacity",
    kind: ParamKind::Range {
        min: 256.0,
        max: 1_048_576.0,
        step: 1.0,
        default: DEFAULT_BUFFER_SAMPLES_F64,
        unit: "samples",
    },
    mutable_while_streaming: false,
};

pub struct AudioSink {
    params: AudioSinkParams,
    ring: AudioRing,
    dropped_samples: u64,
}

impl AudioSink {
    #[must_use]
    pub fn new(params: AudioSinkParams) -> Self {
        let ring = AudioRing::new(params.buffer_samples);
        Self {
            params,
            ring,
            dropped_samples: 0,
        }
    }

    /// Cumulative count of samples dropped because the ring was full.
    /// Matches the TS `droppedSamples` getter. Consumer-facing in tests
    /// and eventually in a diagnostics pane.
    #[must_use]
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples
    }

    /// Mutable view of the internal ring. Pulled by the runtime host
    /// (native tests, or the browser tick loop) to drain audio samples
    /// out of the block after each `process` pass.
    pub fn ring_mut(&mut self) -> &mut AudioRing {
        &mut self.ring
    }

    #[must_use]
    pub fn params(&self) -> &AudioSinkParams {
        &self.params
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for AudioSink {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "AudioSink",
            placement: Placement::WasmOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[],
            params: &[BUFFER_SAMPLES_PARAM],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        self.ring.reset();
        self.dropped_samples = 0;
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let mut w = Work::new();
        let Some(port) = io.inputs.iter().find(|p| p.name == "in") else {
            return Ok(w);
        };
        let crate::block::InBuf::RealF32(slice) = &port.buf else {
            return Ok(w);
        };
        let written = self.ring.write(slice);
        if written < slice.len() {
            // Report full input as consumed even on overflow. Reporting
            // `written` would make the scheduler re-present the same
            // samples, back-pressuring the radio pipeline on the audio
            // clock — bad for live listening. Drop and move on.
            self.dropped_samples += (slice.len() - written) as u64;
        }
        w.consumed[0] = slice.len();
        Ok(w)
    }

    fn stop(&mut self) -> Result<()> {
        self.ring.reset();
        Ok(())
    }
}

impl BlockFactory for AudioSink {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: AudioSinkParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(AudioSink::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioSink, AudioSinkParams, DEFAULT_BUFFER_SAMPLES};
    use crate::block::{Block, BlockIo, InBuf, InitCtx, InputPort, Placement, PortMeta};

    fn make_port(buf: &[f32]) -> InputPort<'_> {
        InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(buf),
        }
    }

    fn run_process(sink: &mut AudioSink, samples: &[f32]) {
        let mut inputs = vec![make_port(samples)];
        let mut outputs: Vec<crate::block::OutputPort<'_>> = Vec::new();
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let work = sink.process(&mut io).unwrap();
        assert_eq!(work.consumed[0], samples.len(), "must report full consume");
    }

    #[test]
    fn spec_is_wasm_only_real_in() {
        let s = AudioSink::spec();
        assert_eq!(s.type_name, "AudioSink");
        assert!(matches!(s.placement, Placement::WasmOnly));
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.outputs.len(), 0);
        assert_eq!(s.inputs[0].port_type, crate::block::PortType::RealF32);
    }

    #[test]
    fn defaults_match_ts() {
        let p = AudioSinkParams::default();
        assert_eq!(p.buffer_samples, DEFAULT_BUFFER_SAMPLES);
    }

    #[test]
    fn params_round_trip_through_json() {
        let value = serde_json::json!({ "buffer_samples": 4096 });
        let p: AudioSinkParams = serde_json::from_value(value).unwrap();
        assert_eq!(p.buffer_samples, 4096);
    }

    #[test]
    fn process_writes_samples_into_the_ring() {
        let mut sink = AudioSink::new(AudioSinkParams { buffer_samples: 8 });
        run_process(&mut sink, &[0.1, 0.2, 0.3]);
        assert_eq!(sink.ring_mut().available_read(), 3);

        let mut drained = [0.0; 3];
        let got = sink.ring_mut().read(&mut drained);
        assert_eq!(got, 3);
        assert_eq!(drained, [0.1, 0.2, 0.3]);
    }

    #[test]
    fn process_tallies_overflow_dropped_samples() {
        // Ring holds 4, we push 6 — two samples drop.
        let mut sink = AudioSink::new(AudioSinkParams { buffer_samples: 4 });
        run_process(&mut sink, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(sink.dropped_samples(), 2);
        assert_eq!(sink.ring_mut().available_read(), 4);
    }

    #[test]
    fn init_resets_ring_and_dropped_count() {
        let mut sink = AudioSink::new(AudioSinkParams { buffer_samples: 4 });
        run_process(&mut sink, &[1.0, 2.0, 3.0, 4.0, 5.0]);
        assert_eq!(sink.dropped_samples(), 1);

        let mut ctx = InitCtx {
            input_meta: &[],
            output_meta: &[],
            frames_hint: 1024,
        };
        sink.init(&mut ctx).unwrap();
        assert_eq!(sink.dropped_samples(), 0);
        assert_eq!(sink.ring_mut().available_read(), 0);
    }

    #[test]
    fn stop_resets_ring() {
        let mut sink = AudioSink::new(AudioSinkParams { buffer_samples: 4 });
        run_process(&mut sink, &[1.0, 2.0]);
        sink.stop().unwrap();
        assert_eq!(sink.ring_mut().available_read(), 0);
    }
}
