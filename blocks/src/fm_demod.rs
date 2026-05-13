//! FM demodulator — complex IQ → real audio, backed by liquid-dsp's
//! `freqdem` (multi-stage phase discriminator with internal filtering).
//!
//! One IQ-in, one Real-out, same sample rate. Input IQ baseband at rate
//! `fs`; output a real signal scaled so a carrier modulated at
//! ±`max_deviation_hz` peak lands at output amplitude ±1.0. The block is
//! rate-aware: it reads its actual input rate from `InitCtx` at init
//! and rebuilds the demod's modulation factor `kf = max_dev/fs` on
//! any live source-rate change.
//!
//! ### Why liquid
//!
//! The previous hand-rolled version did plain `atan2(x · conj(prev))`,
//! which works but has no internal filtering — every bit of out-of-band
//! noise above the demod's passband leaks straight into audio. Liquid's
//! `freqdem` uses a validated discriminator + integrator that handles
//! the ±π phase wrap, DC offset, and transient noise more gracefully.
//! Same 1:1 rate contract; the block surface is unchanged.
//!
//! `freqdem_create` wants `kf ∈ (0, 0.5)` (fraction of sample rate).
//! Broadcast FM at 240 kS/s with ±75 kHz peak is `kf = 0.3125`; NBFM
//! at 50 kS/s with ±5 kHz is `kf = 0.1`. The constructor validates the
//! ratio and rejects anything liquid would refuse.

use anyhow::{bail, Result};
use ferrite_liquid_dsp::FreqDem;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct FmDemodParams {
    /// Input IQ sample rate (Hz). Construction-time hint; init's
    /// `ctx.input_rate("in")` wins once the runtime populates it.
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
    params: FmDemodParams,
    /// Liquid discriminator. Rebuilt whenever `kf` changes (via init or
    /// update_rates). `None` between construction and init when the
    /// rate can't be resolved yet.
    inner: Option<FreqDem>,
    /// Last-applied input sample rate. Tracked so `update_rates` can
    /// skip the rebuild when the scheduler's propagation pass reports
    /// an identical rate.
    input_rate_hz: f64,
}

impl FmDemod {
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
        let mut s = Self {
            params,
            inner: None,
            input_rate_hz: 0.0,
        };
        // Eagerly realize for the construction-time hint rate so the
        // block is usable standalone (tests); init will rebuild if
        // the scheduler reports a different rate.
        s.rebuild_for_input_rate(f64::from(params.sample_rate_hz))?;
        Ok(s)
    }

    fn rebuild_for_input_rate(&mut self, input_rate_hz: f64) -> Result<()> {
        if !(input_rate_hz > 0.0) {
            bail!("fm_demod: need positive input rate, got {input_rate_hz}");
        }
        let kf = (f64::from(self.params.max_deviation_hz) / input_rate_hz) as f32;
        if !(kf > 0.0 && kf < 0.5) {
            bail!(
                "fm_demod: kf = max_deviation / input_rate = {kf} must be in (0, 0.5); \
                 deviation {} Hz at input rate {input_rate_hz} Hz violates Nyquist",
                self.params.max_deviation_hz,
            );
        }
        self.inner = Some(FreqDem::new(kf).map_err(|e| anyhow::anyhow!("freqdem create: {e}"))?);
        self.input_rate_hz = input_rate_hz;
        Ok(())
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
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Demod input rate (typically the channelizer's output). Higher rate = more SNR margin but more CPU.",
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
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "Maximum expected FM peak deviation. WBFM broadcast = 75 kHz, NBFM voice = 2.5–5 kHz, NWR weather = 5 kHz. Too low = clipping at peaks; too high = lower audio output.",
                },
            ],
            ai_notes: "FM demodulator (atan2 discriminator). Outputs the instantaneous frequency as real-valued audio. Use for WBFM broadcast, NBFM voice (ham 2m/70cm, marine VHF, weather), and any other FM mode — `max_deviation_hz` is the discriminator.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.rebuild_for_input_rate(rate)?;
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.rebuild_for_input_rate(rate)?;
            }
        }
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(Work::new());
        };
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
            // Clamp to ±1 — anything past max_deviation is noise (random
            // phase ramps up to ±π → output ±fs/(2·max_dev) which can be
            // many ×). Letting that through saturates downstream listeners
            // (FileAudioSink → i16 clip) and obscures real bursts of
            // narrowband signal that *do* sit inside ±max_dev.
            dst[i] = inner.demodulate(x.re, x.im).clamp(-1.0, 1.0);
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
        // Deviation > 0.5·fs violates Nyquist; liquid's kf must be < 0.5.
        assert!(FmDemod::new(FmDemodParams {
            sample_rate_hz: 100_000.0,
            max_deviation_hz: 75_000.0,
        })
        .is_err());
    }

    #[test]
    fn dc_input_yields_zero_output() {
        // A constant IQ value is DC — phase derivative is exactly zero.
        // Liquid's freqdem has a small internal transient but settles
        // to zero within a few samples.
        let mut demod = FmDemod::new(FmDemodParams::default()).unwrap();
        let input = vec![Complex::new(0.7_f32, -0.3); 256];
        let out = run(&mut demod, &input);
        // Skip the filter's settling transient.
        for (i, y) in out.iter().enumerate().skip(32) {
            assert!(y.abs() < 1e-3, "out[{i}] = {y}");
        }
    }

    #[test]
    fn tone_at_half_deviation_yields_half_scale() {
        // Complex tone at +deviation/2 → steady output near +0.5.
        let params = FmDemodParams {
            sample_rate_hz: 240_000.0,
            max_deviation_hz: 75_000.0,
        };
        let f_test = params.max_deviation_hz / 2.0;
        let phase_step = TAU * f_test / params.sample_rate_hz;
        let n = 512;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = phase_step * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let mut demod = FmDemod::new(params).unwrap();
        let out = run(&mut demod, &input);
        // Skip filter settling.
        let warm = 64;
        let mean: f32 = out[warm..].iter().sum::<f32>() / (n - warm) as f32;
        assert!((mean - 0.5).abs() < 0.02, "expected ~0.5, got mean {mean}");
    }

    #[test]
    fn negative_deviation_yields_negative_output() {
        let params = FmDemodParams {
            sample_rate_hz: 240_000.0,
            max_deviation_hz: 75_000.0,
        };
        let phase_step = -TAU * (params.max_deviation_hz / 2.0) / params.sample_rate_hz;
        let n = 512;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = phase_step * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let mut demod = FmDemod::new(params).unwrap();
        let out = run(&mut demod, &input);
        let warm = 64;
        let mean: f32 = out[warm..].iter().sum::<f32>() / (n - warm) as f32;
        assert!((mean + 0.5).abs() < 0.02, "expected ~-0.5, got mean {mean}");
    }

    #[test]
    fn state_persists_across_process_calls() {
        // Split a steady tone across two calls; output of the second
        // half should continue the steady state of the first, modulo
        // the tiny initial-transient spike on call #1 not repeating.
        let params = FmDemodParams {
            sample_rate_hz: 240_000.0,
            max_deviation_hz: 75_000.0,
        };
        let f_test = params.max_deviation_hz / 4.0;
        let phase_step = TAU * f_test / params.sample_rate_hz;
        let total = 256;
        let input: Vec<Complex<f32>> = (0..total)
            .map(|i| {
                let t = phase_step * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();

        let mut split = FmDemod::new(params).unwrap();
        let _ = run(&mut split, &input[..128]);
        let second = run(&mut split, &input[128..]);
        // After the first half warmed the filter, the second half
        // should hold 0.25 steadily.
        let mean: f32 = second.iter().sum::<f32>() / second.len() as f32;
        assert!(
            (mean - 0.25).abs() < 0.02,
            "expected ~0.25 across the split, got {mean}"
        );
    }
}
