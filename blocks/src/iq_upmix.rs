//! IQ frequency shift — multiply a complex stream by a complex
//! exponential, sliding it `shift_hz` up (or down) the band.
//!
//! `iq[n] · e^{j·2π·shift·n/fs}`. Two uses, both in the end-to-end
//! tests:
//!
//! * **Upmix a recorded IQ capture.** The `samples/**/*-iq` clips are
//!   captured at baseband; shifting one off-centre forces the
//!   `Channelizer`'s tuner to actually find and extract it instead of
//!   trivially passing DC — the realistic case.
//! * **Place a modulator's carrier.** `AmModulator`/`FmModulator` →
//!   `IqUpmix` → `Channelizer` exercises the full tune-back path the
//!   live pipeline runs, not just modulator↔demod.
//!
//! Rate-aware: the shift tracks the actual input rate, read from
//! `InitCtx` at init / `update_rates` (the `sample_rate_hz` param is
//! only the standalone-construction hint, as in `FmDemod`).
//!
//! Same 1:1-rate, same-type contract as any pass-through; the phase
//! accumulator is an `f64` in cycles, wrapped to `[0, 1)` each sample.

use anyhow::{bail, Result};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct IqUpmixParams {
    /// Frequency shift (Hz). Positive slides the spectrum up.
    pub shift_hz: f32,
    /// Input IQ rate (Hz). Construction-time hint; `ctx.input_rate`
    /// wins once the runtime populates it.
    pub sample_rate_hz: f32,
}

impl Default for IqUpmixParams {
    fn default() -> Self {
        Self {
            shift_hz: 0.0,
            sample_rate_hz: 2_400_000.0,
        }
    }
}

pub struct IqUpmix {
    params: IqUpmixParams,
    /// Carrier phase in cycles, wrapped to `[0, 1)`.
    phase: f64,
    /// `shift / input_rate`, per-sample phase increment in cycles.
    inc_cyc: f64,
    input_rate_hz: f64,
}

impl IqUpmix {
    pub fn new(params: IqUpmixParams) -> Result<Self> {
        if !params.shift_hz.is_finite() {
            bail!("iq_upmix shift_hz must be finite");
        }
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!("iq_upmix sample_rate_hz must be > 0");
        }
        let mut s = Self {
            params,
            phase: 0.0,
            inc_cyc: 0.0,
            input_rate_hz: 0.0,
        };
        s.set_input_rate(f64::from(params.sample_rate_hz));
        Ok(s)
    }

    fn set_input_rate(&mut self, input_rate_hz: f64) {
        self.input_rate_hz = input_rate_hz;
        self.inc_cyc = f64::from(self.params.shift_hz) / input_rate_hz;
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for IqUpmix {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "IqUpmix",
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
                    key: "shift_hz",
                    label: "Frequency shift",
                    kind: ParamKind::Range {
                        min: -50_000_000.0,
                        max: 50_000_000.0,
                        step: 1.0,
                        default: 0.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Spectrum shift. Positive = up. Keep the shifted signal inside the band (|shift| + signal bandwidth < fs/2).",
                },
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Input sample rate",
                    kind: ParamKind::Range {
                        min: 1_000.0,
                        max: 100_000_000.0,
                        step: 1.0,
                        default: 2_400_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Standalone-construction rate hint; the runtime's propagated input rate wins.",
                },
            ],
            ai_notes: "IQ frequency-shift (complex NCO mixer). Loopback/test only: upmix a recorded IQ capture, or place a modulator's carrier so the Channelizer must tune it back. Not in any listen preset.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.set_input_rate(rate);
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.set_input_rate(rate);
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

        let n = src.len().min(dst.len());
        for i in 0..n {
            #[allow(clippy::cast_possible_truncation)]
            let theta = (self.phase as f32) * core::f32::consts::TAU;
            let (s, c) = theta.sin_cos();
            let rot = Complex::new(c, s);
            dst[i] = src[i] * rot;
            self.phase += self.inc_cyc;
            self.phase -= self.phase.floor();
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for IqUpmix {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: IqUpmixParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(IqUpmix::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{IqUpmix, IqUpmixParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use num_complex::Complex;

    fn run(m: &mut IqUpmix, input: &[Complex<f32>]) -> Vec<Complex<f32>> {
        let mut out = vec![Complex::new(0.0_f32, 0.0); input.len()];
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
        let w = m.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], input.len());
        out
    }

    #[test]
    fn rejects_bad_params() {
        assert!(IqUpmix::new(IqUpmixParams {
            shift_hz: f32::NAN,
            ..Default::default()
        })
        .is_err());
        assert!(IqUpmix::new(IqUpmixParams {
            shift_hz: 0.0,
            sample_rate_hz: 0.0,
        })
        .is_err());
    }

    #[test]
    fn zero_shift_is_identity() {
        let mut m = IqUpmix::new(IqUpmixParams {
            shift_hz: 0.0,
            sample_rate_hz: 48_000.0,
        })
        .unwrap();
        let input: Vec<Complex<f32>> = (0..32)
            .map(|i| Complex::new((i as f32 * 0.1).cos(), (i as f32 * 0.2).sin()))
            .collect();
        let out = run(&mut m, &input);
        for (a, b) in input.iter().zip(out.iter()) {
            assert!((a.re - b.re).abs() < 1e-5 && (a.im - b.im).abs() < 1e-5);
        }
    }

    #[test]
    fn shifts_a_dc_tone_to_the_shift_frequency() {
        // A DC input (constant 1+0j) shifted by +1 kHz at 48 kHz becomes
        // a complex tone whose per-sample phase advance is 2π·1k/48k.
        let fs = 48_000.0_f32;
        let shift = 1_000.0_f32;
        let mut m = IqUpmix::new(IqUpmixParams {
            shift_hz: shift,
            sample_rate_hz: fs,
        })
        .unwrap();
        let input = vec![Complex::new(1.0_f32, 0.0); 256];
        let out = run(&mut m, &input);
        let expected = core::f32::consts::TAU * shift / fs;
        let dphase = (out[101] / out[100]).arg();
        assert!(
            (dphase - expected).abs() < 1e-4,
            "phase advance {dphase}, want {expected}"
        );
        // Constant envelope preserved.
        for s in &out {
            assert!(((s.re * s.re + s.im * s.im).sqrt() - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn state_persists_across_calls() {
        let p = IqUpmixParams {
            shift_hz: 700.0,
            sample_rate_hz: 8_000.0,
        };
        let input: Vec<Complex<f32>> = (0..64)
            .map(|i| Complex::new((i as f32 * 0.05).cos(), 0.0))
            .collect();
        let mut whole = IqUpmix::new(p).unwrap();
        let ow = run(&mut whole, &input);
        let mut split = IqUpmix::new(p).unwrap();
        let a = run(&mut split, &input[..30]);
        let b = run(&mut split, &input[30..]);
        for i in 0..30 {
            assert!((a[i].re - ow[i].re).abs() < 1e-5 && (a[i].im - ow[i].im).abs() < 1e-5);
        }
        for i in 0..input.len() - 30 {
            assert!(
                (b[i].re - ow[30 + i].re).abs() < 1e-5 && (b[i].im - ow[30 + i].im).abs() < 1e-5
            );
        }
    }
}
