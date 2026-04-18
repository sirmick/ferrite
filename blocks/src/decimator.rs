//! Integer-factor decimator with a windowed-sinc anti-aliasing LPF.
//!
//! One IQ-in, one IQ-out. Takes every Mth filtered sample, where the
//! filter removes everything above the output Nyquist so the decimation
//! does not alias.
//!
//! ### Filter
//!
//! Direct-form FIR with real coefficients applied to complex samples
//! (taps multiply `re` and `im` identically). Coefficients are a
//! Hann-windowed sinc, normalised so DC gain = 1. This is simple, cheap
//! to understand, and good enough for Phase B/D use; swapping in a
//! polyphase decomposition or a Kaiser design can happen if profiling
//! says it matters.
//!
//! ### State
//!
//! A delay line of `num_taps` complex samples (ring buffer) and a
//! modulo-`factor` phase counter. The phase counter emits an output
//! on every `factor`-th input, which preserves sample-rate semantics
//! across [`Block::process`] calls of arbitrary size (and across a
//! single call whose output buffer fills before all inputs are
//! consumed — unread inputs stay in the delay line).

use anyhow::{bail, Result};
use num_complex::Complex;

use crate::block::{
    Block, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, Work,
};

/// Construction-time params.
#[derive(Debug, Clone, Copy)]
pub struct DecimatorParams {
    /// Decimation factor M ≥ 1. Output rate = input rate / M.
    pub factor: usize,
    /// FIR length. Odd gives linear phase; even works too. Default 8·M+1.
    pub num_taps: usize,
    /// LPF cutoff as a fraction of the **input** sample rate, 0 < fc < 0.5.
    /// Default `0.4 / factor` — leaves a 20 % transition band below the
    /// output Nyquist.
    pub cutoff_normalized: f32,
}

impl DecimatorParams {
    /// Builds a parameter set with sensible defaults for a given factor.
    #[must_use]
    pub fn for_factor(factor: usize) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let cutoff_normalized = if factor == 0 {
            0.4
        } else {
            0.4 / factor as f32
        };
        Self {
            factor,
            num_taps: 8 * factor + 1,
            cutoff_normalized,
        }
    }
}

impl Default for DecimatorParams {
    fn default() -> Self {
        Self::for_factor(4)
    }
}

pub struct Decimator {
    factor: usize,
    taps: Vec<f32>,
    /// Ring buffer of the last `num_taps` input samples. `delay_idx` is
    /// the position where the next input sample will be written — i.e.
    /// the oldest sample sits at `delay_idx` immediately after a write.
    delay: Vec<Complex<f32>>,
    delay_idx: usize,
    /// Counts inputs modulo `factor`. When it rolls to 0 we emit.
    phase: usize,
}

impl Decimator {
    /// Constructs a decimator with the supplied params. Fails on
    /// `factor == 0`, `num_taps < 3`, or out-of-range cutoff.
    pub fn new(params: DecimatorParams) -> Result<Self> {
        if params.factor == 0 {
            bail!("decimator factor must be >= 1");
        }
        if params.num_taps < 3 {
            bail!("decimator num_taps must be >= 3");
        }
        if !(params.cutoff_normalized > 0.0 && params.cutoff_normalized < 0.5) {
            bail!(
                "decimator cutoff_normalized must be in (0, 0.5), got {}",
                params.cutoff_normalized
            );
        }
        let taps = design_lpf(params.num_taps, params.cutoff_normalized);
        Ok(Self {
            factor: params.factor,
            delay: vec![Complex::new(0.0, 0.0); taps.len()],
            taps,
            delay_idx: 0,
            phase: 0,
        })
    }

    /// FIR taps — exposed for tests; consumers should go through `process`.
    #[must_use]
    pub fn taps(&self) -> &[f32] {
        &self.taps
    }

    #[must_use]
    pub const fn factor(&self) -> usize {
        self.factor
    }

    /// Convolve the delay line with the filter at the current delay-idx.
    /// The oldest sample sits at `delay_idx` (just-overwritten position),
    /// so walking forward from there iterates old → new.
    fn fir_output(&self) -> Complex<f32> {
        let n = self.taps.len();
        let mut acc = Complex::new(0.0_f32, 0.0);
        let mut idx = self.delay_idx;
        // taps[0] is the coefficient for the oldest sample.
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

impl Block for Decimator {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "Decimator",
            placement: Placement::Either,
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
                    key: "factor",
                    label: "Decimation factor",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 1024.0,
                        step: 1.0,
                        default: 4.0,
                        unit: "",
                    },
                    mutable_while_streaming: false,
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
                    mutable_while_streaming: false,
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
                    mutable_while_streaming: false,
                },
            ],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
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
            // Would the next output overflow the output buffer? Stop
            // before consuming the input so the scheduler can call us
            // again when there's space.
            if self.phase + 1 == self.factor && produced == dst.len() {
                break;
            }
            self.delay[self.delay_idx] = x;
            self.delay_idx += 1;
            if self.delay_idx == n {
                self.delay_idx = 0;
            }
            consumed += 1;
            self.phase += 1;
            if self.phase == self.factor {
                self.phase = 0;
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

/// Windowed-sinc low-pass filter. `cutoff` is the normalised cutoff in
/// `(0, 0.5)` (fraction of sample rate). Applies a Hann window and
/// normalises so the taps sum to 1 (unit DC gain).
fn design_lpf(num_taps: usize, cutoff: f32) -> Vec<f32> {
    use core::f32::consts::PI;
    #[allow(clippy::cast_precision_loss)]
    let m = (num_taps - 1) as f32 / 2.0;
    let mut h = Vec::with_capacity(num_taps);
    for n in 0..num_taps {
        #[allow(clippy::cast_precision_loss)]
        let k = n as f32 - m;
        let ideal = if k.abs() < 1e-7 {
            2.0 * cutoff
        } else {
            (2.0 * PI * cutoff * k).sin() / (PI * k)
        };
        #[allow(clippy::cast_precision_loss)]
        let window_phase = 2.0 * PI * n as f32 / (num_taps - 1) as f32;
        let window = 0.5 * (1.0 - window_phase.cos());
        h.push(ideal * window);
    }
    let sum: f32 = h.iter().sum();
    for x in &mut h {
        *x /= sum;
    }
    h
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{design_lpf, Decimator, DecimatorParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;
    use num_complex::Complex;

    fn run(
        dec: &mut Decimator,
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
        let w = dec.process(&mut io).unwrap();
        (out, w.consumed[0], w.produced[0])
    }

    #[test]
    fn lpf_has_unit_dc_gain() {
        // Hann-sinc taps normalised to sum=1 — DC in at 1.0 yields DC out
        // at 1.0 after the filter has warmed up.
        let taps = design_lpf(33, 0.1);
        let sum: f32 = taps.iter().sum();
        assert!((sum - 1.0).abs() < 1e-6, "sum = {sum}");
    }

    #[test]
    fn constructor_rejects_bad_params() {
        assert!(Decimator::new(DecimatorParams {
            factor: 0,
            num_taps: 33,
            cutoff_normalized: 0.1
        })
        .is_err());
        assert!(Decimator::new(DecimatorParams {
            factor: 2,
            num_taps: 2,
            cutoff_normalized: 0.1
        })
        .is_err());
        assert!(Decimator::new(DecimatorParams {
            factor: 2,
            num_taps: 33,
            cutoff_normalized: 0.0
        })
        .is_err());
        assert!(Decimator::new(DecimatorParams {
            factor: 2,
            num_taps: 33,
            cutoff_normalized: 0.5
        })
        .is_err());
    }

    #[test]
    fn dc_passes_with_unit_gain() {
        let mut dec = Decimator::new(DecimatorParams::for_factor(4)).unwrap();
        // 4× more input than taps so the filter is in steady state.
        let input = vec![Complex::new(1.0_f32, 0.0); 4 * dec.taps().len()];
        let (out, _, produced) = run(&mut dec, &input, input.len() / 4 + 4);
        // Late samples (past warmup) should equal (1, 0) within FP.
        for s in &out[out.len() - 4..produced.min(out.len())] {
            assert!((s.re - 1.0).abs() < 1e-4, "re = {}", s.re);
            assert!(s.im.abs() < 1e-4, "im = {}", s.im);
        }
    }

    #[test]
    fn rate_invariant_holds_across_calls() {
        // For any sequence of calls with total input N, total output
        // equals floor((N + initial_phase_debt) / factor). We start with
        // phase=0 so total_output = floor(N / factor).
        let mut dec = Decimator::new(DecimatorParams::for_factor(3)).unwrap();
        let chunks: [usize; 6] = [1, 2, 3, 4, 7, 11];
        let total_in: usize = chunks.iter().sum();
        let mut total_out = 0;
        for &n in &chunks {
            let input = vec![Complex::new(0.5, -0.5); n];
            // Oversize output so backpressure never triggers.
            let (_, consumed, produced) = run(&mut dec, &input, n + 1);
            assert_eq!(consumed, n);
            total_out += produced;
        }
        assert_eq!(total_out, total_in / 3);
    }

    #[test]
    fn rate_invariant_for_random_sized_chunks() {
        // Deterministic LCG-ish sequence of chunk sizes covering both
        // sub-factor and multi-factor slices. Tests the phase-preservation
        // across call boundaries.
        let mut dec = Decimator::new(DecimatorParams::for_factor(5)).unwrap();
        let sizes = [0_usize, 1, 4, 5, 6, 9, 10, 17, 23, 31, 50];
        let mut total_in = 0;
        let mut total_out = 0;
        let input = vec![Complex::new(0.0_f32, 0.0); 64];
        for &n in &sizes {
            let (_, c, p) = run(&mut dec, &input[..n], n + 1);
            assert_eq!(c, n);
            total_in += n;
            total_out += p;
        }
        assert_eq!(total_out, total_in / 5);
    }

    #[test]
    fn in_band_tone_survives_out_of_band_tone_is_rejected() {
        // factor = 4: output Nyquist is at 0.125 of input rate.
        // In-band tone at 0.05 should survive; out-of-band at 0.3 should
        // be attenuated by a lot.
        let params = DecimatorParams {
            factor: 4,
            num_taps: 65,
            cutoff_normalized: 0.11,
        };

        let measure_gain = |tone_hz_norm: f32| -> f32 {
            let mut dec = Decimator::new(params).unwrap();
            let n = 4096_usize;
            let input: Vec<Complex<f32>> = (0..n)
                .map(|i| {
                    let t = TAU * tone_hz_norm * i as f32;
                    Complex::new(t.cos(), t.sin())
                })
                .collect();
            let (out, _, p) = run(&mut dec, &input, n / 4 + 4);
            // Skip warmup: first ~taps/factor outputs are transient.
            let warm = params.num_taps / params.factor + 4;
            let rms: f32 = out[warm..p]
                .iter()
                .map(|c| c.re * c.re + c.im * c.im)
                .sum::<f32>()
                / (p - warm) as f32;
            rms.sqrt()
        };

        let in_band = measure_gain(0.05);
        let out_of_band = measure_gain(0.30);
        assert!(
            in_band > 0.9,
            "in-band tone attenuated: {in_band} (expected > 0.9)"
        );
        assert!(
            out_of_band < 0.05,
            "out-of-band tone leaked through: {out_of_band} (expected < 0.05)"
        );
    }

    #[test]
    fn output_backpressure_pauses_cleanly() {
        // If the output buffer is smaller than what the input produces,
        // the block consumes exactly enough input to fill the output and
        // leaves the rest in its delay line.
        let mut dec = Decimator::new(DecimatorParams::for_factor(4)).unwrap();
        let input = vec![Complex::new(1.0_f32, 0.0); 40];
        let (_, consumed_a, produced_a) = run(&mut dec, &input, 2);
        assert_eq!(produced_a, 2);
        assert!(consumed_a <= input.len());
        // Next call picks up with the remaining phase state.
        let (_, consumed_b, produced_b) = run(&mut dec, &input[consumed_a..], 20);
        // Total outputs over two calls = 40 / 4 = 10.
        assert_eq!(produced_a + produced_b, 10);
        assert_eq!(consumed_a + consumed_b, input.len());
    }
}
