//! AM demodulator — complex IQ → real audio, envelope-detector style.
//!
//! One IQ-in, one Real-out, same sample rate. Input iq baseband at rate
//! `fs` centred on the AM carrier; output a real signal tracking the
//! instantaneous envelope with the carrier bias removed.
//!
//! ### Algorithm
//!
//! For each sample `x[n]`, take `env = |x[n]|` (the instantaneous
//! amplitude). The carrier component sits as a positive DC bias on `env`
//! because an unmodulated AM broadcast is `A·cos(ω_c·t)` whose envelope
//! is a constant `A`. A 1-pole low-pass tracks that bias and subtracts
//! it, yielding the audio message.
//!
//! ```text
//! env[n]     = hypot(re, im)
//! bias[n]    = (1 - α)·bias[n-1] + α·env[n]
//! audio[n]   = env[n] - bias[n]
//! ```
//!
//! where `α = 1 − exp(-dt / τ)` with `dt = 1/fs`. A short τ tracks the
//! bias aggressively (good for fast AGC, but eats low-frequency audio);
//! a long τ passes more of the audio but responds slowly to fades.
//!
//! ### Rationale
//!
//! Envelope detection is the textbook "diode-style" AM demod and is
//! correct for double-sideband AM with a carrier (the broadcast variant).
//! SSB, DSB-SC, and synchronous AM need a different front end. Like
//! [`crate::fm_demod::FmDemod`] this block is intentionally stateless
//! beyond the one IIR state — downstream blocks handle rate conversion
//! and spectral shaping.

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct AmDemodParams {
    /// Input IQ sample rate (Hz). Wired in by the scheduler from the input
    /// port's metadata; set explicitly for standalone tests.
    pub sample_rate_hz: f32,
    /// DC-blocker time constant (ms). Typical AM broadcast: 50–200 ms —
    /// long enough to keep audio content under a few Hz, short enough to
    /// recover from carrier fades.
    pub bias_tau_ms: f32,
    /// Linear gain applied after the DC blocker. Envelope detection of a
    /// typical AGC'd broadcast leaves audio well below full-scale because
    /// the DC removed (the carrier) dwarfs the AC residual; a post-demod
    /// gain of ≈20 (26 dB) brings a strong station near −6 dBFS. Default
    /// 1.0 — unchanged behaviour unless the preset overrides it.
    pub audio_gain: f32,
}

impl Default for AmDemodParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000.0,
            bias_tau_ms: 100.0,
            audio_gain: 1.0,
        }
    }
}

pub struct AmDemod {
    alpha: f32,
    gain: f32,
    bias: f32,
}

impl AmDemod {
    /// Builds a demod with the supplied params. Fails on non-positive
    /// rates or time constants.
    pub fn new(params: AmDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "am_demod sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        if !(params.bias_tau_ms.is_finite() && params.bias_tau_ms > 0.0) {
            bail!(
                "am_demod bias_tau_ms must be > 0 (got {})",
                params.bias_tau_ms
            );
        }
        if !(params.audio_gain.is_finite() && params.audio_gain > 0.0) {
            bail!(
                "am_demod audio_gain must be > 0 (got {})",
                params.audio_gain
            );
        }
        let tau_s = params.bias_tau_ms * 1e-3;
        let dt = 1.0 / params.sample_rate_hz;
        let alpha = 1.0 - (-dt / tau_s).exp();
        Ok(Self {
            alpha,
            gain: params.audio_gain,
            bias: 0.0,
        })
    }

    /// DC-blocker coefficient used by `process`.
    #[must_use]
    pub const fn alpha(&self) -> f32 {
        self.alpha
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
                    // Matches the upstream producer's rate — changing it
                    // here implies the whole chain is being retuned.
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
                ParamSpec {
                    key: "bias_tau_ms",
                    label: "DC-blocker time constant",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 1_000.0,
                        step: 1.0,
                        default: 100.0,
                        unit: "ms",
                    },
                    // `alpha` is baked at `new()` — re-init the block.
                    // Downstream stays running at the same rate.
                    reconfig_scope: ReconfigureScope::Downstream,
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
            let env = x.re.hypot(x.im);
            self.bias += self.alpha * (env - self.bias);
            dst[i] = (env - self.bias) * self.gain;
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
            bias_tau_ms: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(AmDemod::new(AmDemodParams {
            sample_rate_hz: f32::NAN,
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
    fn unmodulated_carrier_settles_to_zero_output() {
        // A constant-amplitude IQ tone is an unmodulated carrier — the
        // bias tracker should absorb the full envelope and leave 0.
        // τ=5ms → 3τ settling ≈ 720 samples at 48 kHz, so the tail at
        // 2048 samples onwards is well past it.
        let params = AmDemodParams {
            sample_rate_hz: 48_000.0,
            bias_tau_ms: 5.0,
            audio_gain: 1.0,
        };
        let mut demod = AmDemod::new(params).unwrap();
        let input = vec![Complex::new(0.6_f32, -0.8); 4096];
        let out = run(&mut demod, &input);
        let tail = &out[2048..];
        let peak = tail.iter().map(|y| y.abs()).fold(0.0_f32, f32::max);
        assert!(peak < 1e-3, "unmodulated carrier leaks: peak={peak}");
    }

    #[test]
    fn amplitude_modulated_carrier_recovers_tone() {
        // Build a textbook DSB-AM signal at baseband:
        //   x(t) = (1 + m·cos(2π·f_m·t)) · exp(j·0)
        // The envelope |x| equals the real modulator shape, and after
        // DC removal the output is m·cos(2π·f_m·t).
        //
        // τ=5ms gives a HPF cutoff of ~32 Hz, so a 1 kHz tone passes
        // with negligible attenuation and the bias fully tracks the
        // carrier DC within ~720 samples.
        let fs = 48_000.0_f32;
        let f_m = 1_000.0_f32;
        let m = 0.5_f32;
        let n = 4096;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                let env = 1.0 + m * (TAU * f_m * t).cos();
                Complex::new(env, 0.0)
            })
            .collect();
        let params = AmDemodParams {
            sample_rate_hz: fs,
            bias_tau_ms: 5.0,
            audio_gain: 1.0,
        };
        let mut demod = AmDemod::new(params).unwrap();
        let out = run(&mut demod, &input);

        // Skip the DC-tracker settling region.
        let tail = &out[2048..];
        let peak = tail.iter().map(|y| y.abs()).fold(0.0_f32, f32::max);
        assert!(
            (peak - m).abs() < 0.02,
            "AM tone recovery peak={peak}, expected ≈ {m}"
        );
    }

    #[test]
    fn envelope_is_independent_of_iq_phase() {
        // Same envelope, different rotation — output must match bit-for-bit
        // since the detector is |x|.
        let params = AmDemodParams {
            sample_rate_hz: 48_000.0,
            bias_tau_ms: 100.0,
            audio_gain: 1.0,
        };
        let n = 2048;
        let mut real = AmDemod::new(params).unwrap();
        let mut rot = AmDemod::new(params).unwrap();
        let env: Vec<f32> = (0..n)
            .map(|i| 0.8 + 0.2 * (TAU * 500.0 * i as f32 / params.sample_rate_hz).cos())
            .collect();
        let input_real: Vec<Complex<f32>> = env.iter().map(|&a| Complex::new(a, 0.0)).collect();
        let input_rot: Vec<Complex<f32>> = env
            .iter()
            .enumerate()
            .map(|(i, &a)| {
                let theta = 0.1 * i as f32;
                Complex::from_polar(a, theta)
            })
            .collect();
        let out_real = run(&mut real, &input_real);
        let out_rot = run(&mut rot, &input_rot);
        for (i, (&a, &b)) in out_real.iter().zip(out_rot.iter()).enumerate() {
            assert!((a - b).abs() < 1e-5, "phase leaked at {i}: {a} vs {b}");
        }
    }

    #[test]
    fn audio_gain_scales_output() {
        // Same modulator, two gains — output RMS should differ by the gain
        // ratio. The envelope detector and DC blocker are identical; only
        // the post-demod multiplier changes.
        let fs = 48_000.0_f32;
        let f_m = 1_000.0_f32;
        let m = 0.3_f32;
        let n = 4096;
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
                bias_tau_ms: 5.0,
                audio_gain: g,
            })
            .unwrap()
        };
        let mut unity = mk(1.0);
        let mut loud = mk(10.0);
        let out_1 = run(&mut unity, &input);
        let out_10 = run(&mut loud, &input);
        // Post-settling RMS ratio must be ~10×.
        let rms =
            |x: &[f32]| -> f32 { (x.iter().map(|y| y * y).sum::<f32>() / x.len() as f32).sqrt() };
        let r1 = rms(&out_1[2048..]);
        let r10 = rms(&out_10[2048..]);
        let ratio = r10 / r1;
        assert!(
            (ratio - 10.0).abs() < 0.01,
            "gain=10 should give 10× RMS, got ratio={ratio}"
        );
    }

    #[test]
    fn bias_state_persists_across_process_calls() {
        // A tone split across two calls must match one long call — the
        // join-point would glitch if `bias` were not carried over.
        let params = AmDemodParams {
            sample_rate_hz: 48_000.0,
            bias_tau_ms: 100.0,
            audio_gain: 1.0,
        };
        let n = 512;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f32 / params.sample_rate_hz;
                let env = 1.0 + 0.3 * (TAU * 2_000.0 * t).cos();
                Complex::new(env, 0.0)
            })
            .collect();

        let mut whole = AmDemod::new(params).unwrap();
        let out_whole = run(&mut whole, &input);

        let mut split = AmDemod::new(params).unwrap();
        let first = run(&mut split, &input[..256]);
        let second = run(&mut split, &input[256..]);
        let mut out_split = first;
        out_split.extend_from_slice(&second);

        for (i, (a, b)) in out_whole.iter().zip(out_split.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "mismatch at {i}: whole={a} split={b}");
        }
    }
}
