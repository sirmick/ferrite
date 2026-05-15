//! AM demodulator — complex IQ → real audio via envelope detection
//! with a DC blocker.
//!
//! One IQ-in, one Real-out, same sample rate. Input IQ baseband roughly
//! centred on the AM carrier; output a real audio signal.
//!
//! ### Why envelope detection
//!
//! Broadcast AM transmits a large unmodulated carrier *specifically* so
//! that a non-coherent envelope detector — `|x|` minus the carrier DC —
//! recovers the audio regardless of carrier frequency offset or phase.
//! That offset-immunity is the whole point of the modulation scheme.
//!
//! A previous rewrite swapped this for liquid-dsp's coherent `ampmodem`
//! PLL. Measured against the shipped reference recording it regressed
//! badly: the PLL pulls in only ~±500 Hz (at 12 kHz Fs), so any
//! residual channelizer mistuning made it demodulate the carrier *beat
//! note* instead of the audio; and even perfectly tuned it ran ~11 dB
//! quieter, forcing the downstream AGC to max gain (noise/pumping).
//! Envelope detection holds full SINAD at *every* offset — see
//! `blocks/tests/am_quality_probe.rs`. Coherent detection's only edge
//! (off-carrier noise rejection) isn't worth the real-world fragility
//! for large-carrier broadcast AM.
//!
//! ### DC blocker
//!
//! `|x|` carries the audio riding on a large DC pedestal (the carrier
//! amplitude). A one-pole DC tracker (`tau ≈ 100 ms`) estimates that
//! pedestal and subtracts it, leaving the modulation. The tracker is
//! slow enough that programme material below a few Hz isn't touched but
//! fast enough that gain/fade drift doesn't leak a thump.
//!
//! ### Rate contract
//!
//! Rate-aware: reads `ctx.input_rate("in")` at init / update_rates and
//! re-derives the DC-tracker coefficient if the rate changes.

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// DC-tracker time constant. Slow enough not to nibble programme
/// material below a few Hz, fast enough that fade/AGC drift doesn't
/// leak a thump. Matches the shipped reference recording's "100 ms DC
/// blocker". Internal, not user-facing — keeps the param surface small.
const DC_TRACK_TAU_MS: f64 = 100.0;

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct AmDemodParams {
    /// Input IQ sample rate (Hz). Construction-time hint; init's
    /// `ctx.input_rate("in")` wins once the runtime populates it.
    pub sample_rate_hz: f32,
    /// Linear gain applied after the demod. Default 1.0. Useful when
    /// a station's audio is quiet and you want to goose the output
    /// before the audio sink.
    pub audio_gain: f32,
}

impl Default for AmDemodParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000.0,
            audio_gain: 1.0,
        }
    }
}

pub struct AmDemod {
    params: AmDemodParams,
    /// One-pole DC-tracker state — the running estimate of the carrier
    /// pedestal that the envelope rides on.
    dc: f32,
    /// DC-tracker EMA coefficient, derived from the input rate and
    /// [`DC_TRACK_TAU_MS`].
    dc_alpha: f32,
    /// Seeded-DC flag. Before the first sample `dc` is zero and the
    /// first output would be a full-amplitude pedestal thump; seed
    /// from the first envelope so the tracker starts converged.
    primed: bool,
    /// Last-applied input sample rate. Tracked so `update_rates` can
    /// skip recomputation when the scheduler reports the same rate.
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
        if !(params.audio_gain.is_finite() && params.audio_gain > 0.0) {
            bail!(
                "am_demod audio_gain must be > 0 (got {})",
                params.audio_gain
            );
        }
        let mut s = Self {
            params,
            dc: 0.0,
            dc_alpha: 1.0,
            primed: false,
            input_rate_hz: 0.0,
        };
        s.set_input_rate(f64::from(params.sample_rate_hz))?;
        Ok(s)
    }

    fn set_input_rate(&mut self, input_rate_hz: f64) -> Result<()> {
        if !(input_rate_hz > 0.0) {
            bail!("am_demod: need positive input rate, got {input_rate_hz}");
        }
        // One-pole EMA coefficient for a `tau`-millisecond tracker:
        // alpha = dt / (tau + dt).
        let dt = 1.0 / input_rate_hz;
        let tau_s = DC_TRACK_TAU_MS * 1e-3;
        #[allow(clippy::cast_possible_truncation)]
        {
            self.dc_alpha = (dt / (tau_s + dt)) as f32;
        }
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
                    ai_notes: "Demod input rate (typically the channelizer's output).",
                },
                ParamSpec {
                    key: "audio_gain",
                    label: "Post-demod gain",
                    kind: ParamKind::Range {
                        min: 0.01,
                        max: 1_000.0,
                        step: 0.01,
                        default: 1.0,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "Linear post-demod gain. Bump if the audio sounds quiet after NR; drop if it clips.",
                },
            ],
            ai_notes: "AM envelope demodulator (offset-immune — large-carrier broadcast AM is designed for it). Use for AM broadcast (540 kHz – 1.7 MHz), shortwave broadcast, aviation AM. NB: AM carries info in amplitude — pair with the `wbam` preset's `agc=false` force_param so the *hardware* AGC doesn't fight the envelope.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.set_input_rate(rate)?;
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.set_input_rate(rate)?;
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
            .and_then(OutputPort::as_real_f32_mut)
        else {
            return Ok(Work::new());
        };

        let n = src.len().min(dst.len());
        let g = self.params.audio_gain;
        for i in 0..n {
            let x = src[i];
            // Envelope = carrier-DC pedestal + audio. Offset/phase
            // independent: |x| ignores where the carrier sits.
            let env = x.re.hypot(x.im);
            if !self.primed {
                self.dc = env;
                self.primed = true;
            }
            // Slow one-pole DC tracker; subtract to leave the audio.
            self.dc += self.dc_alpha * (env - self.dc);
            dst[i] = (env - self.dc) * g;
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
            audio_gain: 0.0,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn amplitude_modulated_carrier_recovers_tone() {
        // DSB-AM with carrier OFFSET from baseband: x(t) =
        // (1 + m·cos(2π·f_m·t))·e^{j2π·f_c·t}. Envelope detection must
        // recover the f_m tone regardless of the f_c carrier offset —
        // that offset-immunity is the whole reason to prefer it for
        // large-carrier broadcast AM.
        let fs = 48_000.0_f32;
        let f_m = 1_000.0_f32;
        let f_c = 7_000.0_f32; // deliberately way off centre
        let m = 0.5_f32;
        let n = 8192;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                let env = 1.0 + m * (TAU * f_m * t).cos();
                let ph = TAU * f_c * t;
                Complex::new(env * ph.cos(), env * ph.sin())
            })
            .collect();
        let mut demod = AmDemod::new(AmDemodParams {
            sample_rate_hz: fs,
            audio_gain: 1.0,
        })
        .unwrap();
        let out = run(&mut demod, &input);

        // Skip the DC-tracker settle (~first quarter).
        let tail = &out[n / 4..];
        // Recovered audio is m·cos(2π·f_m·t); RMS ≈ m/√2 ≈ 0.354.
        let rms: f32 = (tail.iter().map(|y| y * y).sum::<f32>() / tail.len() as f32).sqrt();
        assert!(rms > 0.2, "envelope demod gave no audio energy (rms={rms})");
        assert!(
            rms < 1.0,
            "envelope demod output unreasonably large (rms={rms})"
        );
        // Tone must land at f_m, not at the carrier offset f_c.
        let mag_at = |f: f32| -> f32 {
            let w = TAU * f / fs;
            let (mut re, mut im) = (0.0f32, 0.0f32);
            for (k, &y) in tail.iter().enumerate() {
                let p = w * k as f32;
                re += y * p.cos();
                im -= y * p.sin();
            }
            (re * re + im * im).sqrt() / tail.len() as f32
        };
        let tone = mag_at(f_m);
        let leak = mag_at(f_c).max(mag_at(2.0 * f_m));
        assert!(
            tone > 10.0 * leak,
            "f_m tone ({tone}) should dominate carrier-offset/harmonic leak ({leak})"
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
