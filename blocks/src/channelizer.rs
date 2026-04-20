//! Frequency-shift + LPF + decimate in one block.
//!
//! The classic "single-channel tuner" used to pluck a narrowband signal
//! out of a wideband IQ stream. One IQ-in, one IQ-out, at rate
//! `input / factor`. Used as the server-side half of a VFO: the channel
//! at RF frequency `center + freq_shift_hz` lands at baseband after one
//! process pass.
//!
//! ### Algorithm
//!
//! ```text
//!                         ┌── e^(-j·2π·f_shift·n/fs) ──┐
//!         x[n] ────────►  │         mixer              │ ──► LPF ──► ↓M ──► y[m]
//!                         └────────────────────────────┘
//! ```
//!
//! - The mixer shifts the target frequency to DC. A phase accumulator
//!   wraps to `(-π, π]` each sample so we never lose precision to a
//!   growing counter, and `sincosf` runs per-sample — cheap enough for
//!   a few-MS/s input and avoids the magnitude drift of rotator
//!   multiplication.
//! - The LPF is the same Hann-windowed sinc used by [`crate::Decimator`],
//!   sized by `cutoff_normalized` relative to the input rate. The
//!   decimator invariant `total_out = total_in / factor` holds across
//!   arbitrary `process()` call sizes, same as Decimator.
//!
//! `freq_shift_hz` is the only param with `ReconfigureScope::SelfBlock`:
//! the VFO can retune without tearing down the pipeline.

use anyhow::{bail, Result};
use core::f32::consts::{PI, TAU};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct ChannelizerParams {
    /// Wideband input sample rate, Hz.
    pub input_rate_hz: f64,
    /// Frequency to pull to baseband, in Hz relative to the input's
    /// centre frequency. Positive values tune to RF above centre.
    pub freq_shift_hz: f64,
    /// Integer decimation factor M ≥ 1. Output rate = input / M.
    pub factor: usize,
    /// FIR length. Default `8·M+1`, matching `DecimatorParams`.
    pub num_taps: usize,
    /// LPF cutoff as a fraction of the **input** rate, 0 < fc < 0.5.
    /// Default `0.4/M` leaves a 20 % transition below output Nyquist.
    pub cutoff_normalized: f32,
}

impl ChannelizerParams {
    /// Sensible defaults for a given factor against a known input rate.
    #[must_use]
    pub fn new(input_rate_hz: f64, freq_shift_hz: f64, factor: usize) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let cutoff_normalized = if factor == 0 {
            0.4
        } else {
            0.4 / factor as f32
        };
        Self {
            input_rate_hz,
            freq_shift_hz,
            factor,
            num_taps: 8 * factor + 1,
            cutoff_normalized,
        }
    }
}

impl Default for ChannelizerParams {
    fn default() -> Self {
        // 2 MS/s wideband, centre of band, 4× decimation (500 kS/s out).
        // Matches the WBFM preset's channelizer so a flowgraph can omit
        // params entirely and still get a sensible tuner.
        Self::new(2_000_000.0, 0.0, 4)
    }
}

pub struct Channelizer {
    factor: usize,
    taps: Vec<f32>,
    /// Ring buffer of the last `num_taps` **mixed** samples. `delay_idx`
    /// is the next write slot, which after the write is the oldest entry.
    delay: Vec<Complex<f32>>,
    delay_idx: usize,
    /// Inputs modulo `factor`. Rolls to 0 → emit one output.
    decim_phase: usize,

    input_rate_hz: f64,
    /// Current mixer phase, radians, wrapped to `(-π, π]`.
    mix_phase: f32,
    /// Radians per sample; recomputed on `set_freq_shift`.
    mix_omega: f32,
}

impl Channelizer {
    /// Constructs a channelizer with the supplied params. Fails on
    /// `factor == 0`, `num_taps < 3`, non-positive input rate, or an
    /// out-of-range cutoff.
    pub fn new(params: ChannelizerParams) -> Result<Self> {
        if params.factor == 0 {
            bail!("channelizer factor must be >= 1");
        }
        if params.num_taps < 3 {
            bail!("channelizer num_taps must be >= 3");
        }
        if !(params.input_rate_hz.is_finite() && params.input_rate_hz > 0.0) {
            bail!(
                "channelizer input_rate_hz must be > 0, got {}",
                params.input_rate_hz
            );
        }
        if !(params.cutoff_normalized > 0.0 && params.cutoff_normalized < 0.5) {
            bail!(
                "channelizer cutoff_normalized must be in (0, 0.5), got {}",
                params.cutoff_normalized
            );
        }
        let taps = crate::decimator::design_lpf(params.num_taps, params.cutoff_normalized);
        let omega = omega_for(params.freq_shift_hz, params.input_rate_hz);
        Ok(Self {
            factor: params.factor,
            delay: vec![Complex::new(0.0, 0.0); taps.len()],
            taps,
            delay_idx: 0,
            decim_phase: 0,
            input_rate_hz: params.input_rate_hz,
            mix_phase: 0.0,
            mix_omega: omega,
        })
    }

    /// Retune the VFO without interrupting sample flow. The mixer phase
    /// stays continuous (no pop); only the per-sample increment changes.
    pub fn set_freq_shift(&mut self, freq_shift_hz: f64) {
        self.mix_omega = omega_for(freq_shift_hz, self.input_rate_hz);
    }

    #[must_use]
    pub const fn factor(&self) -> usize {
        self.factor
    }

    #[must_use]
    pub fn taps(&self) -> &[f32] {
        &self.taps
    }

    /// Current mixer frequency shift, Hz. Useful for tests and status.
    #[must_use]
    pub fn freq_shift_hz(&self) -> f64 {
        #[allow(clippy::cast_lossless)]
        let omega = self.mix_omega as f64;
        omega * self.input_rate_hz / -core::f64::consts::TAU
    }

    /// Convolve the delay line with the LPF at the current `delay_idx`.
    fn fir_output(&self) -> Complex<f32> {
        let n = self.taps.len();
        let mut acc = Complex::new(0.0_f32, 0.0);
        let mut idx = self.delay_idx;
        for &t in &self.taps {
            acc += self.delay[idx] * t;
            idx += 1;
            if idx == n {
                idx = 0;
            }
        }
        acc
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for Channelizer {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "Channelizer",
            placement: Placement::NativeOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[
                ParamSpec {
                    key: "freq_shift_hz",
                    label: "VFO offset",
                    kind: ParamKind::Range {
                        min: -1.0e9,
                        max: 1.0e9,
                        step: 1.0,
                        default: 0.0,
                        unit: "Hz",
                    },
                    // The only "tune knob" on the VFO — the whole point is
                    // to retune without restarting the graph.
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "factor",
                    label: "Decimation factor",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 1024.0,
                        step: 1.0,
                        default: 4.0,
                        unit: "",
                    },
                    // Changes output rate; downstream chain re-inits.
                    reconfig_scope: ReconfigureScope::Downstream,
                },
                ParamSpec {
                    key: "num_taps",
                    label: "FIR length",
                    kind: ParamKind::Range {
                        min: 3.0,
                        max: 2048.0,
                        step: 2.0,
                        default: 33.0,
                        unit: "taps",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                },
                ParamSpec {
                    key: "cutoff_normalized",
                    label: "Cutoff (× input rate)",
                    kind: ParamKind::Range {
                        min: 0.001,
                        max: 0.499,
                        step: 0.001,
                        default: 0.1,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                },
            ],
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        // If the scheduler knows the input rate, it wins — keeps the
        // mixer in sync even if construction-time params were stale.
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                let shift = self.freq_shift_hz();
                self.input_rate_hz = rate;
                self.mix_omega = omega_for(shift, rate);
            }
        }
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_iq_f32)
        else {
            return Ok(Work::new());
        };
        let Some(dst) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_iq_f32_mut)
        else {
            return Ok(Work::new());
        };

        let n = self.taps.len();
        let mut consumed = 0;
        let mut produced = 0;
        for &x in src {
            if self.decim_phase + 1 == self.factor && produced == dst.len() {
                break;
            }

            // Mix: y = x · e^(j·phase). mix_omega is already signed, so
            // positive freq_shift yields a negative omega and shifts the
            // input down in frequency by the expected amount.
            let (sin_p, cos_p) = self.mix_phase.sin_cos();
            let mixed = Complex::new(x.re * cos_p - x.im * sin_p, x.re * sin_p + x.im * cos_p);

            self.mix_phase += self.mix_omega;
            // Keep phase small for FP stability across long streams.
            if self.mix_phase > PI {
                self.mix_phase -= TAU;
            } else if self.mix_phase <= -PI {
                self.mix_phase += TAU;
            }

            self.delay[self.delay_idx] = mixed;
            self.delay_idx += 1;
            if self.delay_idx == n {
                self.delay_idx = 0;
            }
            consumed += 1;
            self.decim_phase += 1;
            if self.decim_phase == self.factor {
                self.decim_phase = 0;
                dst[produced] = self.fir_output();
                produced += 1;
            }
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;
        w.produced[0] = produced;
        Ok(w)
    }
}

impl BlockFactory for Channelizer {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: ChannelizerParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(Channelizer::new(p)?))
    }
}

fn omega_for(freq_shift_hz: f64, input_rate_hz: f64) -> f32 {
    // `exp(j·ω·n)` with ω = -2π·f/fs shifts a tone at +f down to DC.
    #[allow(clippy::cast_possible_truncation)]
    let omega = (-core::f64::consts::TAU * freq_shift_hz / input_rate_hz) as f32;
    omega
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
mod tests {
    use super::{Channelizer, ChannelizerParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;
    use num_complex::Complex;

    fn run(
        ch: &mut Channelizer,
        input: &[Complex<f32>],
        out_len: usize,
    ) -> (Vec<Complex<f32>>, usize, usize) {
        let mut out = vec![Complex::new(0.0, 0.0); out_len];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(input),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = ch.process(&mut io).unwrap();
        (out, w.consumed[0], w.produced[0])
    }

    #[test]
    fn constructor_rejects_bad_params() {
        assert!(Channelizer::new(ChannelizerParams::new(2.0e6, 0.0, 0)).is_err());
        assert!(Channelizer::new(ChannelizerParams {
            input_rate_hz: 2.0e6,
            freq_shift_hz: 0.0,
            factor: 4,
            num_taps: 2,
            cutoff_normalized: 0.1,
        })
        .is_err());
        assert!(Channelizer::new(ChannelizerParams {
            input_rate_hz: 0.0,
            freq_shift_hz: 0.0,
            factor: 4,
            num_taps: 33,
            cutoff_normalized: 0.1,
        })
        .is_err());
        assert!(Channelizer::new(ChannelizerParams {
            input_rate_hz: 2.0e6,
            freq_shift_hz: 0.0,
            factor: 4,
            num_taps: 33,
            cutoff_normalized: 0.5,
        })
        .is_err());
    }

    #[test]
    fn freq_shift_zero_passes_dc() {
        // freq_shift=0 reduces the channelizer to a plain decimator.
        let mut ch = Channelizer::new(ChannelizerParams::new(2.0e6, 0.0, 4)).unwrap();
        let input = vec![Complex::new(1.0_f32, 0.0); 8 * ch.taps().len()];
        let (out, _, produced) = run(&mut ch, &input, input.len() / 4 + 4);
        for s in &out[produced.saturating_sub(4)..produced] {
            assert!((s.re - 1.0).abs() < 1e-4, "re = {}", s.re);
            assert!(s.im.abs() < 1e-4, "im = {}", s.im);
        }
    }

    #[test]
    fn rate_invariant_holds_across_calls() {
        let mut ch = Channelizer::new(ChannelizerParams::new(2.0e6, 0.0, 5)).unwrap();
        let sizes = [0_usize, 1, 4, 5, 6, 9, 10, 17, 23, 31, 50];
        let mut total_in = 0;
        let mut total_out = 0;
        let input = vec![Complex::new(0.5_f32, -0.5); 64];
        for &n in &sizes {
            let (_, c, p) = run(&mut ch, &input[..n], n + 1);
            assert_eq!(c, n);
            total_in += n;
            total_out += p;
        }
        assert_eq!(total_out, total_in / 5);
    }

    #[test]
    fn tunes_a_tone_to_baseband() {
        // Input: complex exponential at +200 kHz against 2 MS/s.
        // After tuning to +200 kHz and decimating by 8 → output rate
        // 250 kS/s, the tone should land at DC.
        let fs = 2.0e6;
        let tone_hz = 200_000.0;
        let factor = 8;
        let params = ChannelizerParams {
            input_rate_hz: fs,
            freq_shift_hz: tone_hz,
            factor,
            num_taps: 129,
            cutoff_normalized: 0.4 / factor as f32,
        };
        let mut ch = Channelizer::new(params).unwrap();

        let n = 8192_usize;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = TAU * (tone_hz as f32 / fs as f32) * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let (out, _, produced) = run(&mut ch, &input, n / factor + 4);

        // Skip warmup: ~num_taps/factor outputs are transient.
        let warm = params.num_taps / params.factor + 4;
        assert!(produced > warm, "no steady-state samples produced");
        // The tone should be (nearly) a DC complex constant of magnitude 1
        // rotating slowly — after mixing, phase is ~constant, magnitude
        // is preserved by the unit-gain LPF.
        let avg_mag: f32 = out[warm..produced]
            .iter()
            .map(|c| (c.re * c.re + c.im * c.im).sqrt())
            .sum::<f32>()
            / (produced - warm) as f32;
        assert!(
            (avg_mag - 1.0).abs() < 0.02,
            "expected unit-magnitude tone at baseband, got {avg_mag}"
        );
    }

    #[test]
    fn out_of_band_tone_is_rejected() {
        // Tune to 0; input is a tone way outside the decimated passband.
        let fs = 2.0e6;
        let factor = 8;
        let params = ChannelizerParams {
            input_rate_hz: fs,
            freq_shift_hz: 0.0,
            factor,
            num_taps: 129,
            cutoff_normalized: 0.4 / factor as f32,
        };
        let mut ch = Channelizer::new(params).unwrap();

        // Tone at +600 kHz with fs=2 MS/s → normalized 0.3, well outside
        // the post-decim passband of ±125 kHz (0.0625 normalized).
        let tone_hz = 600_000.0;
        let n = 8192_usize;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = TAU * (tone_hz as f32 / fs as f32) * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let (out, _, produced) = run(&mut ch, &input, n / factor + 4);

        let warm = params.num_taps / params.factor + 4;
        let rms: f32 = (out[warm..produced]
            .iter()
            .map(|c| c.re * c.re + c.im * c.im)
            .sum::<f32>()
            / (produced - warm) as f32)
            .sqrt();
        assert!(rms < 0.05, "out-of-band tone leaked through: {rms}");
    }

    #[test]
    fn retune_mid_stream_is_continuous() {
        // Two-tone input: A at +100 kHz for first half, B at -150 kHz
        // for second half. Retune between halves and confirm both halves
        // demodulate to near-DC in their own windows.
        let fs = 2.0e6;
        let factor = 8;
        let params = ChannelizerParams {
            input_rate_hz: fs,
            freq_shift_hz: 100_000.0,
            factor,
            num_taps: 129,
            cutoff_normalized: 0.4 / factor as f32,
        };
        let mut ch = Channelizer::new(params).unwrap();

        let half = 4096_usize;
        let tone_a = 100_000.0_f32;
        let first: Vec<Complex<f32>> = (0..half)
            .map(|i| {
                let t = TAU * (tone_a / fs as f32) * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let (_a_out, _, p_a) = run(&mut ch, &first, half / factor + 4);

        ch.set_freq_shift(-150_000.0);

        let tone_b = -150_000.0_f32;
        let second: Vec<Complex<f32>> = (0..half)
            .map(|i| {
                let t = TAU * (tone_b / fs as f32) * (i + half) as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let (b_out, _, p_b) = run(&mut ch, &second, half / factor + 4);

        // First-half output should be finite (no NaN/Inf from the retune
        // boundary).
        for s in &b_out[..p_b] {
            assert!(s.re.is_finite() && s.im.is_finite());
        }
        assert!(p_a > 0 && p_b > 0);

        // Second-half settled magnitude should match baseband unit-tone.
        let warm = params.num_taps / params.factor + 4;
        let avg_mag: f32 = b_out[warm..p_b]
            .iter()
            .map(|c| (c.re * c.re + c.im * c.im).sqrt())
            .sum::<f32>()
            / (p_b - warm) as f32;
        assert!(
            (avg_mag - 1.0).abs() < 0.1,
            "expected unit mag after retune, got {avg_mag}"
        );
    }

    #[test]
    fn output_backpressure_pauses_cleanly() {
        let mut ch = Channelizer::new(ChannelizerParams::new(2.0e6, 0.0, 4)).unwrap();
        let input = vec![Complex::new(1.0_f32, 0.0); 40];
        let (_, consumed_a, produced_a) = run(&mut ch, &input, 2);
        assert_eq!(produced_a, 2);
        assert!(consumed_a <= input.len());
        let (_, consumed_b, produced_b) = run(&mut ch, &input[consumed_a..], 20);
        assert_eq!(produced_a + produced_b, 10);
        assert_eq!(consumed_a + consumed_b, input.len());
    }
}
