//! Neural noise reduction.
//!
//! Currently wraps **RNNoise** (via the pure-Rust [`nnnoiseless`]
//! port). Ships with embedded GRU weights; no ONNX runtime, no model
//! file, ~50 kB of code, wasm32-clean.
//!
//! ### Why not DeepFilterNet3 in V1?
//!
//! The original plan was DFN3-only. Reality check while implementing:
//! the published `deep_filter` crate is the STFT/ERB *plumbing* the
//! DFN team uses for training and preprocessing — it is **not** a
//! full inference runtime. Real DFN3 inference wants `tract-onnx`
//! plus an embedded ~2 MB ONNX model file, plus verification that
//! tract runs DFN3 at real-time on wasm32. That's a separate
//! milestone, not a drop-in dependency bump. RNNoise gives us a
//! functional neural stage today; DFN3 is the V2 quality upgrade
//! once we've tested, measured, and decided we want to pay for the
//! extra model-size + compute budget.
//!
//! ### Fixed-rate operation
//!
//! RNNoise is hard-coded to 48 kHz with 480-sample (10 ms) frames.
//! For the V1 audio_nr pipeline we require the input rate to already
//! be 48 kHz — which matches every broadcast/voice preset downstream
//! of `RealF32Resamp`. If the live rate is anything else the stage
//! quietly passes through and leaves a trace warning; adding a
//! resample bridge via `liquid::MsResamp` is an easy follow-up when
//! a variable-rate audio path shows up.
//!
//! ### Warmup and wet/dry
//!
//! nnnoiseless's `process_frame` has one frame of warmup latency —
//! the first output frame is discarded. We emit zeros for that span
//! so the stream length stays 1:1 with input. `attenuation_db` caps
//! how deep the neural stage can attenuate (wet/dry mix) so the
//! output stays natural instead of "processed".

use nnnoiseless::DenoiseState;

/// RNNoise input/output frame length in samples. nnnoiseless hard-codes
/// this as 480 — re-exported for callers doing buffer math.
pub const NEURAL_FRAME_SIZE: usize = DenoiseState::FRAME_SIZE;
/// Required input rate for RNNoise.
pub const NEURAL_RATE_HZ: f64 = 48_000.0;
/// Tolerance around [`NEURAL_RATE_HZ`] within which we still engage
/// the neural path. Wider than f32 epsilon because our rates come
/// from channelizer arithmetic that lands a few ppm off.
const RATE_TOLERANCE_HZ: f64 = 1.0;

pub struct NeuralStage {
    inner: Box<DenoiseState<'static>>,
    input_rate_hz: f64,
    /// Wet gain derived from `attenuation_db`. `output = dry * (1-w) +
    /// denoised * w` per sample.
    wet: f32,
    /// 480-sample input accumulator at 48 kHz.
    frame_in: [f32; NEURAL_FRAME_SIZE],
    frame_filled: usize,
    /// Queue of samples ready for emission (post-denoise, dry/wet mixed).
    out_ring: Vec<f32>,
    out_read: usize,
    /// Dry copy of the most recently-buffered frame, used for the
    /// wet/dry mix when the denoised frame drops out of the box.
    frame_dry: [f32; NEURAL_FRAME_SIZE],
    /// nnnoiseless emits one frame of junk before the model warms up;
    /// we drop that and emit zeros for the first frame instead of the
    /// garbage.
    warmup_frames: u32,
}

impl NeuralStage {
    #[must_use]
    pub fn new(attenuation_db: f32, input_rate_hz: f64) -> Self {
        // Convert cap-dB to wet-mix weight. 0 dB means "full denoised"
        // (wet=1); +∞ dB means zero dry bleed. Practical range ~6–60 dB.
        // `wet = 1 - 10^(-a/20)` gives 0.5 at 6 dB, 0.87 at 18 dB,
        // 0.97 at 30 dB. This is the attenuation *floor* the stage is
        // allowed to reach, not a fixed amount.
        let wet = if attenuation_db.is_finite() && attenuation_db > 0.0 {
            1.0 - 10.0_f32.powf(-attenuation_db / 20.0)
        } else {
            1.0
        };
        Self {
            inner: DenoiseState::new(),
            input_rate_hz,
            wet,
            frame_in: [0.0; NEURAL_FRAME_SIZE],
            frame_filled: 0,
            out_ring: vec![0.0; NEURAL_FRAME_SIZE],
            out_read: NEURAL_FRAME_SIZE, // empty on init
            frame_dry: [0.0; NEURAL_FRAME_SIZE],
            warmup_frames: 1,
        }
    }

    pub fn set_input_rate(&mut self, input_rate_hz: f64) {
        self.input_rate_hz = input_rate_hz;
    }

    pub fn reset(&mut self) {
        self.inner = DenoiseState::new();
        self.frame_filled = 0;
        self.out_read = NEURAL_FRAME_SIZE;
        self.warmup_frames = 1;
    }

    #[must_use]
    fn at_neural_rate(&self) -> bool {
        (self.input_rate_hz - NEURAL_RATE_HZ).abs() < RATE_TOLERANCE_HZ
    }

    /// Run neural denoise in place. 1:1 sample count; first frame of
    /// output is zeros (warmup). Passthrough when the input rate isn't
    /// 48 kHz (V1 limitation — add a resample bridge to lift).
    pub fn run(&mut self, buf: &mut [f32]) {
        if !self.at_neural_rate() {
            // V1: only engage at the native RNNoise rate. Pass through
            // silently; the block-level rate log surfaces if this is
            // the unexpected path.
            return;
        }

        let wet = self.wet;
        for sample in buf {
            // Push input into the 480-sample frame buffer.
            self.frame_in[self.frame_filled] = *sample * (i16::MAX as f32);
            self.frame_dry[self.frame_filled] = *sample;
            self.frame_filled += 1;

            // When full, denoise → scale back → wet/dry mix → enqueue.
            if self.frame_filled == NEURAL_FRAME_SIZE {
                let mut out = [0.0_f32; NEURAL_FRAME_SIZE];
                self.inner.process_frame(&mut out, &self.frame_in);
                let inv_scale = 1.0 / (i16::MAX as f32);
                if self.warmup_frames > 0 {
                    // Emit zeros during warmup. Preserves 1:1 timing
                    // without leaking the denoiser's garbage frame.
                    self.out_ring.fill(0.0);
                    self.warmup_frames -= 1;
                } else {
                    for (i, slot) in self.out_ring.iter_mut().take(NEURAL_FRAME_SIZE).enumerate() {
                        let dry = self.frame_dry[i];
                        let denoised = out[i] * inv_scale;
                        *slot = dry * (1.0 - wet) + denoised * wet;
                    }
                }
                self.out_read = 0;
                self.frame_filled = 0;
            }

            // Drain one sample (zero when the ring is empty — i.e.
            // before the first full frame has been processed).
            let y = if self.out_read < NEURAL_FRAME_SIZE {
                let v = self.out_ring[self.out_read];
                self.out_read += 1;
                v
            } else {
                0.0
            };
            *sample = y;
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{NeuralStage, NEURAL_FRAME_SIZE, NEURAL_RATE_HZ};

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn passthrough_off_rate() {
        // At a non-48k rate the stage must not touch the signal.
        let mut s = NeuralStage::new(18.0, 24_000.0);
        let mut buf: Vec<f32> = (0..4_000).map(|i| (i as f32 * 0.01).sin() * 0.3).collect();
        let input = buf.clone();
        s.run(&mut buf);
        for (i, (a, b)) in input.iter().zip(buf.iter()).enumerate() {
            assert_eq!(a.to_bits(), b.to_bits(), "sample {i}: {a} vs {b}");
        }
    }

    #[test]
    fn suppresses_white_noise_at_48k() {
        // Generate 2 s of white noise at 48 k, let the denoiser warm
        // up on the first frame, measure the reduction on the tail.
        let mut rng = 0x13FFFFFF_u32;
        let n = (NEURAL_RATE_HZ as usize) * 2;
        let input: Vec<f32> = (0..n)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                (rng as f32 / u32::MAX as f32 - 0.5) * 0.2
            })
            .collect();

        let mut s = NeuralStage::new(30.0, NEURAL_RATE_HZ);
        let mut out = input.clone();
        s.run(&mut out);

        // Skip warmup + a few extra frames of convergence.
        let warm = 5 * NEURAL_FRAME_SIZE;
        let db = 20.0 * (rms(&input[warm..]) / rms(&out[warm..])).log10();
        assert!(db > 2.0, "expected ≥2 dB suppression, got {db:.1}");
    }

    #[test]
    fn state_persists_across_chunked_calls() {
        let n = 4 * NEURAL_FRAME_SIZE;
        let input: Vec<f32> = (0..n).map(|i| (i as f32 * 0.01).sin() * 0.1).collect();

        let mut whole = NeuralStage::new(18.0, NEURAL_RATE_HZ);
        let mut out_whole = input.clone();
        whole.run(&mut out_whole);

        let mut split = NeuralStage::new(18.0, NEURAL_RATE_HZ);
        let mut a = input[..n / 2].to_vec();
        let mut b = input[n / 2..].to_vec();
        split.run(&mut a);
        split.run(&mut b);
        let mut joined = a;
        joined.extend_from_slice(&b);

        for i in 0..n {
            assert!(
                (out_whole[i] - joined[i]).abs() < 1e-5,
                "mismatch at {i}: whole={} split={}",
                out_whole[i],
                joined[i]
            );
        }
    }
}
