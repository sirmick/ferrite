//! Spectral subtraction noise reduction for voice-band audio.
//!
//! One real-f32 in, one real-f32 out, same rate. Operates in
//! overlap-add STFT frames:
//!
//! ```text
//! buffer `block_size` samples with a Hann window and 50 % overlap
//! FFT → magnitude-squared spectrum
//! estimate noise floor as a leaky minimum across frames
//! subtract (over-subtraction factor β) from per-bin magnitude²
//! clamp with a floor so holes stay shallow (avoid musical noise)
//! reconstruct with the original phase, inverse FFT
//! overlap-add into the output stream
//! ```
//!
//! ### Why this is acceptable for voice SDR
//!
//! Classic 1979-Boll spectral subtraction. Computationally cheap (two
//! FFTs per frame at e.g. 512 points @ 48 kHz → 11 ms frames), real-time
//! safe (fixed allocation, no branches in the hot path), and audibly
//! effective on stationary hiss from SSB/NBFM weak-signal receive.
//! Weakness is *non-stationary* noise and the well-known "musical
//! noise" artefact from aggressive floor clamping; the preset defaults
//! pick a conservative over-subtraction β = 1.5 that keeps the
//! warbling imperceptible on voice while still shaving ~6–10 dB of
//! hiss. RNN-based options (see future `RnnNoise` block) handle both
//! issues better at the cost of a ~300 kB model.
//!
//! ### Latency
//!
//! One `block_size` of samples of group delay: the output of sample
//! `k` doesn't emit until `k + block_size` has entered. At 48 kHz with
//! `block_size = 512` that's 10.7 ms — below the ~20 ms threshold
//! where voice echo becomes perceptible. Block sizes ≥ 1024 start to
//! noticeably lag speech; ≤ 256 hurt the noise-floor estimator.
//!
//! ### Rate awareness
//!
//! No rate-dependent state — the frame size is in samples, not
//! seconds — so the block works identically at 44.1 kHz, 48 kHz,
//! 192 kHz. Declared rate via `InitCtx` is propagated downstream for
//! completeness.

use anyhow::{bail, Result};
use rustfft::{num_complex::Complex, FftPlanner};
use serde::Deserialize;
use std::sync::Arc;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct SpectralNrParams {
    /// STFT block size in samples. Power of two, ≥ 64. 512 gives
    /// ~10 ms frames at 48 kHz, the sweet spot for voice.
    pub block_size: usize,
    /// Over-subtraction factor β. The classic Boll formula subtracts
    /// β·|noise|² from |signal|². 1.0 = exact, 1.5 = moderate over-
    /// subtraction (smoother noise floor at the cost of slight hole-
    /// punching), 2.0 = aggressive. Above ~2.5 you start hearing
    /// musical-noise warble.
    pub beta: f32,
    /// Floor gain: the minimum per-bin gain after subtraction, in
    /// linear amplitude. 0.1 = −20 dB floor; 0.316 = −10 dB. A higher
    /// floor kills less noise but suppresses the warble; a lower
    /// floor sounds quieter but more artificial.
    pub floor: f32,
    /// Noise-floor learning rate per frame. 0 = never adapt (freeze
    /// on the first frame); 1 = replace every frame (no memory).
    /// 0.05 = 20-frame half-life, ~200 ms at 10 ms frames.
    pub noise_alpha: f32,
}

impl Default for SpectralNrParams {
    fn default() -> Self {
        Self {
            block_size: 512,
            beta: 1.5,
            floor: 0.1,
            noise_alpha: 0.05,
        }
    }
}

pub struct SpectralNr {
    params: SpectralNrParams,
    fft_fwd: Arc<dyn rustfft::Fft<f32>>,
    fft_inv: Arc<dyn rustfft::Fft<f32>>,
    window: Vec<f32>,

    /// Input ring: when it fills to one hop, we run a frame.
    in_buf: Vec<f32>,
    in_filled: usize,

    /// Overlap-add accumulator: output samples waiting to be drained.
    /// Holds the tail end of the most-recent frame (half overlaps with
    /// the one before it).
    ola: Vec<f32>,
    ola_read: usize,

    /// Scratch FFT buffer.
    scratch: Vec<Complex<f32>>,

    /// Learned noise magnitude² per bin. Leaky minimum.
    noise_mag2: Vec<f32>,
    noise_initialised: bool,
}

impl SpectralNr {
    pub fn new(params: SpectralNrParams) -> Result<Self> {
        if !(params.block_size >= 64 && params.block_size.is_power_of_two()) {
            bail!(
                "spectral_nr: block_size must be power-of-two and ≥ 64 (got {})",
                params.block_size
            );
        }
        if !(params.beta.is_finite() && params.beta > 0.0) {
            bail!("spectral_nr: beta must be > 0 (got {})", params.beta);
        }
        if !(params.floor.is_finite() && (0.0..=1.0).contains(&params.floor)) {
            bail!(
                "spectral_nr: floor must be in [0, 1] (got {})",
                params.floor
            );
        }
        if !(params.noise_alpha.is_finite() && (0.0..=1.0).contains(&params.noise_alpha)) {
            bail!(
                "spectral_nr: noise_alpha must be in [0, 1] (got {})",
                params.noise_alpha
            );
        }

        let n = params.block_size;
        let mut planner = FftPlanner::<f32>::new();
        let fft_fwd = planner.plan_fft_forward(n);
        let fft_inv = planner.plan_fft_inverse(n);

        // sqrt(Hann) window applied on BOTH analysis and synthesis;
        // the product is a full Hann, which satisfies COLA at 50 %
        // overlap and gives perfect reconstruction for the passthrough
        // case (spectral gain == 1 in every bin).
        #[allow(clippy::cast_precision_loss)]
        let window: Vec<f32> = (0..n)
            .map(|i| {
                let phase = core::f32::consts::TAU * i as f32 / n as f32;
                (0.5 - 0.5 * phase.cos()).sqrt()
            })
            .collect();

        Ok(Self {
            params,
            fft_fwd,
            fft_inv,
            window,
            in_buf: vec![0.0; n],
            in_filled: n / 2, // start half-full so the first hop (n/2)
            // triggers a frame — otherwise we'd need n samples before
            // emitting anything.
            ola: vec![0.0; n],
            ola_read: 0,
            scratch: vec![Complex::new(0.0, 0.0); n],
            noise_mag2: vec![0.0; n],
            noise_initialised: false,
        })
    }

    fn process_one_frame(&mut self) {
        let n = self.params.block_size;

        // Windowed copy into FFT scratch.
        for i in 0..n {
            self.scratch[i] = Complex::new(self.in_buf[i] * self.window[i], 0.0);
        }

        self.fft_fwd.process(&mut self.scratch);

        // Update noise estimate and apply suppression. Symmetric
        // spectrum (real input → conjugate-symmetric output); apply
        // the same gain to bin k and bin n-k so iFFT stays real.
        let alpha = self.params.noise_alpha;
        let beta = self.params.beta;
        let floor = self.params.floor;
        let half = n / 2 + 1;
        for k in 0..half {
            let c = self.scratch[k];
            let mag2 = c.re * c.re + c.im * c.im;
            if !self.noise_initialised {
                self.noise_mag2[k] = mag2;
            } else if mag2 < self.noise_mag2[k] {
                // Instantaneous when the signal dips below the current
                // estimate — captures quiet gaps quickly.
                self.noise_mag2[k] = mag2;
            } else {
                // Slow decay upward (never let the noise estimate
                // race away with loud voice frames).
                self.noise_mag2[k] += alpha * (mag2 - self.noise_mag2[k]);
            }

            let sub = mag2 - beta * self.noise_mag2[k];
            let gain = if mag2 > 1e-20 {
                (sub / mag2).max(floor * floor).sqrt()
            } else {
                floor
            };
            self.scratch[k] = c * gain;
            // Mirror the gain onto the conjugate bin so the iFFT's
            // imaginary part stays zero.
            if k != 0 && k != n / 2 {
                let mirror = n - k;
                self.scratch[mirror] = self.scratch[mirror] * gain;
            }
        }
        self.noise_initialised = true;

        self.fft_inv.process(&mut self.scratch);

        // Overlap-add into `ola`. rustfft's inverse isn't normalised
        // (Parseval says scale by 1/n); fold that in with the window.
        #[allow(clippy::cast_precision_loss)]
        let norm = 1.0 / n as f32;
        let hop = n / 2;
        // Shift the previous second-half into the first-half slot.
        self.ola.copy_within(hop.., 0);
        for v in self.ola[hop..].iter_mut() {
            *v = 0.0;
        }
        // Add windowed iFFT output.
        for i in 0..n {
            self.ola[i] += self.scratch[i].re * self.window[i] * norm;
        }
        self.ola_read = 0;
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for SpectralNr {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "SpectralNr",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::RealF32,
            }],
            params: &[
                ParamSpec {
                    key: "block_size",
                    label: "FFT block size",
                    kind: ParamKind::EnumNumeric {
                        values: &[128.0, 256.0, 512.0, 1024.0, 2048.0],
                        default: 512.0,
                        unit: "samples",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "beta",
                    label: "Over-subtraction β",
                    kind: ParamKind::Range {
                        min: 0.5,
                        max: 3.0,
                        step: 0.1,
                        default: 1.5,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "floor",
                    label: "Gain floor",
                    kind: ParamKind::Range {
                        min: 0.01,
                        max: 1.0,
                        step: 0.01,
                        default: 0.1,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "noise_alpha",
                    label: "Noise learning α",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1.0,
                        step: 0.001,
                        default: 0.05,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
            ],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn forecast(&self, _noutput_items: usize) -> Option<[usize; crate::block::MAX_PORTS]> {
        // We produce in hops of `block_size/2`; the scheduler can call
        // us as often as it wants, we'll emit what's ready.
        None
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
        else {
            return Ok(Work::new());
        };
        let Some(dst) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_real_f32_mut)
        else {
            return Ok(Work::new());
        };

        let n = self.params.block_size;
        let hop = n / 2;

        let mut consumed = 0;
        let mut produced = 0;

        loop {
            // Drain ready overlap-add samples first — anything in
            // [ola_read, hop) is finalised (second-half ola not yet
            // complete, sits idle for the next frame).
            while self.ola_read < hop && produced < dst.len() {
                dst[produced] = self.ola[self.ola_read];
                self.ola_read += 1;
                produced += 1;
            }
            if produced == dst.len() {
                break;
            }

            // Otherwise, pull more input until we have a full hop.
            while self.in_filled < n && consumed < src.len() {
                self.in_buf[self.in_filled] = src[consumed];
                consumed += 1;
                self.in_filled += 1;
            }
            if self.in_filled < n {
                // Starved upstream; stop.
                break;
            }

            self.process_one_frame();
            // Shift input buffer: drop the first hop, keep second half
            // as the front of the next frame.
            self.in_buf.copy_within(hop.., 0);
            self.in_filled = hop;
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;
        w.produced[0] = produced;
        Ok(w)
    }
}

impl BlockFactory for SpectralNr {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: SpectralNrParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(SpectralNr::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{SpectralNr, SpectralNrParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;

    fn run_all(block: &mut SpectralNr, input: &[f32]) -> Vec<f32> {
        // Oversize the output so a single `process` call can always
        // drain any OLA backlog — otherwise the scheduler-style
        // break on `produced == dst.len()` would stall before the
        // caller's input had been fully consumed, and subsequent
        // calls would never see those trailing input samples.
        let mut out = vec![0.0_f32; input.len() + block.params.block_size];
        let mut i = 0;
        let mut o = 0;
        while i < input.len() {
            let in_slice = &input[i..];
            let out_slice = &mut out[o..];
            let mut inputs = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(in_slice),
            }];
            let mut outputs = [OutputPort {
                name: "out",
                meta: PortMeta::default(),
                buf: OutBuf::RealF32(out_slice),
            }];
            let mut io = BlockIo {
                inputs: &mut inputs,
                outputs: &mut outputs,
            };
            let w = block.process(&mut io).unwrap();
            if w.consumed[0] == 0 && w.produced[0] == 0 {
                break;
            }
            i += w.consumed[0];
            o += w.produced[0];
        }
        out.truncate(o);
        out
    }

    #[test]
    fn rejects_bad_params() {
        assert!(SpectralNr::new(SpectralNrParams {
            block_size: 63,
            ..Default::default()
        })
        .is_err());
        assert!(SpectralNr::new(SpectralNrParams {
            block_size: 500,
            ..Default::default()
        })
        .is_err());
        assert!(SpectralNr::new(SpectralNrParams {
            beta: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(SpectralNr::new(SpectralNrParams {
            floor: 1.5,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn disabled_suppression_is_exact_passthrough() {
        // floor=1.0 sets the minimum per-bin gain to unity, which
        // makes spectral subtraction a no-op and isolates the STFT
        // reconstruction path. A clean tone should survive within
        // OLA-reconstruction tolerance (sqrt-Hann + 50 % overlap
        // satisfies COLA, so amplitude is preserved exactly in the
        // steady state).
        let fs = 48_000.0_f32;
        let f_tone = 1_000.0_f32;
        let n = 4096_usize;
        let input: Vec<f32> = (0..n)
            .map(|i| (TAU * f_tone * i as f32 / fs).cos())
            .collect();
        let mut b = SpectralNr::new(SpectralNrParams {
            floor: 1.0,
            ..Default::default()
        })
        .unwrap();
        let out = run_all(&mut b, &input);
        let rms =
            |x: &[f32]| -> f32 { (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt() };
        let warm = 1024;
        let cap = out.len().min(input.len());
        let r_in = rms(&input[warm..cap]);
        let r_out = rms(&out[warm..cap]);
        let ratio = r_out / r_in;
        assert!(
            (0.95..=1.05).contains(&ratio),
            "floor=1.0 OLA passthrough ratio {ratio} (in={r_in}, out={r_out})"
        );
    }

    #[test]
    fn white_noise_is_attenuated() {
        // Seed a deterministic pseudo-random noise stream; run it
        // through a short window of training, then measure the ratio
        // of output RMS to input RMS on the tail. Should be well
        // below 1 (the block learned the noise and pulled it down
        // to near the floor).
        let n = 16384_usize;
        let mut rng = 0x12345678u32;
        let input: Vec<f32> = (0..n)
            .map(|_| {
                // xorshift
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                (rng as f32 / u32::MAX as f32 - 0.5) * 0.2
            })
            .collect();
        let mut b = SpectralNr::new(SpectralNrParams {
            beta: 2.0,
            floor: 0.1,
            noise_alpha: 0.1,
            ..Default::default()
        })
        .unwrap();
        let out = run_all(&mut b, &input);
        let rms =
            |x: &[f32]| -> f32 { (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt() };
        // Skip the first 2048 samples so the noise estimate has
        // converged.
        let warm = 2048;
        let cap = out.len().min(input.len());
        let r_in = rms(&input[warm..cap]);
        let r_out = rms(&out[warm..cap]);
        let suppression_db = 20.0 * (r_in / r_out).log10();
        assert!(
            suppression_db > 3.0,
            "expected ≥ 3 dB of noise suppression, got {suppression_db:.1} dB"
        );
    }

    #[test]
    fn state_persists_across_calls() {
        // Calling process() with a chopped-up input should produce the
        // same output as one long call. Tolerates small numerical
        // differences from FFT edge effects but should be close.
        let n = 8192_usize;
        let input: Vec<f32> = (0..n)
            .map(|i| (TAU * 1_000.0 * i as f32 / 48_000.0).cos())
            .collect();
        let params = SpectralNrParams {
            floor: 1.0,
            ..Default::default()
        };
        let mut whole = SpectralNr::new(params).unwrap();
        let out_whole = run_all(&mut whole, &input);

        let mut split = SpectralNr::new(params).unwrap();
        let out_a = run_all(&mut split, &input[..n / 2]);
        let out_b = run_all(&mut split, &input[n / 2..]);
        let mut joined = out_a;
        joined.extend_from_slice(&out_b);

        let len = out_whole.len().min(joined.len());
        for i in 0..len {
            assert!(
                (out_whole[i] - joined[i]).abs() < 1e-3,
                "mismatch at {i}: whole={} split={}",
                out_whole[i],
                joined[i]
            );
        }
    }
}
