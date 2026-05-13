//! Power-threshold squelch — mutes the IQ stream when the signal level
//! drops below a threshold, opens again when it rises.
//!
//! One IQ-in, one IQ-out, same sample rate. Sits between the channelizer
//! and any demod block — keeps the demod agnostic to squelching, and
//! the same `Squelch` instance works for FM, AM, and SSB presets.
//!
//! ### Algorithm
//!
//! ```text
//! power[n] = (1 − α_p)·power[n−1] + α_p·|iq[n]|²
//! target   = 1.0  if  gated && power > T_open
//!          = 0.0  if !gated && power < T_close
//!          = previous target otherwise   (hysteresis)
//! gain[n]  = (1 − α_g)·gain[n−1] + α_g·target
//! out[n]   = iq[n] · gain[n]
//! ```
//!
//! `α_p = 1 − exp(−dt / τ_power)` smooths the instantaneous power so a
//! single loud sample doesn't trip the gate; `α_g = 1 − exp(−dt / τ_gain)`
//! applies a soft fade on the mute path so FM demod doesn't pop when
//! the gain snaps to zero. Hysteresis (`T_close < T_open`) avoids
//! chattering on a signal that bounces around the threshold.
//!
//! ### Units
//!
//! Thresholds are in dBFS assuming full-scale IQ magnitude ≈ 1.0 —
//! matches the rest of the pipeline which works in `f32` normalized
//! units. `−60 dBFS` is a reasonable default for a quiet RF input;
//! raise it for noisier environments.

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct SquelchParams {
    /// Input IQ sample rate (Hz).
    pub sample_rate_hz: f32,
    /// Open threshold in dBFS — gate opens when the smoothed IQ power
    /// rises above this level. `0 dB = full-scale IQ magnitude = 1.0`.
    pub threshold_db: f32,
    /// Hysteresis in dB — close threshold sits `hysteresis_db` below
    /// `threshold_db`. Keeps the gate from chattering on marginal
    /// signals.
    pub hysteresis_db: f32,
    /// Power-smoother time constant (ms). Short → snappy response,
    /// picks up brief signal bursts; long → ignores momentary peaks
    /// from noise spikes.
    pub power_tau_ms: f32,
    /// Gain-fader time constant (ms). Short → audible click at mute
    /// transitions; long → audible tail after a transmission ends.
    /// 10–30 ms is a reasonable tradeoff.
    pub fade_tau_ms: f32,
}

impl Default for SquelchParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000.0,
            threshold_db: -60.0,
            hysteresis_db: 3.0,
            power_tau_ms: 20.0,
            fade_tau_ms: 20.0,
        }
    }
}

pub struct Squelch {
    open_threshold: f32,
    close_threshold: f32,
    alpha_power: f32,
    alpha_fade: f32,
    power: f32,
    gain: f32,
    open: bool,
}

impl Squelch {
    /// Build a squelch with the supplied params. Fails on non-positive
    /// rates or time constants, or NaN thresholds.
    pub fn new(params: SquelchParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "squelch sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        if !params.threshold_db.is_finite() {
            bail!("squelch threshold_db must be finite");
        }
        if !(params.hysteresis_db.is_finite() && params.hysteresis_db >= 0.0) {
            bail!(
                "squelch hysteresis_db must be ≥ 0 (got {})",
                params.hysteresis_db
            );
        }
        if !(params.power_tau_ms.is_finite() && params.power_tau_ms > 0.0) {
            bail!(
                "squelch power_tau_ms must be > 0 (got {})",
                params.power_tau_ms
            );
        }
        if !(params.fade_tau_ms.is_finite() && params.fade_tau_ms > 0.0) {
            bail!(
                "squelch fade_tau_ms must be > 0 (got {})",
                params.fade_tau_ms
            );
        }

        // Convert dBFS → linear power. Power is |z|², so 0 dB = 1.0.
        let open_threshold = 10.0_f32.powf(params.threshold_db / 10.0);
        let close_threshold = 10.0_f32.powf((params.threshold_db - params.hysteresis_db) / 10.0);
        let dt = 1.0 / params.sample_rate_hz;
        let alpha_power = 1.0 - (-dt / (params.power_tau_ms * 1e-3)).exp();
        let alpha_fade = 1.0 - (-dt / (params.fade_tau_ms * 1e-3)).exp();

        Ok(Self {
            open_threshold,
            close_threshold,
            alpha_power,
            alpha_fade,
            power: 0.0,
            gain: 0.0,
            open: false,
        })
    }

    /// Current gate state. Exposed for tests and future UI probes.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.open
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for Squelch {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "Squelch",
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
                    ai_notes: "Input rate.",
                },
                ParamSpec {
                    key: "threshold_db",
                    label: "Open threshold",
                    kind: ParamKind::Range {
                        min: -120.0,
                        max: 0.0,
                        step: 0.5,
                        default: -60.0,
                        unit: "dBFS",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "Power threshold to open the gate. Set just above the noise floor measured on a quiet channel.",
                },
                ParamSpec {
                    key: "hysteresis_db",
                    label: "Hysteresis",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 30.0,
                        step: 0.5,
                        default: 3.0,
                        unit: "dB",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "Gap between open and close thresholds (close = threshold - hysteresis). Prevents chatter on borderline signals.",
                },
                ParamSpec {
                    key: "power_tau_ms",
                    label: "Power smoother τ",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 1_000.0,
                        step: 1.0,
                        default: 20.0,
                        unit: "ms",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "Envelope smoother time constant. Shorter = faster open/close response; longer = stabler decisions on weak/fluttery signals.",
                },
                ParamSpec {
                    key: "fade_tau_ms",
                    label: "Fade τ",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 500.0,
                        step: 1.0,
                        default: 20.0,
                        unit: "ms",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "Cross-fade time when opening/closing the gate. Avoids audible clicks.",
                },
            ],
            ai_notes: "Power-threshold squelch on an IQ stream. Mutes the output below `threshold_db` (closed) and passes through above + hysteresis (open). Sits before the demod chain to skip processing quiet channels.",
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

        let n = src.len().min(dst.len());
        for i in 0..n {
            let z = src[i];
            let inst_power = z.re * z.re + z.im * z.im;
            self.power += self.alpha_power * (inst_power - self.power);

            if self.open {
                if self.power < self.close_threshold {
                    self.open = false;
                }
            } else if self.power > self.open_threshold {
                self.open = true;
            }

            let target = if self.open { 1.0 } else { 0.0 };
            self.gain += self.alpha_fade * (target - self.gain);
            dst[i].re = z.re * self.gain;
            dst[i].im = z.im * self.gain;
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for Squelch {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: SquelchParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(Squelch::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{Squelch, SquelchParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use num_complex::Complex;

    fn run(sq: &mut Squelch, input: &[Complex<f32>]) -> Vec<Complex<f32>> {
        let mut out = vec![Complex::new(0.0, 0.0); input.len()];
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
        let w = sq.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], input.len());
        assert_eq!(w.produced[0], input.len());
        out
    }

    fn peak(x: &[Complex<f32>]) -> f32 {
        x.iter()
            .map(|z| (z.re * z.re + z.im * z.im).sqrt())
            .fold(0.0_f32, f32::max)
    }

    #[test]
    fn constructor_rejects_bad_params() {
        assert!(Squelch::new(SquelchParams {
            sample_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(Squelch::new(SquelchParams {
            threshold_db: f32::NAN,
            ..Default::default()
        })
        .is_err());
        assert!(Squelch::new(SquelchParams {
            hysteresis_db: -1.0,
            ..Default::default()
        })
        .is_err());
        assert!(Squelch::new(SquelchParams {
            power_tau_ms: 0.0,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn gate_stays_closed_below_threshold() {
        // Very quiet input (−80 dBFS) with a −60 dBFS open threshold
        // should keep the gate closed and the output near zero.
        let params = SquelchParams {
            threshold_db: -60.0,
            ..Default::default()
        };
        let mut sq = Squelch::new(params).unwrap();
        // Amplitude ≈ 10^(−80/20) = 1e−4.
        let input = vec![Complex::new(1e-4_f32, 0.0); 8192];
        let out = run(&mut sq, &input);
        assert!(!sq.is_open(), "gate opened on sub-threshold input");
        let p = peak(&out);
        assert!(p < 1e-5, "muted peak should be tiny, got {p}");
    }

    #[test]
    fn gate_opens_on_loud_input() {
        // Amplitude ≈ 0.1 → power ≈ −20 dBFS, far above the default
        // −60 dBFS threshold. After a few smoothing τ the gate is open
        // and peak is ≈ 0.1.
        let params = SquelchParams {
            threshold_db: -60.0,
            power_tau_ms: 5.0,
            fade_tau_ms: 5.0,
            ..Default::default()
        };
        let mut sq = Squelch::new(params).unwrap();
        // ~21 ms of audio — ≈ 4τ settling for both smoothers.
        let input = vec![Complex::new(0.1_f32, 0.0); 1024];
        let out = run(&mut sq, &input);
        assert!(sq.is_open(), "gate failed to open on loud input");
        let tail_peak = peak(&out[512..]);
        assert!(
            (tail_peak - 0.1).abs() < 0.005,
            "tail peak should ≈ input amplitude, got {tail_peak}"
        );
    }

    #[test]
    fn hysteresis_prevents_chatter() {
        // Open on loud burst, then drop to a level that's above the
        // close threshold but below the open threshold — must stay open.
        let params = SquelchParams {
            threshold_db: -40.0,
            hysteresis_db: 10.0, // close at −50 dBFS
            power_tau_ms: 2.0,
            fade_tau_ms: 2.0,
            ..Default::default()
        };
        let mut sq = Squelch::new(params).unwrap();

        // Loud burst at −20 dBFS: amplitude = 10^(-20/20) = 0.1.
        let loud = vec![Complex::new(0.1_f32, 0.0); 1024];
        run(&mut sq, &loud);
        assert!(sq.is_open());

        // Mid burst at −45 dBFS: amplitude ≈ 10^(-45/20) ≈ 0.00562.
        // Above close (−50) and below open (−40) — gate must stay open.
        let mid = vec![Complex::new(0.00562_f32, 0.0); 2048];
        run(&mut sq, &mid);
        assert!(sq.is_open(), "hysteresis failed — gate closed in dead band");
    }

    #[test]
    fn state_persists_across_process_calls() {
        // One long call and two half calls must give identical output
        // — the smoothers and gate bit must carry across.
        let params = SquelchParams {
            threshold_db: -30.0,
            power_tau_ms: 5.0,
            fade_tau_ms: 5.0,
            ..Default::default()
        };
        // Ramp that crosses the threshold partway through.
        let n = 1024;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let a = i as f32 / n as f32; // 0.0 → 1.0
                Complex::new(a, 0.0)
            })
            .collect();

        let mut whole = Squelch::new(params).unwrap();
        let out_whole = run(&mut whole, &input);

        let mut split = Squelch::new(params).unwrap();
        let first = run(&mut split, &input[..n / 2]);
        let second = run(&mut split, &input[n / 2..]);
        let mut out_split = first;
        out_split.extend_from_slice(&second);

        for (i, (a, b)) in out_whole.iter().zip(out_split.iter()).enumerate() {
            assert!(
                (a.re - b.re).abs() < 1e-5 && (a.im - b.im).abs() < 1e-5,
                "mismatch at {i}"
            );
        }
    }
}
