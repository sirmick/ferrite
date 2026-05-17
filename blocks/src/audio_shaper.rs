//! Post-demod audio conditioning — `RealF32` in, `RealF32` out, same
//! rate. Two stages, in order:
//!
//! 1. **DC blocker.** A one-pole/one-zero high-pass (`y = x − x₁ +
//!    R·y₁`) that removes the DC/near-DC term a frequency discriminator
//!    leaves when the carrier is slightly mistuned (and the residual
//!    pedestal an envelope detector leaves). Flat passband above a few
//!    Hz, exact null at DC.
//!
//! 2. **Audio low-pass.** A windowed-sinc (Hamming) FIR brick-wall.
//!    This is the stage every serious receiver has and Ferrite's mono
//!    paths lacked: WBFM mono needs ~15 kHz to kill the 19 kHz stereo
//!    pilot + 38 kHz L−R + 57 kHz RDS *before* decimation aliases them
//!    into audio; NBFM voice needs ~3.4 kHz because FM noise is
//!    triangular (rises with frequency) so an unfiltered NBFM is all
//!    hiss; AM wants ~5 kHz.
//!
//! Deliberately **not** an `FmDemod` feature: AIS / AX.25-packet /
//! POCSAG-FLEX presets consume the *raw* discriminator and would be
//! broken by band-limiting or DC removal. Keeping this a separate block
//! means it is simply absent from those flowgraphs — safe by
//! construction. De-emphasis stays in the AudioNr stage (already wired
//! for the FM audio presets).

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// DC-blocker high-pass corner (Hz). Sub-audible; just kills the
/// discriminator's tuning-offset DC term without touching programme.
const DC_BLOCK_HZ: f64 = 3.0;

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct AudioShaperParams {
    /// Input/output sample rate (Hz). Construction-time hint; init's
    /// `ctx.input_rate("in")` wins once the runtime populates it.
    pub sample_rate_hz: f32,
    /// Audio low-pass cutoff (Hz). `0` (or ≥ Nyquist) bypasses the LPF.
    /// 15 kHz WBFM mono, ~3.4 kHz NBFM voice, ~5 kHz AM.
    pub lpf_hz: f32,
    /// Enable the DC blocker. Default `true`.
    pub dc_block: bool,
}

impl Default for AudioShaperParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000.0,
            lpf_hz: 0.0,
            dc_block: true,
        }
    }
}

pub struct AudioShaper {
    params: AudioShaperParams,
    input_rate_hz: f64,

    // --- DC blocker (y = x - x1 + R*y1) ---
    dc_r: f32,
    dc_x1: f32,
    dc_y1: f32,

    // --- FIR low-pass ---
    taps: Vec<f32>,
    ring: Vec<f32>,
    pos: usize,
}

impl AudioShaper {
    pub fn new(params: AudioShaperParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "audio_shaper sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        if !params.lpf_hz.is_finite() || params.lpf_hz < 0.0 {
            bail!("audio_shaper lpf_hz must be >= 0 (got {})", params.lpf_hz);
        }
        let mut s = Self {
            params,
            input_rate_hz: 0.0,
            dc_r: 0.0,
            dc_x1: 0.0,
            dc_y1: 0.0,
            taps: Vec::new(),
            ring: Vec::new(),
            pos: 0,
        };
        s.rebuild(f64::from(params.sample_rate_hz))?;
        Ok(s)
    }

    fn rebuild(&mut self, fs: f64) -> Result<()> {
        if !(fs > 0.0) {
            bail!("audio_shaper: need positive input rate, got {fs}");
        }
        self.input_rate_hz = fs;

        // DC blocker pole from the corner frequency:
        // R = 1 - 2π·fc/fs (one-pole/one-zero high-pass approximation).
        #[allow(clippy::cast_possible_truncation)]
        {
            self.dc_r = (1.0 - core::f64::consts::TAU * DC_BLOCK_HZ / fs).clamp(0.0, 0.9999) as f32;
        }
        self.dc_x1 = 0.0;
        self.dc_y1 = 0.0;

        // Windowed-sinc LPF. Bypass when no cutoff or cutoff past
        // Nyquist (taps empty → pass-through in `process`).
        let cutoff = f64::from(self.params.lpf_hz);
        if cutoff <= 0.0 || cutoff >= fs / 2.0 {
            self.taps.clear();
            self.ring.clear();
            self.pos = 0;
            return Ok(());
        }
        // Hamming transition ≈ 3.3·fs/N. Aim the transition at 30 % of
        // the cutoff (e.g. 15 k → ~4.5 k, so the 19 k pilot lands deep
        // in the stopband). Clamp tap count to keep the hot path sane.
        let trans = (cutoff * 0.30).max(1.0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let mut n = (3.3 * fs / trans).ceil() as usize;
        n = n.clamp(31, 401);
        if n.is_multiple_of(2) {
            n += 1;
        }
        let fc_norm = cutoff / fs; // cycles/sample, 0..0.5
        let m = (n - 1) as f64 / 2.0;
        let mut taps: Vec<f32> = Vec::with_capacity(n);
        let mut sum = 0.0_f64;
        for i in 0..n {
            let k = i as f64 - m;
            // Ideal LPF impulse response (sinc), Hamming-windowed.
            let sinc = if k.abs() < 1e-9 {
                2.0 * fc_norm
            } else {
                (core::f64::consts::TAU * fc_norm * k).sin() / (core::f64::consts::PI * k)
            };
            let w = 0.54 - 0.46 * (core::f64::consts::TAU * i as f64 / (n - 1) as f64).cos();
            let t = sinc * w;
            sum += t;
            #[allow(clippy::cast_possible_truncation)]
            taps.push(t as f32);
        }
        // Normalise to unity DC gain.
        #[allow(clippy::cast_possible_truncation)]
        for t in &mut taps {
            *t = (f64::from(*t) / sum) as f32;
        }
        self.taps = taps;
        self.ring = vec![0.0; n];
        self.pos = 0;
        Ok(())
    }

    #[inline]
    fn dc(&mut self, x: f32) -> f32 {
        if !self.params.dc_block {
            return x;
        }
        let y = x - self.dc_x1 + self.dc_r * self.dc_y1;
        self.dc_x1 = x;
        self.dc_y1 = y;
        y
    }

    #[inline]
    fn lpf(&mut self, x: f32) -> f32 {
        if self.taps.is_empty() {
            return x;
        }
        let n = self.taps.len();
        self.ring[self.pos] = x;
        let mut acc = 0.0_f32;
        // taps[0] applies to the oldest sample → walk the ring forward
        // from the slot just written (which is the newest).
        let mut idx = self.pos;
        for k in (0..n).rev() {
            acc += self.taps[k] * self.ring[idx];
            idx = if idx == 0 { n - 1 } else { idx - 1 };
        }
        self.pos = (self.pos + 1) % n;
        acc
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for AudioShaper {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "AudioShaper",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::RealF32,
            }],
            params: &[
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Sample rate",
                    kind: ParamKind::Range {
                        min: 1_000.0,
                        max: 10_000_000.0,
                        step: 1.0,
                        default: 48_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Audio rate the shaper runs at — the demod/discriminator output rate, before any decimation.",
                },
                ParamSpec {
                    key: "lpf_hz",
                    label: "Audio LPF cutoff",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 100_000.0,
                        step: 100.0,
                        default: 0.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Brick-wall audio low-pass. 15000 WBFM mono (kills 19 kHz pilot + 38 kHz L−R + 57 kHz RDS pre-decimation), ~3400 NBFM voice (FM noise is triangular — huge SNR win), ~5000 AM. 0 = bypass.",
                },
                ParamSpec {
                    key: "dc_block",
                    label: "DC blocker",
                    kind: ParamKind::Toggle { default: true },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Remove the DC term a discriminator leaves when the carrier is slightly mistuned (audible as rumble / a level bias into de-emphasis). Leave on for voice/broadcast.",
                },
            ],
            ai_notes: "Post-demod audio conditioner: DC-block + brick-wall audio LPF. Wire between an FM/AM demod and the resampler on AUDIO presets only — never on AIS/packet/pager paths, which need the raw discriminator. De-emphasis is handled separately in the AudioNr stage.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.rebuild(rate)?;
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.rebuild(rate)?;
            }
        }
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
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
            let x = self.dc(src[i]);
            dst[i] = self.lpf(x);
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for AudioShaper {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: AudioShaperParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(AudioShaper::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{AudioShaper, AudioShaperParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;

    fn run(s: &mut AudioShaper, input: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0_f32; input.len()];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(input),
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
        let w = s.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], input.len());
        assert_eq!(w.produced[0], input.len());
        out
    }

    fn tone(freq: f32, fs: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (TAU * freq * i as f32 / fs).sin()).collect()
    }

    fn rms(xs: &[f32]) -> f32 {
        (xs.iter().map(|x| x * x).sum::<f32>() / xs.len() as f32).sqrt()
    }

    #[test]
    fn rejects_bad_params() {
        assert!(AudioShaper::new(AudioShaperParams {
            sample_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(AudioShaper::new(AudioShaperParams {
            lpf_hz: -1.0,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn passband_tone_survives_stopband_tone_killed() {
        // WBFM-mono shape: 240 kHz, 15 kHz LPF. 1 kHz passes, the
        // 19 kHz pilot is crushed.
        let fs = 240_000.0_f32;
        let n = 48_000;
        let mut s = AudioShaper::new(AudioShaperParams {
            sample_rate_hz: fs,
            lpf_hz: 15_000.0,
            dc_block: true,
        })
        .unwrap();
        let pass = run(&mut s, &tone(1_000.0, fs, n));
        let mut s2 = AudioShaper::new(AudioShaperParams {
            sample_rate_hz: fs,
            lpf_hz: 15_000.0,
            dc_block: true,
        })
        .unwrap();
        let stop = run(&mut s2, &tone(19_000.0, fs, n));
        let tail_p = rms(&pass[2_048..]);
        let tail_s = rms(&stop[2_048..]);
        assert!(tail_p > 0.6, "1 kHz should pass ~unity, got {tail_p}");
        let atten_db = 20.0 * (tail_p / tail_s.max(1e-9)).log10();
        assert!(
            atten_db > 40.0,
            "19 kHz pilot only {atten_db:.1} dB down (need >40)",
        );
    }

    #[test]
    fn nbfm_voice_lpf_kills_hf_hiss_band() {
        // 25 kHz, 3.4 kHz LPF: 800 Hz passes, 8 kHz crushed.
        let fs = 25_000.0_f32;
        let n = 25_000;
        let mut a = AudioShaper::new(AudioShaperParams {
            sample_rate_hz: fs,
            lpf_hz: 3_400.0,
            dc_block: true,
        })
        .unwrap();
        let p = run(&mut a, &tone(800.0, fs, n));
        let mut b = AudioShaper::new(AudioShaperParams {
            sample_rate_hz: fs,
            lpf_hz: 3_400.0,
            dc_block: true,
        })
        .unwrap();
        let q = run(&mut b, &tone(8_000.0, fs, n));
        let atten_db = 20.0 * (rms(&p[1_024..]) / rms(&q[1_024..]).max(1e-9)).log10();
        assert!(atten_db > 40.0, "8 kHz hiss only {atten_db:.1} dB down");
    }

    #[test]
    fn dc_blocker_removes_offset_keeps_tone() {
        let fs = 48_000.0_f32;
        let n = 24_000;
        let sig: Vec<f32> = tone(1_000.0, fs, n).iter().map(|x| x + 0.5).collect();
        let mut s = AudioShaper::new(AudioShaperParams {
            sample_rate_hz: fs,
            lpf_hz: 0.0, // bypass LPF, isolate the DC blocker
            dc_block: true,
        })
        .unwrap();
        let out = run(&mut s, &sig);
        let tail = &out[4_096..];
        let mean = tail.iter().sum::<f32>() / tail.len() as f32;
        assert!(mean.abs() < 0.02, "DC not removed: residual mean {mean}");
        // 1 kHz tone preserved (~0.707 RMS).
        assert!(
            (rms(tail) - 0.707).abs() < 0.05,
            "tone altered by DC blocker: rms {}",
            rms(tail)
        );
    }

    #[test]
    fn lpf_bypass_is_passthrough() {
        let fs = 48_000.0_f32;
        let sig = tone(7_000.0, fs, 8_192);
        let mut s = AudioShaper::new(AudioShaperParams {
            sample_rate_hz: fs,
            lpf_hz: 0.0,
            dc_block: false,
        })
        .unwrap();
        let out = run(&mut s, &sig);
        for (i, (a, b)) in sig.iter().zip(out.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "sample {i}: {a} != {b}");
        }
    }
}
