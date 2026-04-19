//! FM demodulator — complex IQ → real audio, phase-discriminator style.
//!
//! One IQ-in, one Real-out, same sample rate. Input iq baseband at rate
//! `fs`; output a real signal whose instantaneous value is the normalised
//! phase derivative of the input, scaled so a full ±`max_deviation_hz`
//! FM swing maps to ±1.0.
//!
//! ### Algorithm
//!
//! For each sample `x[n]`, compute `y[n] = angle(x[n] · conj(x[n-1]))`.
//! That is the phase advance per sample of the complex input, in radians,
//! and equals `2π · f_inst / fs` where `f_inst` is the current audio-band
//! message frequency. Rescaling by `fs / (2π · max_deviation_hz)` yields
//! a unit-range audio output.
//!
//! The discriminator is stateless except for `prev`, the last input
//! sample. Keeping it across `process` calls is what makes successive
//! calls equivalent to one long call.
//!
//! ### Rationale
//!
//! Phase discrimination is the simplest correct FM demod. It is numerically
//! robust (`atan2` handles the ±π wrap without branching), matches a
//! textbook definition, and leaves downstream blocks (decimator, de-emph,
//! audio sink) responsible for rate and spectral shaping. Polyphase
//! rate-conversion and FIR de-emphasis land as separate blocks.

use anyhow::{bail, Result};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, Work,
};

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct FmDemodParams {
    /// Input IQ sample rate (Hz). Wired in by the scheduler from the input
    /// port's metadata; set explicitly for standalone tests.
    pub sample_rate_hz: f32,
    /// Peak frequency deviation of the FM signal (Hz). WBFM broadcast =
    /// 75 000; NBFM voice ≈ 5 000. Audio full-scale maps to this.
    pub max_deviation_hz: f32,
}

impl Default for FmDemodParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 240_000.0,
            max_deviation_hz: 75_000.0,
        }
    }
}

pub struct FmDemod {
    gain: f32,
    prev: Complex<f32>,
}

impl FmDemod {
    /// Builds a demod with the supplied params. Fails on non-positive
    /// rates or deviation.
    pub fn new(params: FmDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "fm_demod sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        if !(params.max_deviation_hz.is_finite() && params.max_deviation_hz > 0.0) {
            bail!(
                "fm_demod max_deviation_hz must be > 0 (got {})",
                params.max_deviation_hz
            );
        }
        let gain = params.sample_rate_hz / (core::f32::consts::TAU * params.max_deviation_hz);
        Ok(Self {
            gain,
            prev: Complex::new(0.0, 0.0),
        })
    }

    /// Discriminator scaling used by `process`: audio = `atan2(...) * gain`.
    #[must_use]
    pub const fn gain(&self) -> f32 {
        self.gain
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for FmDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "FmDemod",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::RealF32,
            }],
            params: &[
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Input sample rate",
                    kind: ParamKind::Range {
                        min: 1_000.0,
                        max: 10_000_000.0,
                        step: 1.0,
                        default: 240_000.0,
                        unit: "Hz",
                    },
                    mutable_while_streaming: false,
                },
                ParamSpec {
                    key: "max_deviation_hz",
                    label: "Peak deviation",
                    kind: ParamKind::Range {
                        min: 100.0,
                        max: 200_000.0,
                        step: 100.0,
                        default: 75_000.0,
                        unit: "Hz",
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
            .and_then(OutputPort::as_real_f32_mut)
        else {
            return Ok(Work::new());
        };

        let n = src.len().min(dst.len());
        for i in 0..n {
            let x = src[i];
            let p = x * self.prev.conj();
            dst[i] = p.im.atan2(p.re) * self.gain;
            self.prev = x;
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for FmDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: FmDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(FmDemod::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{FmDemod, FmDemodParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;
    use num_complex::Complex;

    fn run(demod: &mut FmDemod, input: &[Complex<f32>]) -> Vec<f32> {
        let mut out = vec![0.0_f32; input.len()];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(input),
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
        let w = demod.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], input.len());
        assert_eq!(w.produced[0], input.len());
        out
    }

    #[test]
    fn constructor_rejects_bad_params() {
        assert!(FmDemod::new(FmDemodParams {
            sample_rate_hz: 0.0,
            max_deviation_hz: 75_000.0,
        })
        .is_err());
        assert!(FmDemod::new(FmDemodParams {
            sample_rate_hz: 240_000.0,
            max_deviation_hz: 0.0,
        })
        .is_err());
        assert!(FmDemod::new(FmDemodParams {
            sample_rate_hz: f32::NAN,
            max_deviation_hz: 75_000.0,
        })
        .is_err());
    }

    #[test]
    fn dc_input_yields_zero_output() {
        // A constant IQ value is DC — phase derivative is exactly zero
        // once we've seen one sample. The very first sample compares
        // against prev = (0, 0) and produces atan2(0, 0) = 0 too.
        let mut demod = FmDemod::new(FmDemodParams::default()).unwrap();
        let input = vec![Complex::new(0.7_f32, -0.3); 128];
        let out = run(&mut demod, &input);
        for (i, y) in out.iter().enumerate() {
            assert!(y.abs() < 1e-6, "out[{i}] = {y}");
        }
    }

    #[test]
    fn tone_at_half_deviation_yields_half_scale() {
        // Complex tone at f_test Hz → constant audio output = f_test / max_deviation.
        // Using f_test = +deviation/2 exactly should give +0.5, well-scaled.
        let params = FmDemodParams {
            sample_rate_hz: 240_000.0,
            max_deviation_hz: 75_000.0,
        };
        let f_test = params.max_deviation_hz / 2.0;
        let phase_step = TAU * f_test / params.sample_rate_hz;
        let n = 256;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = phase_step * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let mut demod = FmDemod::new(params).unwrap();
        let out = run(&mut demod, &input);
        // Skip sample 0 (prev was zero on first call).
        for (i, y) in out.iter().enumerate().skip(1) {
            assert!((y - 0.5).abs() < 1e-4, "out[{i}] = {y}");
        }
    }

    #[test]
    fn negative_deviation_yields_negative_output() {
        // Same as above but going the other way around the circle.
        let params = FmDemodParams {
            sample_rate_hz: 240_000.0,
            max_deviation_hz: 75_000.0,
        };
        let phase_step = -TAU * params.max_deviation_hz / params.sample_rate_hz;
        let n = 128;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = phase_step * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let mut demod = FmDemod::new(params).unwrap();
        let out = run(&mut demod, &input);
        for (i, y) in out.iter().enumerate().skip(1) {
            assert!((y + 1.0).abs() < 1e-4, "out[{i}] = {y}");
        }
    }

    #[test]
    fn prev_sample_persists_across_process_calls() {
        // A tone split across two calls must produce the same steady
        // output as one call of the full block — the join point would
        // show a glitch if `prev` were not carried over.
        let params = FmDemodParams {
            sample_rate_hz: 240_000.0,
            max_deviation_hz: 75_000.0,
        };
        let f_test = params.max_deviation_hz / 4.0;
        let phase_step = TAU * f_test / params.sample_rate_hz;
        let total = 64;
        let input: Vec<Complex<f32>> = (0..total)
            .map(|i| {
                let t = phase_step * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();

        let mut whole = FmDemod::new(params).unwrap();
        let out_whole = run(&mut whole, &input);

        let mut split = FmDemod::new(params).unwrap();
        let first = run(&mut split, &input[..32]);
        let second = run(&mut split, &input[32..]);
        let mut out_split = first;
        out_split.extend_from_slice(&second);

        for (i, (a, b)) in out_whole.iter().zip(out_split.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "mismatch at {i}: whole={a} split={b}");
        }
    }
}
