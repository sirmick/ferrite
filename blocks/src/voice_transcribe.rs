//! `VoiceTranscribe` — browser-side passthrough tap for speech-to-text.
//!
//! Drops into the audio chain *after* the noise-reduction block and
//! *before* `AudioSink`:
//!
//! ```text
//! … → AudioNr → VoiceTranscribe → AudioSink
//! ```
//!
//! It is a **pure passthrough** on the audio path: every input sample
//! is copied to `out` unchanged (or zeroed in `muted` mode) so
//! inserting it never alters what the operator hears. Separately, when
//! transcription is active it mirrors the input into an internal
//! [`AudioRing`] — the exact tap mechanism [`AudioSink`] uses. The
//! browser runner drains that ring after each tick into a
//! `SharedArrayBuffer` that a transcription Web Worker (Silero VAD +
//! whisper.cpp WASM) reads on its own clock. Inference never touches
//! the audio or main thread.
//!
//! `Placement::WasmOnly` — `env_split` carves this out of the node-side
//! doc, so `ferrited` never instantiates it. The native equivalent of
//! "tap audio to disk" remains `FileAudioSink`.
//!
//! ### Tri-state `mode`
//!
//! - `off`   — passthrough only. Zero cost: no ring write, no drain.
//! - `on`    — passthrough **and** transcription (hear it *and* log it).
//! - `muted` — transcription only; `out` is silenced. "Leave it
//!   logging a frequency you're not actively listening to."

use anyhow::Result;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};
use crate::spsc_ring::AudioRing;

/// Ring capacity default — ~340 ms at 48 kHz. Larger than `AudioSink`'s
/// because the consumer is a VAD/STT worker on a coarser cadence than
/// the audio clock, so it tolerates (and benefits from) more slack.
const DEFAULT_BUFFER_SAMPLES: usize = 16_384;
const DEFAULT_BUFFER_SAMPLES_F64: f64 = 16_384.0;

/// Transcription engagement state. String-valued so the preset / UI
/// control reads `off` / `on` / `muted` rather than a magic number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Passthrough only — no tap, no transcription.
    Off,
    /// Passthrough **and** tap for transcription.
    On,
    /// Tap for transcription; audio output silenced.
    Muted,
}

impl Mode {
    fn parse(s: &str) -> Self {
        match s {
            "on" => Self::On,
            "muted" => Self::Muted,
            // Unknown / "off" — fail safe to the zero-cost state.
            _ => Self::Off,
        }
    }

    /// True when the block should mirror input into the tap ring.
    #[must_use]
    pub fn taps(self) -> bool {
        matches!(self, Self::On | Self::Muted)
    }

    /// True when the audio output should be passed through audibly.
    #[must_use]
    pub fn audible(self) -> bool {
        matches!(self, Self::Off | Self::On)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VoiceTranscribeParams {
    /// `off` | `on` | `muted` — see [`Mode`].
    pub mode: String,
    /// Tap-ring capacity in samples (power of two).
    pub buffer_samples: usize,
}

impl Default for VoiceTranscribeParams {
    fn default() -> Self {
        Self {
            mode: "off".to_string(),
            buffer_samples: DEFAULT_BUFFER_SAMPLES,
        }
    }
}

const MODE_PARAM: ParamSpec = ParamSpec {
    key: "mode",
    label: "Transcription",
    kind: ParamKind::EnumString {
        values: &["off", "on", "muted"],
        default: "off",
    },
    // Toggling engagement must take effect without tearing down the
    // source — it's the whole point of a "leave it running" control.
    reconfig_scope: ReconfigureScope::SelfBlock,
    ai_notes: "Speech-to-text engagement. 'off' = audio passes through untouched, no transcription (zero cost). 'on' = audio plays AND is transcribed. 'muted' = transcribed only, audio silenced — for logging a frequency you're not listening to.",
};

const BUFFER_SAMPLES_PARAM: ParamSpec = ParamSpec {
    key: "buffer_samples",
    label: "Tap ring capacity",
    kind: ParamKind::Range {
        min: 4_096.0,
        max: 1_048_576.0,
        step: 1.0,
        default: DEFAULT_BUFFER_SAMPLES_F64,
        unit: "samples",
    },
    reconfig_scope: ReconfigureScope::SourceRestart,
    ai_notes: "SharedArrayBuffer tap-ring capacity in samples handed to the transcription worker. Sized to a few hundred ms at the audio rate; larger only helps if the worker falls behind.",
};

pub struct VoiceTranscribe {
    params: VoiceTranscribeParams,
    mode: Mode,
    ring: AudioRing,
    dropped_samples: u64,
    /// Negotiated input sample rate, captured at `init`. The worker
    /// needs it to resample to whisper's 16 kHz mono.
    input_rate_hz: f64,
}

impl VoiceTranscribe {
    #[must_use]
    pub fn new(params: VoiceTranscribeParams) -> Self {
        let ring = AudioRing::new(params.buffer_samples.next_power_of_two());
        let mode = Mode::parse(&params.mode);
        Self {
            params,
            mode,
            ring,
            dropped_samples: 0,
            input_rate_hz: 0.0,
        }
    }

    /// Cumulative count of samples dropped because the tap ring was
    /// full (worker fell behind). Surfaced as a glitch counter.
    #[must_use]
    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples
    }

    /// Mutable view of the tap ring — drained by the browser runner
    /// after each tick into the worker's `SharedArrayBuffer`.
    pub fn ring_mut(&mut self) -> &mut AudioRing {
        &mut self.ring
    }

    /// Negotiated input sample rate (Hz), known after `init`. 0.0
    /// before the first `init`.
    #[must_use]
    pub fn input_rate_hz(&self) -> f64 {
        self.input_rate_hz
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    #[must_use]
    pub fn params(&self) -> &VoiceTranscribeParams {
        &self.params
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for VoiceTranscribe {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "VoiceTranscribe",
            placement: Placement::WasmOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::RealF32,
            }],
            params: &[MODE_PARAM, BUFFER_SAMPLES_PARAM],
            ai_notes: "Browser-only passthrough tap that feeds demodulated voice to an in-browser speech-to-text worker (Silero VAD + whisper.cpp). Insert it between AudioNr and AudioSink. Audio is passed through unchanged ('off'/'on') or silenced ('muted'); it never degrades the signal. Tri-state 'mode' engages transcription. Built for noisy SSB/AM voice with a 'set it and leave it' workflow — accuracy over latency.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        self.input_rate_hz = ctx.input_rate("in").unwrap_or(0.0);
        self.ring.reset();
        self.dropped_samples = 0;
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let mut w = Work::new();
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
        else {
            return Ok(w);
        };
        // Borrow `io.outputs` directly (disjoint from the `io.inputs`
        // borrow held by `src`) — the `output_mut` helper takes `&mut
        // io` whole and would collide. Mirrors `TeeRealF32`.
        let Some(out) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(|p| p.as_real_f32_mut())
        else {
            return Ok(w);
        };
        let n = src.len().min(out.len());

        // Tap first (the *clean* input, regardless of mute state).
        if self.mode.taps() {
            let written = self.ring.write(&src[..n]);
            if written < n {
                // Drop on overflow — the worker clock must never
                // back-pressure the audio pipeline.
                self.dropped_samples += (n - written) as u64;
            }
        }

        // Passthrough (or silence in `muted`).
        if self.mode.audible() {
            out[..n].copy_from_slice(&src[..n]);
        } else {
            out[..n].fill(0.0);
        }

        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }

    fn stop(&mut self) -> Result<()> {
        self.ring.reset();
        Ok(())
    }
}

impl BlockFactory for VoiceTranscribe {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: VoiceTranscribeParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(VoiceTranscribe::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{Mode, VoiceTranscribe, VoiceTranscribeParams, DEFAULT_BUFFER_SAMPLES};
    use crate::block::{
        Block, BlockIo, InBuf, InitCtx, InputPort, OutBuf, OutputPort, Placement, PortMeta,
    };

    fn run(vt: &mut VoiceTranscribe, samples: &[f32]) -> Vec<f32> {
        let mut out = vec![-1.0_f32; samples.len()];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(samples),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = vt.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], samples.len());
        assert_eq!(w.produced[0], samples.len());
        out
    }

    #[test]
    fn spec_is_wasm_only_passthrough() {
        let s = VoiceTranscribe::spec();
        assert_eq!(s.type_name, "VoiceTranscribe");
        assert!(matches!(s.placement, Placement::WasmOnly));
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.outputs.len(), 1);
        assert_eq!(s.inputs[0].port_type, crate::block::PortType::RealF32);
        assert_eq!(s.outputs[0].port_type, crate::block::PortType::RealF32);
    }

    #[test]
    fn default_mode_is_off_zero_cost() {
        let p = VoiceTranscribeParams::default();
        assert_eq!(p.mode, "off");
        assert_eq!(p.buffer_samples, DEFAULT_BUFFER_SAMPLES);
        let mut vt = VoiceTranscribe::new(p);
        assert_eq!(vt.mode(), Mode::Off);
        let out = run(&mut vt, &[0.1, 0.2, 0.3]);
        // Passthrough verbatim, nothing tapped.
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
        assert_eq!(vt.ring_mut().available_read(), 0);
    }

    #[test]
    fn on_mode_passes_through_and_taps() {
        let mut vt = VoiceTranscribe::new(VoiceTranscribeParams {
            mode: "on".into(),
            buffer_samples: 16,
        });
        let out = run(&mut vt, &[0.5, -0.5, 0.25]);
        assert_eq!(out, vec![0.5, -0.5, 0.25], "audio passes through");
        let mut tap = [0.0; 3];
        assert_eq!(vt.ring_mut().read(&mut tap), 3);
        assert_eq!(tap, [0.5, -0.5, 0.25], "clean input tapped");
    }

    #[test]
    fn muted_mode_silences_output_but_still_taps_clean_input() {
        let mut vt = VoiceTranscribe::new(VoiceTranscribeParams {
            mode: "muted".into(),
            buffer_samples: 16,
        });
        let out = run(&mut vt, &[0.5, -0.5, 0.25]);
        assert_eq!(out, vec![0.0, 0.0, 0.0], "output silenced");
        let mut tap = [0.0; 3];
        assert_eq!(vt.ring_mut().read(&mut tap), 3);
        assert_eq!(tap, [0.5, -0.5, 0.25], "tap sees the un-muted signal");
    }

    #[test]
    fn tap_overflow_tallies_dropped_samples() {
        let mut vt = VoiceTranscribe::new(VoiceTranscribeParams {
            mode: "on".into(),
            buffer_samples: 4,
        });
        run(&mut vt, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        assert_eq!(vt.dropped_samples(), 2);
        assert_eq!(vt.ring_mut().available_read(), 4);
    }

    #[test]
    fn init_captures_input_rate_and_resets() {
        let mut vt = VoiceTranscribe::new(VoiceTranscribeParams {
            mode: "on".into(),
            buffer_samples: 8,
        });
        run(&mut vt, &[1.0, 2.0]);
        let inputs_meta = [(
            "in",
            PortMeta {
                sample_rate_hz: 12_000.0,
                center_freq_hz: 0.0,
            },
        )];
        let mut ctx = InitCtx {
            input_meta: &inputs_meta[..],
            output_meta: &[],
            frames_hint: 1024,
        };
        vt.init(&mut ctx).unwrap();
        assert_eq!(vt.input_rate_hz(), 12_000.0);
        assert_eq!(vt.ring_mut().available_read(), 0);
        assert_eq!(vt.dropped_samples(), 0);
    }

    #[test]
    fn buffer_samples_rounded_to_power_of_two() {
        let vt = VoiceTranscribe::new(VoiceTranscribeParams {
            mode: "off".into(),
            buffer_samples: 5000,
        });
        // 5000 → 8192; SpscRing requires a power of two.
        let mut vt = vt;
        assert_eq!(vt.ring_mut().capacity(), 8192);
    }
}
