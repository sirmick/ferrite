//! SSB modulator — real audio → complex IQ at a higher sample rate.
//!
//! The exact inverse of [`SsbDemod`](crate::ssb_demod): both wrap
//! liquid's `ampmodem`, so the same Remez-designed `firhilbf` that
//! demod uses to split sidebands is the one that *builds* the
//! single-sideband analytic signal here. For each output sample:
//!
//! ```text
//! audio[k] = linear_interp(input, k · input_rate / output_rate)
//! ssb[k]   = ampmodem_modulate(audio[k])        (complex, at DC)
//! iq[k]    = ssb[k] · exp(j · 2π · offset_hz · k / output_rate)
//! ```
//!
//! Why it exists: the sigidwiki fldigi-mode fixtures are *audio*
//! recordings (what an SSB receiver hears). Feeding them straight
//! into a decoder skips the real RX chain. SSB-modulating them onto
//! IQ at a carrier offset lets the e2e run the production flowgraph —
//! `FileAudioSource → SsbModulator → Channelizer → SsbDemod →
//! RealF32Resamp → <decoder>` — so the tuner/AutoTune, resampler and
//! demod are all exercised, not bypassed. Loopback/test source like
//! [`AmModulator`](crate::am_modulator)/[`FmModulator`](crate::fm_modulator);
//! no listen preset uses it.
//!
//! ### Phase accumulator
//!
//! `ampmodem` is same-rate (one audio in → one complex out at DC), so
//! interpolation + the carrier offset live here. The offset NCO is an
//! `f64` phase in cycles wrapped to `[0, 1)`, as in `FmModulator`.

use anyhow::{bail, Result};
use ferrite_liquid_dsp::{AmType, Ampmodem};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Sideband to transmit. Matches [`crate::ssb_demod::Sideband`]'s wire
/// form so a preset reads the same either side of the loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sideband {
    #[default]
    Usb,
    Lsb,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct SsbModulatorParams {
    pub input_rate_hz: f32,
    pub output_rate_hz: f32,
    /// Carrier offset from baseband DC. Non-zero so the channelizer
    /// (or AutoTune) has to find it — the realistic case.
    pub offset_hz: f32,
    pub sideband: Sideband,
}

impl Default for SsbModulatorParams {
    fn default() -> Self {
        Self {
            input_rate_hz: 8_000.0,
            output_rate_hz: 48_000.0,
            offset_hz: 12_000.0,
            sideband: Sideband::Usb,
        }
    }
}

pub struct SsbModulator {
    params: SsbModulatorParams,
    modem: Ampmodem,
    /// Linear-interp state (see [`crate::am_modulator`]).
    prev_in: f32,
    curr_in: f32,
    frac: f32,
    step: f32,
    /// Offset-carrier phase in cycles, wrapped to `[0, 1)`.
    phase: f64,
    inc_cyc: f64,
    primed: bool,
}

impl SsbModulator {
    pub fn new(params: SsbModulatorParams) -> Result<Self> {
        if !(params.input_rate_hz.is_finite() && params.input_rate_hz > 0.0) {
            bail!("ssb_modulator input_rate_hz must be > 0");
        }
        if !(params.output_rate_hz.is_finite() && params.output_rate_hz > 0.0) {
            bail!("ssb_modulator output_rate_hz must be > 0");
        }
        if !params.offset_hz.is_finite() {
            bail!("ssb_modulator offset_hz must be finite");
        }
        if f64::from(params.offset_hz.abs()) >= f64::from(params.output_rate_hz) / 2.0 {
            bail!(
                "ssb_modulator: |offset| {} Hz exceeds Nyquist at output rate {} Hz",
                params.offset_hz,
                params.output_rate_hz
            );
        }
        let kind = match params.sideband {
            Sideband::Usb => AmType::Usb,
            Sideband::Lsb => AmType::Lsb,
        };
        // mod_index 0.8 + suppressed carrier — identical to SsbDemod so
        // the pair is a matched inverse.
        let modem = Ampmodem::new(0.8, kind, true).map_err(|e| anyhow::anyhow!("ampmodem: {e}"))?;
        Ok(Self {
            params,
            modem,
            prev_in: 0.0,
            curr_in: 0.0,
            frac: 1.0,
            step: params.input_rate_hz / params.output_rate_hz,
            phase: 0.0,
            inc_cyc: f64::from(params.offset_hz) / f64::from(params.output_rate_hz),
            primed: false,
        })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for SsbModulator {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "SsbModulator",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[
                ParamSpec {
                    key: "input_rate_hz",
                    label: "Input sample rate",
                    kind: ParamKind::Range {
                        min: 1_000.0,
                        max: 10_000_000.0,
                        step: 1.0,
                        default: 8_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Audio-rate input (fldigi modes: 8 kHz).",
                },
                ParamSpec {
                    key: "output_rate_hz",
                    label: "Output sample rate",
                    kind: ParamKind::Range {
                        min: 1_000.0,
                        max: 100_000_000.0,
                        step: 1.0,
                        default: 48_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "IQ output rate. Match the channelizer input in the loopback chain.",
                },
                ParamSpec {
                    key: "offset_hz",
                    label: "Carrier offset",
                    kind: ParamKind::Range {
                        min: -50_000_000.0,
                        max: 50_000_000.0,
                        step: 1.0,
                        default: 12_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Where the SSB signal sits relative to baseband. Non-zero so the channelizer/AutoTune must tune to it (the realistic RX case).",
                },
                ParamSpec {
                    key: "sideband",
                    label: "Sideband",
                    kind: ParamKind::EnumString {
                        values: &["usb", "lsb"],
                        default: "usb",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "USB or LSB. Must match the paired SsbDemod for a clean round-trip; fldigi data modes are USB.",
                },
            ],
            ai_notes: "SSB modulator — audio → single-sideband IQ (wraps liquid ampmodem, exact inverse of SsbDemod). Loopback/test only: turns an audio fixture into an RF signal so the e2e runs the real channelizer→demod chain. Not in any listen preset.",
        }
    }

    fn relative_rate(&self, _in_port: usize, _out_port: usize) -> (u32, u32) {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let out_n = self.params.output_rate_hz.round().max(1.0) as u32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let in_n = self.params.input_rate_hz.round().max(1.0) as u32;
        (out_n, in_n)
    }

    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        Some(f64::from(self.params.output_rate_hz))
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
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
            .and_then(OutputPort::as_iq_f32_mut)
        else {
            return Ok(Work::new());
        };

        let mut consumed = 0;
        let mut produced = 0;
        while produced < dst.len() {
            while self.frac >= 1.0 {
                if consumed == src.len() {
                    let mut w = Work::new();
                    w.consumed[0] = consumed;
                    w.produced[0] = produced;
                    return Ok(w);
                }
                self.prev_in = self.curr_in;
                self.curr_in = src[consumed];
                consumed += 1;
                self.frac -= 1.0;
                self.primed = true;
            }
            if !self.primed {
                let mut w = Work::new();
                w.consumed[0] = consumed;
                w.produced[0] = produced;
                return Ok(w);
            }

            let audio = (1.0 - self.frac) * self.prev_in + self.frac * self.curr_in;
            let (sre, sim) = self.modem.modulate(audio);
            #[allow(clippy::cast_possible_truncation)]
            let theta = (self.phase as f32) * core::f32::consts::TAU;
            let (osin, ocos) = theta.sin_cos();
            // (sre + j sim) · (ocos + j osin)
            dst[produced] = Complex::new(sre * ocos - sim * osin, sre * osin + sim * ocos);

            self.phase += self.inc_cyc;
            self.phase -= self.phase.floor();
            self.frac += self.step;
            produced += 1;
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;
        w.produced[0] = produced;
        Ok(w)
    }
}

impl BlockFactory for SsbModulator {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: SsbModulatorParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(SsbModulator::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{Sideband, SsbModulator, SsbModulatorParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use num_complex::Complex;

    fn run(m: &mut SsbModulator, input: &[f32], out_len: usize) -> (Vec<Complex<f32>>, usize) {
        let mut out = vec![Complex::new(0.0_f32, 0.0); out_len];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(input),
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
        (out, w.produced[0])
    }

    #[test]
    fn rejects_bad_params() {
        assert!(SsbModulator::new(SsbModulatorParams {
            input_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(SsbModulator::new(SsbModulatorParams {
            output_rate_hz: 10_000.0,
            offset_hz: 9_000.0,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn produces_finite_iq_with_energy() {
        let mut m = SsbModulator::new(SsbModulatorParams {
            input_rate_hz: 8_000.0,
            output_rate_hz: 48_000.0,
            offset_hz: 6_000.0,
            sideband: Sideband::Usb,
        })
        .unwrap();
        let input: Vec<f32> = (0..2_000)
            .map(|i| 0.5 * (core::f32::consts::TAU * 700.0 * i as f32 / 8_000.0).sin())
            .collect();
        let (out, n) = run(&mut m, &input, 16_384);
        assert!(n > 1_000);
        for s in &out[..n] {
            assert!(s.re.is_finite() && s.im.is_finite());
        }
        let p: f32 = out[..n].iter().map(|c| c.norm_sqr()).sum::<f32>() / n as f32;
        assert!(p > 1e-4, "modulated SSB has no energy (p={p})");
    }

    #[test]
    fn round_trips_through_ssb_demod() {
        // Tone → SSB-modulate at offset → mix the offset back down →
        // SsbDemod (USB) → recover the tone. Asserts the matched
        // ampmodem pair reproduces the audio (correlation, gain-free).
        use crate::ssb_demod::{SsbDemod, SsbDemodParams};
        let fs_a = 8_000.0_f32;
        let fs_iq = 48_000.0_f32;
        let off = 6_000.0_f32;
        let f_tone = 900.0_f32;
        let n_in = 6_000;
        let input: Vec<f32> = (0..n_in)
            .map(|i| 0.6 * (core::f32::consts::TAU * f_tone * i as f32 / fs_a).sin())
            .collect();

        let mut modu = SsbModulator::new(SsbModulatorParams {
            input_rate_hz: fs_a,
            output_rate_hz: fs_iq,
            offset_hz: off,
            sideband: super::Sideband::Usb,
        })
        .unwrap();
        let (iq, np) = run(&mut modu, &input, 65_536);
        assert!(np > 10_000);

        // De-rotate the offset so the signal is at DC for SsbDemod.
        let mut base = vec![Complex::new(0.0_f32, 0.0); np];
        for (k, slot) in base.iter_mut().enumerate() {
            let th = -core::f32::consts::TAU * off * k as f32 / fs_iq;
            *slot = iq[k] * Complex::new(th.cos(), th.sin());
        }

        let mut demod = SsbDemod::new(SsbDemodParams {
            sample_rate_hz: fs_iq,
            sideband: crate::ssb_demod::Sideband::Usb,
            audio_gain: 1.0,
        })
        .unwrap();
        let mut audio = vec![0.0_f32; np];
        let mut ins = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&base),
        }];
        let mut outs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut audio),
        }];
        demod
            .process(&mut BlockIo {
                inputs: &mut ins,
                outputs: &mut outs,
            })
            .unwrap();

        // The recovered tone is at f_tone in `audio` (rate fs_iq).
        // Goertzel: it must dominate a nearby off-tone bin.
        let goertzel = |xs: &[f32], f: f32| -> f32 {
            let w = core::f32::consts::TAU * f / fs_iq;
            let c = 2.0 * w.cos();
            let (mut s1, mut s2) = (0.0f32, 0.0f32);
            for &x in xs {
                let s0 = c * s1 - s2 + x;
                s2 = s1;
                s1 = s0;
            }
            (s1 * s1 + s2 * s2 - c * s1 * s2).sqrt() / xs.len() as f32
        };
        let body = &audio[np / 4..np - 10];
        let tone = goertzel(body, f_tone);
        let off_tone = goertzel(body, f_tone + 350.0).max(goertzel(body, f_tone - 350.0));
        assert!(
            tone > 8.0 * off_tone,
            "SSB round-trip tone {tone} should dominate off-tone {off_tone}"
        );
    }
}
