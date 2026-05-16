//! FM modulator — real audio → complex IQ at a higher sample rate.
//!
//! The exact inverse of [`FmDemod`](crate::fm_demod): for each output
//! sample the instantaneous frequency is the carrier offset plus the
//! interpolated message scaled by the peak deviation, and the phase is
//! its running integral.
//!
//! ```text
//! audio[k] = linear_interp(input, k · input_rate / output_rate)
//! f_inst   = offset_hz + deviation_hz · audio[k]          (audio ∈ [-1, 1])
//! θ        = θ + 2π · f_inst / output_rate
//! iq[k]    = (cos θ, sin θ)                                (constant envelope)
//! ```
//!
//! Pairs with `FmModulator → [Channelizer/Decimator] → FmDemod` to
//! verify the NBFM/WBFM chain end to end against a known message. There
//! is no listen preset that uses it — it is a loopback/test source like
//! [`AmModulator`](crate::am_modulator).
//!
//! ### Phase accumulator
//!
//! The per-sample frequency varies with the message, so the fixed-step
//! Q0.32 trick used by `AmModulator`/`SineSource` does not apply. We
//! integrate an `f64` phase in cycles and wrap it to `[0, 1)` every
//! sample — `f64` keeps the residual phase error far below the audio
//! noise floor over the longest fixture (a few hundred k samples).

use anyhow::{bail, Result};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct FmModulatorParams {
    pub input_rate_hz: f32,
    pub output_rate_hz: f32,
    /// Carrier offset from baseband DC. Keep `|offset| + deviation`
    /// inside the channel bandwidth; for the bare modulator↔demod
    /// round-trip leave it 0 so the recovered audio has no DC pedestal
    /// (a non-zero offset demodulates to `offset/deviation` of DC,
    /// which `FmDemod` clamps to ±1).
    pub offset_hz: f32,
    /// Peak frequency deviation. Audio full-scale (±1.0) maps to
    /// ±`deviation_hz`. Must match the paired `FmDemod`'s
    /// `max_deviation_hz` for unity round-trip. NBFM voice ≈ 2.5–5 kHz,
    /// WBFM broadcast = 75 kHz.
    pub deviation_hz: f32,
}

impl Default for FmModulatorParams {
    fn default() -> Self {
        Self {
            input_rate_hz: 48_000.0,
            output_rate_hz: 240_000.0,
            offset_hz: 0.0,
            deviation_hz: 75_000.0,
        }
    }
}

pub struct FmModulator {
    params: FmModulatorParams,
    /// Linear-interp state (see [`crate::am_modulator`] for the scheme).
    prev_in: f32,
    curr_in: f32,
    frac: f32,
    step: f32,
    /// Carrier phase in cycles, wrapped to `[0, 1)`.
    phase: f64,
    /// `offset / output_rate`, the per-sample carrier phase increment in
    /// cycles before the message contribution is added.
    offset_cyc: f64,
    /// `deviation / output_rate`, the per-sample cycles per unit of
    /// message amplitude.
    dev_cyc: f64,
    /// True once the first input sample has been pulled.
    primed: bool,
}

impl FmModulator {
    pub fn new(params: FmModulatorParams) -> Result<Self> {
        if !(params.input_rate_hz.is_finite() && params.input_rate_hz > 0.0) {
            bail!("fm_modulator input_rate_hz must be > 0");
        }
        if !(params.output_rate_hz.is_finite() && params.output_rate_hz > 0.0) {
            bail!("fm_modulator output_rate_hz must be > 0");
        }
        if !(params.deviation_hz.is_finite() && params.deviation_hz > 0.0) {
            bail!("fm_modulator deviation_hz must be > 0");
        }
        if !params.offset_hz.is_finite() {
            bail!("fm_modulator offset_hz must be finite");
        }
        // |offset| + deviation must stay under Nyquist or the carrier
        // (and its message excursions) alias in the IQ output.
        let peak = f64::from(params.offset_hz.abs()) + f64::from(params.deviation_hz);
        if peak >= f64::from(params.output_rate_hz) / 2.0 {
            bail!(
                "fm_modulator: |offset| + deviation = {peak} Hz exceeds Nyquist \
                 ({} Hz) at output rate {} Hz",
                f64::from(params.output_rate_hz) / 2.0,
                params.output_rate_hz
            );
        }
        let step = params.input_rate_hz / params.output_rate_hz;
        Ok(Self {
            params,
            prev_in: 0.0,
            curr_in: 0.0,
            frac: 1.0,
            step,
            phase: 0.0,
            offset_cyc: f64::from(params.offset_hz) / f64::from(params.output_rate_hz),
            dev_cyc: f64::from(params.deviation_hz) / f64::from(params.output_rate_hz),
            primed: false,
        })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for FmModulator {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "FmModulator",
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
                        default: 48_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Audio-rate input (typical 48 kHz).",
                },
                ParamSpec {
                    key: "output_rate_hz",
                    label: "Output sample rate",
                    kind: ParamKind::Range {
                        min: 1_000.0,
                        max: 100_000_000.0,
                        step: 1.0,
                        default: 240_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "IQ output rate. Match the demod input rate (e.g. the channelizer's 240 kHz).",
                },
                ParamSpec {
                    key: "offset_hz",
                    label: "Carrier offset",
                    kind: ParamKind::Range {
                        min: -50_000_000.0,
                        max: 50_000_000.0,
                        step: 1.0,
                        default: 0.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Carrier offset from baseband. 0 for a bare modulator↔demod round-trip; non-zero to make the channelizer tune it back.",
                },
                ParamSpec {
                    key: "deviation_hz",
                    label: "Peak deviation",
                    kind: ParamKind::Range {
                        min: 100.0,
                        max: 200_000.0,
                        step: 100.0,
                        default: 75_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Peak FM deviation; ±1.0 audio → ±deviation. Match the paired FmDemod max_deviation_hz. WBFM 75 kHz, NBFM 2.5–5 kHz.",
                },
            ],
            ai_notes: "FM modulator — audio + carrier → constant-envelope IQ. Loopback/test only (paired with FmDemod for end-to-end chain verification). Not a runtime block in any listen preset.",
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
            #[allow(clippy::cast_possible_truncation)]
            let theta = (self.phase as f32) * core::f32::consts::TAU;
            let (sin_p, cos_p) = theta.sin_cos();
            dst[produced] = Complex::new(cos_p, sin_p);

            // Integrate instantaneous frequency (cycles) and wrap to keep
            // `f64` precision bounded over long fixtures.
            self.phase += self.offset_cyc + self.dev_cyc * f64::from(audio);
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

impl BlockFactory for FmModulator {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: FmModulatorParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(FmModulator::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{FmModulator, FmModulatorParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use num_complex::Complex;

    fn run(
        m: &mut FmModulator,
        input: &[f32],
        out_len: usize,
    ) -> (Vec<Complex<f32>>, usize, usize) {
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
        (out, w.consumed[0], w.produced[0])
    }

    #[test]
    fn rejects_bad_params() {
        assert!(FmModulator::new(FmModulatorParams {
            input_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(FmModulator::new(FmModulatorParams {
            deviation_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        // |offset| + deviation past Nyquist.
        assert!(FmModulator::new(FmModulatorParams {
            input_rate_hz: 48_000.0,
            output_rate_hz: 100_000.0,
            offset_hz: 0.0,
            deviation_hz: 75_000.0,
        })
        .is_err());
    }

    #[test]
    fn constant_envelope() {
        let mut m = FmModulator::new(FmModulatorParams {
            input_rate_hz: 8_000.0,
            output_rate_hz: 80_000.0,
            offset_hz: 5_000.0,
            deviation_hz: 5_000.0,
        })
        .unwrap();
        let input: Vec<f32> = (0..64).map(|i| (i as f32 * 0.3).sin()).collect();
        let (out, _, produced) = run(&mut m, &input, 2048);
        assert!(produced > 16);
        for s in &out[1..produced] {
            let mag = (s.re * s.re + s.im * s.im).sqrt();
            assert!((mag - 1.0).abs() < 1e-4, "non-unit envelope {mag}");
        }
    }

    #[test]
    fn constant_audio_gives_constant_inst_frequency() {
        // offset 0, audio = +1 → instantaneous frequency = +deviation.
        // Per-sample phase advance should be 2π·deviation/out_rate.
        let params = FmModulatorParams {
            input_rate_hz: 8_000.0,
            output_rate_hz: 800_000.0,
            offset_hz: 0.0,
            deviation_hz: 100_000.0,
        };
        let mut m = FmModulator::new(params).unwrap();
        let input = vec![1.0_f32; 64];
        let (out, _, produced) = run(&mut m, &input, 4096);
        assert!(produced > 200);
        let a = out[100];
        let b = out[101];
        let expected = core::f32::consts::TAU * 100_000.0 / 800_000.0;
        let dphase = (b / a).arg();
        assert!(
            (dphase - expected).abs() < 1e-3,
            "phase advance got {dphase}, want {expected}"
        );
    }

    #[test]
    fn round_trips_through_fm_demod() {
        // Modulate a low tone, demodulate, recover it. offset 0 so there
        // is no DC pedestal to clamp.
        use crate::fm_demod::{FmDemod, FmDemodParams};
        let in_rate = 48_000.0_f32;
        let out_rate = 240_000.0_f32;
        let dev = 5_000.0_f32;
        let f_msg = 1_000.0_f32;
        let n_in = 4_096;
        let input: Vec<f32> = (0..n_in)
            .map(|i| 0.6 * (core::f32::consts::TAU * f_msg * i as f32 / in_rate).sin())
            .collect();

        let mut modu = FmModulator::new(FmModulatorParams {
            input_rate_hz: in_rate,
            output_rate_hz: out_rate,
            offset_hz: 0.0,
            deviation_hz: dev,
        })
        .unwrap();
        let (iq, _, np) = run(&mut modu, &input, 32_768);

        let mut demod = FmDemod::new(FmDemodParams {
            sample_rate_hz: out_rate,
            max_deviation_hz: dev,
        })
        .unwrap();
        let mut audio = vec![0.0_f32; np];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&iq[..np]),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut audio),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        demod.process(&mut io).unwrap();

        // Recovered audio runs at out_rate; the message period there is
        // out_rate/f_msg. Skip the discriminator settle and check the
        // recovered amplitude ≈ the 0.6 we modulated with.
        let body = &audio[2_000..np - 2_000];
        let peak = body.iter().fold(0.0_f32, |a, &x| a.max(x.abs()));
        assert!(
            (peak - 0.6).abs() < 0.08,
            "recovered peak {peak}, expected ~0.6"
        );
    }

    #[test]
    fn state_persists_across_process_calls() {
        let params = FmModulatorParams {
            input_rate_hz: 1_000.0,
            output_rate_hz: 4_000.0,
            offset_hz: 100.0,
            deviation_hz: 300.0,
        };
        let input: Vec<f32> = (0..40)
            .map(|i| (core::f32::consts::TAU * 50.0 * i as f32 / 1_000.0).sin())
            .collect();
        let mut whole = FmModulator::new(params).unwrap();
        let (out_whole, _, p_whole) = run(&mut whole, &input, 512);

        let mut split = FmModulator::new(params).unwrap();
        let (out_a, ca, pa) = run(&mut split, &input[..20], 512);
        let (out_b, _cb, pb) = run(&mut split, &input[ca..], 512);

        assert_eq!(pa + pb, p_whole);
        for i in 0..pa {
            assert!((out_a[i].re - out_whole[i].re).abs() < 1e-5);
            assert!((out_a[i].im - out_whole[i].im).abs() < 1e-5);
        }
        for i in 0..pb {
            assert!((out_b[i].re - out_whole[pa + i].re).abs() < 1e-5);
            assert!((out_b[i].im - out_whole[pa + i].im).abs() < 1e-5);
        }
    }
}
