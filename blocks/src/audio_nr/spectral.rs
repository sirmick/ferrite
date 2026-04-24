//! STFT-based spectral noise reduction.
//!
//! Runs an overlap-add STFT (sqrt-Hann analysis & synthesis windows,
//! 50 % overlap → full Hann product, COLA-correct) and applies a
//! per-bin gain derived from the observed magnitude² and a leaky-min
//! noise-floor tracker.
//!
//! Two gain formulas selectable at construction time:
//!
//! - **Boll** (1979 spectral subtraction): `G = √(max(1 − β·λ/|Y|², f²))`
//!   where `λ` is the noise mag² estimate, `β` is the over-subtraction
//!   factor, `f` is the linear gain floor. Cheap, classic, prone to
//!   "musical noise" when β gets aggressive.
//! - **MMSE-LSA** (Ephraim–Malah 1985 log-spectral-amplitude MMSE):
//!   optimal MMSE estimator of `log |X|` given a Gaussian speech/noise
//!   model. Uses a decision-directed a-priori-SNR estimator to
//!   stabilise the gain frame-to-frame. Sounds noticeably more
//!   natural than Boll at equal noise-attenuation depth — the gain
//!   varies smoothly across frames so the noise floor doesn't
//!   warble. Costs an exponential-integral evaluation per bin per
//!   frame, which we approximate with Abramowitz-Stegun 5.1.53/11.
//!
//! ### Streaming API
//!
//! [`SpectralStage::run`] takes `&mut [f32]`, consumes N input
//! samples and emits N output samples with `block_size/2` samples of
//! latency (the first hop-worth of outputs are zeros until the first
//! STFT frame completes). Internal state carries across calls so the
//! caller can pass any chunk size.

use rustfft::{num_complex::Complex, FftPlanner};
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum SpectralMethod {
    /// 1979 Boll spectral subtraction — cheap, audible musical-noise
    /// artefacts at aggressive β.
    #[serde(rename = "boll")]
    Boll,
    /// Log-spectral-amplitude MMSE estimator (Ephraim-Malah 1985)
    /// with decision-directed a-priori-SNR tracking.
    #[serde(rename = "mmse_lsa")]
    MmseLsa,
}

impl Default for SpectralMethod {
    fn default() -> Self {
        Self::Boll
    }
}

pub struct SpectralStage {
    method: SpectralMethod,
    block_size: usize,
    oversub: f32,
    floor: f32,
    noise_alpha: f32,
    /// Decision-directed smoothing for a-priori SNR (MMSE-LSA only).
    /// Ephraim-Malah recommends 0.98 — heavy smoothing so the gain is
    /// stable, slight responsiveness from the instantaneous residual.
    alpha_snr: f32,

    fft_fwd: Arc<dyn rustfft::Fft<f32>>,
    fft_inv: Arc<dyn rustfft::Fft<f32>>,
    window: Vec<f32>,

    in_buf: Vec<f32>,
    in_filled: usize,

    ola: Vec<f32>,
    ola_read: usize,

    scratch: Vec<Complex<f32>>,

    noise_mag2: Vec<f32>,
    noise_initialised: bool,
    /// MMSE-LSA decision-directed a-priori SNR memory, per bin.
    prior_snr: Vec<f32>,
}

impl SpectralStage {
    #[must_use]
    pub fn new(
        method: SpectralMethod,
        block_size: usize,
        oversub: f32,
        floor: f32,
        noise_alpha: f32,
    ) -> Self {
        let n = block_size.max(64).next_power_of_two();
        let mut planner = FftPlanner::<f32>::new();
        let fft_fwd = planner.plan_fft_forward(n);
        let fft_inv = planner.plan_fft_inverse(n);

        #[allow(clippy::cast_precision_loss)]
        let window: Vec<f32> = (0..n)
            .map(|i| {
                let phase = core::f32::consts::TAU * i as f32 / n as f32;
                (0.5 - 0.5 * phase.cos()).sqrt()
            })
            .collect();

        Self {
            method,
            block_size: n,
            oversub,
            floor,
            noise_alpha,
            alpha_snr: 0.98,
            fft_fwd,
            fft_inv,
            window,
            in_buf: vec![0.0; n],
            in_filled: n / 2, // half-full on init so the first hop
            // of input pushes us to `n` and triggers frame #0 —
            // otherwise the first `n` samples would produce nothing.
            ola: vec![0.0; n],
            ola_read: 0,
            scratch: vec![Complex::new(0.0, 0.0); n],
            noise_mag2: vec![0.0; n],
            noise_initialised: false,
            prior_snr: vec![1.0; n],
        }
    }

    pub fn reset(&mut self) {
        for v in &mut self.in_buf {
            *v = 0.0;
        }
        self.in_filled = self.block_size / 2;
        for v in &mut self.ola {
            *v = 0.0;
        }
        self.ola_read = 0;
        self.noise_initialised = false;
        for v in &mut self.noise_mag2 {
            *v = 0.0;
        }
        for v in &mut self.prior_snr {
            *v = 1.0;
        }
    }

    /// Run the spectral NR stage in place. One sample in → one sample
    /// out; `block_size/2` samples of latency on the very first call
    /// (output is zeros until the first frame completes). State
    /// persists across calls so chunk size doesn't matter.
    pub fn run(&mut self, buf: &mut [f32]) {
        for sample in buf {
            let x = *sample;
            // Buffer input.
            self.in_buf[self.in_filled] = x;
            self.in_filled += 1;

            let n = self.block_size;
            let hop = n / 2;

            // When the analysis window fills, process one frame and
            // reset ola_read to 0 so the newly-written first-half of
            // `ola` starts draining.
            if self.in_filled == n {
                self.process_one_frame();
                self.in_buf.copy_within(hop.., 0);
                self.in_filled = hop;
                self.ola_read = 0;
            }

            // Emit the next OLA sample; zero once the first-half has
            // been drained (steady-state paces one-for-one with input
            // because the next frame is triggered exactly every `hop`
            // inputs, which is also `hop` outputs drained).
            let y = if self.ola_read < hop {
                let v = self.ola[self.ola_read];
                self.ola_read += 1;
                v
            } else {
                0.0
            };
            *sample = y;
        }
    }

    fn process_one_frame(&mut self) {
        let n = self.block_size;

        // Analysis: window and transform.
        for i in 0..n {
            self.scratch[i] = Complex::new(self.in_buf[i] * self.window[i], 0.0);
        }
        self.fft_fwd.process(&mut self.scratch);

        // Per-bin gain computation. Real input → conjugate-symmetric
        // output, so compute on bins 0..=n/2 and mirror the gain onto
        // the upper half so the iFFT's imaginary part stays zero.
        let alpha_noise = self.noise_alpha;
        let beta = self.oversub;
        let floor = self.floor;
        let alpha_snr = self.alpha_snr;
        let half = n / 2 + 1;
        for k in 0..half {
            let c = self.scratch[k];
            let mag2 = c.re * c.re + c.im * c.im;

            // Leaky-min noise tracker: instant when mag² dips below
            // the estimate, slow creep upward otherwise. Keeps the
            // floor from running away on loud voice frames.
            if !self.noise_initialised {
                self.noise_mag2[k] = mag2;
            } else if mag2 < self.noise_mag2[k] {
                self.noise_mag2[k] = mag2;
            } else {
                self.noise_mag2[k] += alpha_noise * (mag2 - self.noise_mag2[k]);
            }

            let noise_m2 = self.noise_mag2[k].max(1e-20);
            let gain = match self.method {
                SpectralMethod::Boll => {
                    let sub = mag2 - beta * noise_m2;
                    if mag2 > 1e-20 {
                        (sub / mag2).max(floor * floor).sqrt()
                    } else {
                        floor
                    }
                }
                SpectralMethod::MmseLsa => {
                    // γ: a-posteriori SNR. ξ: a-priori SNR, updated
                    // decision-directed per Ephraim-Malah: weighted
                    // average of the instantaneous residual SNR and
                    // the previous frame's *applied* estimate.
                    let gamma = mag2 / noise_m2;
                    let inst = (gamma - 1.0).max(0.0);
                    let xi = alpha_snr * self.prior_snr[k] + (1.0 - alpha_snr) * inst;
                    let xi = xi.max(1e-6);
                    let nu = xi / (1.0 + xi) * gamma;
                    // G(ξ, ν) = (ξ/(1+ξ)) · exp(½ · E1(ν))
                    let base = xi / (1.0 + xi);
                    let g_raw = base * (0.5 * expint_e1(nu)).exp();
                    let g = g_raw.clamp(floor, 1.0);
                    // Remember the *applied* SNR for the next frame.
                    // `g² · γ` is the MMSE-LSA estimate of the clean
                    // a-priori SNR given this frame's observation.
                    self.prior_snr[k] = g * g * gamma;
                    g
                }
            };

            self.scratch[k] = c * gain;
            if k != 0 && k != n / 2 {
                let mirror = n - k;
                self.scratch[mirror] = self.scratch[mirror] * gain;
            }
        }
        self.noise_initialised = true;

        // iFFT + overlap-add into `ola`. rustfft doesn't normalise
        // the inverse; fold `1/n` into the synthesis step.
        self.fft_inv.process(&mut self.scratch);

        let hop = n / 2;
        #[allow(clippy::cast_precision_loss)]
        let norm = 1.0 / n as f32;
        // Shift the old second-half into the first-half slot — those
        // samples are "partially accumulated" and get completed by
        // this frame's first half.
        self.ola.copy_within(hop.., 0);
        for v in self.ola[hop..].iter_mut() {
            *v = 0.0;
        }
        for i in 0..n {
            self.ola[i] += self.scratch[i].re * self.window[i] * norm;
        }
    }
}

/// Exponential integral E1(x), `∫_x^∞ e^{-t}/t dt`, for `x > 0`.
/// Abramowitz-Stegun 5.1.53 for `0 < x ≤ 1` (rational polynomial) and
/// 5.1.56 for `x > 1` (rational approximation of `x·e^x·E1(x)`).
/// Accuracy ~2×10⁻⁷ below 1 and ~5×10⁻⁵ above — plenty for an audio
/// gain formula.
fn expint_e1(x: f32) -> f32 {
    debug_assert!(x > 0.0);
    if x <= 1.0 {
        // -ln(x) + polynomial in x.
        const A: [f32; 6] = [
            -0.577_215_66,
            0.999_991_93,
            -0.249_910_55,
            0.055_199_68,
            -0.009_760_04,
            0.001_078_57,
        ];
        let mut poly = A[5];
        for &a in A[..5].iter().rev() {
            poly = poly * x + a;
        }
        // Above is equivalent to a0 + a1 x + a2 x² + … + a5 x⁵.
        -x.ln() + poly
    } else {
        // x·e^x·E1(x) ≈ (x² + a1 x + a2) / (x² + b1 x + b2)
        let a1 = 2.334_733;
        let a2 = 0.250_621;
        let b1 = 3.330_657;
        let b2 = 1.681_534;
        let num = x * x + a1 * x + a2;
        let den = x * x + b1 * x + b2;
        (num / den) * (-x).exp() / x
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{expint_e1, SpectralMethod, SpectralStage};
    use core::f32::consts::TAU;

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    }

    #[test]
    fn expint_matches_reference() {
        // Reference values from scipy.special.exp1:
        //   E1(0.1) = 1.8229239584193906
        //   E1(0.5) = 0.5597735947761609
        //   E1(1.0) = 0.21938393439552037
        //   E1(2.0) = 0.04890051070806112
        //   E1(5.0) = 0.0011482955912753257
        let cases = [
            (0.1_f32, 1.822_924),
            (0.5, 0.559_773_6),
            (1.0, 0.219_383_9),
            (2.0, 0.048_900_5),
            (5.0, 0.001_148_3),
        ];
        for (x, expected) in cases {
            let got = expint_e1(x);
            let rel = (got - expected).abs() / expected.abs();
            assert!(
                rel < 2e-3,
                "E1({x}) = {got}, expected {expected} (rel {rel})"
            );
        }
    }

    fn run_all(stage: &mut SpectralStage, input: &[f32]) -> Vec<f32> {
        let mut out = input.to_vec();
        stage.run(&mut out);
        out
    }

    #[test]
    fn boll_floor_one_is_passthrough() {
        // floor=1.0 pins the gain to unity everywhere, so the STFT
        // round-trip (sqrt-Hann analysis + synthesis, 50 % overlap =
        // full Hann product, COLA-satisfying) should reconstruct a
        // steady tone exactly in the steady state.
        let fs = 48_000.0_f32;
        let f = 1_000.0_f32;
        let input: Vec<f32> = (0..8192).map(|i| (TAU * f * i as f32 / fs).cos()).collect();
        let mut s = SpectralStage::new(SpectralMethod::Boll, 512, 1.5, 1.0, 0.05);
        let out = run_all(&mut s, &input);
        let warm = 1024;
        let ratio = rms(&out[warm..]) / rms(&input[warm..]);
        assert!(
            (0.95..=1.05).contains(&ratio),
            "passthrough ratio should be ~1.0, got {ratio}"
        );
    }

    #[test]
    fn boll_attenuates_white_noise() {
        let mut rng = 0xDEADBEEF_u32;
        let input: Vec<f32> = (0..32_768)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                (rng as f32 / u32::MAX as f32 - 0.5) * 0.2
            })
            .collect();
        let mut s = SpectralStage::new(SpectralMethod::Boll, 512, 2.0, 0.1, 0.1);
        let out = run_all(&mut s, &input);
        let warm = 2048;
        let r_in = rms(&input[warm..]);
        let r_out = rms(&out[warm..]);
        let db = 20.0 * (r_in / r_out).log10();
        assert!(db > 3.0, "expected ≥3 dB suppression, got {db:.1}");
    }

    #[test]
    fn mmse_lsa_attenuates_white_noise() {
        let mut rng = 0xCAFEBABE_u32;
        let input: Vec<f32> = (0..32_768)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                (rng as f32 / u32::MAX as f32 - 0.5) * 0.2
            })
            .collect();
        let mut s = SpectralStage::new(SpectralMethod::MmseLsa, 512, 1.0, 0.1, 0.1);
        let out = run_all(&mut s, &input);
        let warm = 2048;
        let r_in = rms(&input[warm..]);
        let r_out = rms(&out[warm..]);
        let db = 20.0 * (r_in / r_out).log10();
        assert!(db > 3.0, "expected ≥3 dB suppression, got {db:.1}");
    }

    #[test]
    fn state_persists_across_chunked_calls() {
        let input: Vec<f32> = (0..8192)
            .map(|i| (TAU * 1_000.0 * i as f32 / 48_000.0).cos())
            .collect();
        let mut whole = SpectralStage::new(SpectralMethod::Boll, 512, 1.5, 1.0, 0.05);
        let out_whole = run_all(&mut whole, &input);

        let mut split = SpectralStage::new(SpectralMethod::Boll, 512, 1.5, 1.0, 0.05);
        let mut a = input[..4096].to_vec();
        let mut b = input[4096..].to_vec();
        split.run(&mut a);
        split.run(&mut b);
        let mut joined = a;
        joined.extend_from_slice(&b);

        for i in 0..out_whole.len() {
            assert!(
                (out_whole[i] - joined[i]).abs() < 1e-4,
                "mismatch at {i}: whole={} split={}",
                out_whole[i],
                joined[i]
            );
        }
    }
}
