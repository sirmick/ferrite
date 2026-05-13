//! AM demodulator — complex IQ → real audio, backed by liquid-dsp's
//! `ampmodem` in DSB mode with carrier-present recovery.
//!
//! One IQ-in, one Real-out, same sample rate. Input IQ baseband centred
//! on the AM carrier (i.e. upstream mixer has already pulled the carrier
//! to DC); output a real audio signal.
//!
//! ### Coherent vs envelope detection
//!
//! The previous hand-rolled version was a classic envelope detector:
//! take `|x|` and subtract a tracking DC bias to remove the unmodulated
//! carrier. Worked, but was vulnerable to AGC pumping and noise —
//! every out-of-band spur contributed to `|x|` and leaked into audio
//! proportionally.
//!
//! Liquid's `ampmodem` DSB-with-carrier mode is *coherent*: it runs a
//! PLL on the residual carrier, uses that as a reference to multiply
//! the signal back down to audio, and low-pass filters. Because the
//! PLL rejects off-carrier energy, out-of-band noise doesn't fold into
//! audio, and there's no DC baseline to subtract — the audio term
//! drops out of the math directly. Tradeoff: when the PLL can't lock
//! (very weak carrier, deep selective fade), the output squeals
//! briefly until lock recovers. For normal broadcast AM that's a net
//! improvement.
//!
//! ### Rate contract
//!
//! Rate-aware: reads `ctx.input_rate("in")` at init / update_rates and
//! rebuilds the liquid modem instance if the rate changes. The PLL
//! bandwidth is sized internally by liquid relative to sample rate, so
//! no preset-level knob is needed.

use anyhow::{bail, Result};
use ferrite_liquid_dsp::{AmType, Ampmodem};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct AmDemodParams {
    /// Input IQ sample rate (Hz). Construction-time hint; init's
    /// `ctx.input_rate("in")` wins once the runtime populates it.
    pub sample_rate_hz: f32,
    /// Modulation index hint passed to liquid. Broadcast AM runs
    /// 0.5–0.9 typically; the demod is not hyper-sensitive to the
    /// exact value, but a reasonable hint helps the PLL lock.
    pub mod_index: f32,
    /// Linear gain applied after the demod. Default 1.0. Useful when
    /// a station's audio is quiet and you want to goose the output
    /// before the audio sink.
    pub audio_gain: f32,
}

impl Default for AmDemodParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000.0,
            mod_index: 0.8,
            audio_gain: 1.0,
        }
    }
}

pub struct AmDemod {
    params: AmDemodParams,
    /// Liquid `ampmodem` handle, lazily realized. `None` between
    /// construction and first init when the input rate can't be
    /// resolved from the construction-time hint.
    inner: Option<Ampmodem>,
    /// Last-applied input sample rate. Tracked so `update_rates` can
    /// skip the rebuild when the scheduler reports the same rate.
    input_rate_hz: f64,
}

impl AmDemod {
    pub fn new(params: AmDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "am_demod sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        if !(params.mod_index.is_finite() && params.mod_index > 0.0) {
            bail!("am_demod mod_index must be > 0 (got {})", params.mod_index);
        }
        if !(params.audio_gain.is_finite() && params.audio_gain > 0.0) {
            bail!(
                "am_demod audio_gain must be > 0 (got {})",
                params.audio_gain
            );
        }
        let mut s = Self {
            params,
            inner: None,
            input_rate_hz: 0.0,
        };
        s.rebuild_for_input_rate(f64::from(params.sample_rate_hz))?;
        Ok(s)
    }

    fn rebuild_for_input_rate(&mut self, input_rate_hz: f64) -> Result<()> {
        if !(input_rate_hz > 0.0) {
            bail!("am_demod: need positive input rate, got {input_rate_hz}");
        }
        // DSB + carrier-present = coherent broadcast AM recovery. The
        // PLL internally sizes its loop filter from the sample rate
        // (liquid drives it off its `ampmodem_s::fc` — scaled fraction
        // of Fs), so just rebuild the handle whenever rate shifts.
        let modem = Ampmodem::new(
            self.params.mod_index,
            AmType::Dsb,
            /*suppressed_carrier=*/ false,
        )
        .map_err(|e| anyhow::anyhow!("ampmodem create: {e}"))?;
        self.inner = Some(modem);
        self.input_rate_hz = input_rate_hz;
        Ok(())
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for AmDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "AmDemod",
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
                        default: 48_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "",
                },
                ParamSpec {
                    key: "mod_index",
                    label: "Modulation index",
                    kind: ParamKind::Range {
                        min: 0.01,
                        max: 1.5,
                        step: 0.01,
                        default: 0.8,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "",
                },
                ParamSpec {
                    key: "audio_gain",
                    label: "Post-demod gain",
                    kind: ParamKind::Range {
                        min: 0.01,
                        max: 1_000.0,
                        step: 0.01,
                        default: 1.0,
                        unit: "×",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "",
                },
            ],
            ai_notes: "",
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
        let g = self.params.audio_gain;
        for i in 0..n {
            let x = src[i];
            dst[i] = inner.demodulate(x.re, x.im) * g;
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for AmDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: AmDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(AmDemod::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{AmDemod, AmDemodParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;
    use num_complex::Complex;

    fn run(demod: &mut AmDemod, input: &[Complex<f32>]) -> Vec<f32> {
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
        assert!(AmDemod::new(AmDemodParams {
            sample_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(AmDemod::new(AmDemodParams {
            mod_index: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(AmDemod::new(AmDemodParams {
            audio_gain: 0.0,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn amplitude_modulated_carrier_recovers_tone() {
        // DSB-AM with carrier at baseband: x(t) = (1 + m·cos(2π·f_m·t))·1
        // The coherent demod locks to the residual DC carrier and
        // recovers the modulation term. Output level depends on the
        // demod's internal normalization — we check for the modulation
        // frequency's presence via RMS above the pre-settle noise
        // floor, not a precise scale factor.
        let fs = 48_000.0_f32;
        let f_m = 1_000.0_f32;
        let m = 0.5_f32;
        let n = 8192;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                let env = 1.0 + m * (TAU * f_m * t).cos();
                Complex::new(env, 0.0)
            })
            .collect();
        let mut demod = AmDemod::new(AmDemodParams {
            sample_rate_hz: fs,
            mod_index: m,
            audio_gain: 1.0,
        })
        .unwrap();
        let out = run(&mut demod, &input);

        // Skip PLL lock transient (~first half of the block).
        let tail = &out[n / 2..];
        let rms: f32 = (tail.iter().map(|y| y * y).sum::<f32>() / tail.len() as f32).sqrt();
        // Expected scale is not nailed down by spec; just confirm the
        // modulation is present (≫ noise floor) and bounded.
        assert!(
            rms > 0.05,
            "coherent demod gave no audio energy (rms={rms})"
        );
        assert!(
            rms < 10.0,
            "coherent demod output unreasonably large (rms={rms})"
        );
    }

    #[test]
    fn audio_gain_scales_output() {
        let fs = 48_000.0_f32;
        let f_m = 1_000.0_f32;
        let m = 0.3_f32;
        let n = 8192;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                let env = 1.0 + m * (TAU * f_m * t).cos();
                Complex::new(env, 0.0)
            })
            .collect();
        let mk = |g| {
            AmDemod::new(AmDemodParams {
                sample_rate_hz: fs,
                mod_index: m,
                audio_gain: g,
            })
            .unwrap()
        };
        let mut unity = mk(1.0);
        let mut loud = mk(10.0);
        let out_1 = run(&mut unity, &input);
        let out_10 = run(&mut loud, &input);
        let rms =
            |x: &[f32]| -> f32 { (x.iter().map(|y| y * y).sum::<f32>() / x.len() as f32).sqrt() };
        let r1 = rms(&out_1[n / 2..]);
        let r10 = rms(&out_10[n / 2..]);
        let ratio = r10 / r1;
        assert!(
            (ratio - 10.0).abs() < 0.01,
            "gain=10 should give 10× RMS, got ratio={ratio}"
        );
    }
}
