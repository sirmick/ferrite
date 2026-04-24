//! AM modulator — real audio → complex IQ at a higher sample rate.
//!
//! One `real_f32` in at `input_rate_hz`, one `iq_f32` out at
//! `output_rate_hz`. For each output sample:
//!
//! ```text
//! audio[k] = linear_interp(input, k · input_rate / output_rate)
//! iq[k]    = (1 + mod_depth · audio[k]) · exp(j · 2π · offset_hz · k / output_rate)
//! ```
//!
//! Used by the DTMF E2E test to synthesise a realistic upmixed AM
//! signal from a low-rate audio stream. Linear interpolation is cheap
//! and good enough — any interpolation artefacts sit at `input_rate_hz`
//! and its harmonics, well outside the 240 kHz channel bandwidth we
//! channelize down to.
//!
//! ### Phase accumulator
//!
//! Q0.32 integer phase like [`crate::sine::SineSource`]; the signed
//! increment wraps so positive `offset_hz` lands the AM signal above
//! the output centre.

use anyhow::{bail, Result};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

const INV_PHASE_SCALE: f32 = 1.0_f32 / 4_294_967_296.0_f32;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct AmModulatorParams {
    pub input_rate_hz: f32,
    pub output_rate_hz: f32,
    pub offset_hz: f32,
    pub mod_depth: f32,
}

impl Default for AmModulatorParams {
    fn default() -> Self {
        Self {
            input_rate_hz: 8_000.0,
            output_rate_hz: 2_400_000.0,
            offset_hz: 50_000.0,
            mod_depth: 0.7,
        }
    }
}

pub struct AmModulator {
    params: AmModulatorParams,
    /// Linear-interp state: most-recent two input samples and the
    /// fractional position between them in `[0, 1)`.
    prev_in: f32,
    curr_in: f32,
    /// How far through the current `[prev_in, curr_in]` pair we are.
    /// Advances by `step = input_rate / output_rate` per output sample;
    /// when it crosses 1.0 we pull the next input sample.
    frac: f32,
    /// `input_rate / output_rate`, precomputed.
    step: f32,
    /// Q0.32 carrier-phase register.
    phase: u32,
    phase_inc: u32,
    /// True until the first input sample has been pulled — suppresses
    /// output so we don't emit modulated samples against the zero prev.
    primed: bool,
}

impl AmModulator {
    pub fn new(params: AmModulatorParams) -> Result<Self> {
        if !(params.input_rate_hz.is_finite() && params.input_rate_hz > 0.0) {
            bail!("am_modulator input_rate_hz must be > 0");
        }
        if !(params.output_rate_hz.is_finite() && params.output_rate_hz > 0.0) {
            bail!("am_modulator output_rate_hz must be > 0");
        }
        if !params.mod_depth.is_finite() || params.mod_depth < 0.0 {
            bail!("am_modulator mod_depth must be >= 0");
        }
        let step = params.input_rate_hz / params.output_rate_hz;
        let phase_inc = phase_inc_for(params.offset_hz, params.output_rate_hz);
        Ok(Self {
            params,
            prev_in: 0.0,
            curr_in: 0.0,
            // Start at 1.0 so the first output iteration triggers a pull.
            frac: 1.0,
            step,
            phase: 0,
            phase_inc,
            primed: false,
        })
    }

    pub fn set_offset_hz(&mut self, offset_hz: f32) {
        self.params.offset_hz = offset_hz;
        self.phase_inc = phase_inc_for(offset_hz, self.params.output_rate_hz);
    }

    #[must_use]
    pub const fn phase_inc(&self) -> u32 {
        self.phase_inc
    }
}

fn phase_inc_for(offset_hz: f32, output_rate_hz: f32) -> u32 {
    let cycles_per_sample = f64::from(offset_hz) / f64::from(output_rate_hz);
    #[allow(clippy::cast_possible_truncation)]
    let scaled = (cycles_per_sample * f64::from(u32::MAX) + cycles_per_sample) as i64;
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    {
        scaled as i32 as u32
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for AmModulator {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "AmModulator",
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
                },
                ParamSpec {
                    key: "output_rate_hz",
                    label: "Output sample rate",
                    kind: ParamKind::Range {
                        min: 1_000.0,
                        max: 100_000_000.0,
                        step: 1.0,
                        default: 2_400_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
                ParamSpec {
                    key: "offset_hz",
                    label: "Carrier offset",
                    kind: ParamKind::Range {
                        min: -50_000_000.0,
                        max: 50_000_000.0,
                        step: 1.0,
                        default: 50_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "mod_depth",
                    label: "Modulation depth",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1.0,
                        step: 0.001,
                        default: 0.7,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
            ],
        }
    }

    fn relative_rate(&self, _in_port: usize, _out_port: usize) -> (u32, u32) {
        // Interpolator: for every 1 real input sample the block emits
        // `output_rate / input_rate` iq output samples. Rates may be
        // non-integer; round to the nearest u32 for the scheduler's
        // integer math. Guard divide-by-zero at construction.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let out_n = self.params.output_rate_hz.round().max(1.0) as u32;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let in_n = self.params.input_rate_hz.round().max(1.0) as u32;
        (out_n, in_n)
    }

    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        // Rate-changing block: declare the configured output so the
        // runtime's rate-propagation pass reports the upsampled rate
        // to downstream consumers rather than the 1:1 fallback.
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
            // Pull input samples until `frac` is in [0, 1). For the
            // typical upsampling case step << 1, this inner loop runs
            // at most once per output sample and usually zero times.
            while self.frac >= 1.0 {
                if consumed == src.len() {
                    // Out of input — yield, the scheduler will call us
                    // again once the upstream produces more.
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
                // Never happens in practice (we start with frac=1.0 so
                // the first iteration pulls) but guard against a future
                // init change.
                let mut w = Work::new();
                w.consumed[0] = consumed;
                w.produced[0] = produced;
                return Ok(w);
            }

            let audio = (1.0 - self.frac) * self.prev_in + self.frac * self.curr_in;
            #[allow(clippy::cast_precision_loss)]
            let theta = self.phase as f32 * INV_PHASE_SCALE * core::f32::consts::TAU;
            let (sin_p, cos_p) = theta.sin_cos();
            let env = 1.0 + self.params.mod_depth * audio;
            dst[produced] = Complex::new(env * cos_p, env * sin_p);
            self.phase = self.phase.wrapping_add(self.phase_inc);
            self.frac += self.step;
            produced += 1;
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;
        w.produced[0] = produced;
        Ok(w)
    }
}

impl BlockFactory for AmModulator {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: AmModulatorParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(AmModulator::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{AmModulator, AmModulatorParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;
    use num_complex::Complex;

    fn run(
        m: &mut AmModulator,
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
        assert!(AmModulator::new(AmModulatorParams {
            input_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(AmModulator::new(AmModulatorParams {
            output_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(AmModulator::new(AmModulatorParams {
            mod_depth: -0.1,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn zero_offset_zero_depth_gives_constant_carrier() {
        // offset=0 → phase stays at 0; depth=0 → envelope is a constant
        // 1.0. Output should be (1.0, 0.0) for every sample regardless
        // of input.
        let params = AmModulatorParams {
            input_rate_hz: 8_000.0,
            output_rate_hz: 48_000.0,
            offset_hz: 0.0,
            mod_depth: 0.0,
        };
        let mut m = AmModulator::new(params).unwrap();
        let input = vec![0.5_f32; 32];
        let (out, _, produced) = run(&mut m, &input, 256);
        assert!(produced > 16);
        // Skip the first sample so we're past the priming pull.
        for s in &out[1..produced] {
            assert!((s.re - 1.0).abs() < 1e-5);
            assert!(s.im.abs() < 1e-5);
        }
    }

    #[test]
    fn rate_invariant_total_out_matches_step() {
        // At step = 1/3, feeding N input samples should emit ≈ 3N output
        // samples (within ±1 for the last fractional position).
        let params = AmModulatorParams {
            input_rate_hz: 1_000.0,
            output_rate_hz: 3_000.0,
            offset_hz: 0.0,
            mod_depth: 0.0,
        };
        let mut m = AmModulator::new(params).unwrap();
        let input = vec![0.0_f32; 100];
        let (_, consumed, produced) = run(&mut m, &input, 500);
        assert_eq!(consumed, input.len());
        // 100 inputs at step=1/3 → 99 outputs before we run out of input
        // (we pull the 100th input, then stop on exhaustion). Tolerate
        // ±3 for start-up/end-of-buffer accounting.
        assert!((produced as i64 - 300).abs() <= 3, "produced={produced}");
    }

    #[test]
    fn iq_magnitude_tracks_envelope() {
        // depth=1.0, constant input 0.0 → env = 1 → |iq| = 1.
        // Then flip to input = 1.0 → env = 2 → |iq| = 2.
        let params = AmModulatorParams {
            input_rate_hz: 1_000.0,
            output_rate_hz: 100_000.0,
            offset_hz: 10_000.0,
            mod_depth: 1.0,
        };
        let mut m = AmModulator::new(params).unwrap();

        // Phase 1: zero audio → unit envelope.
        let zero_in = vec![0.0_f32; 50];
        let (out_zero, _, p_zero) = run(&mut m, &zero_in, 4096);
        assert!(p_zero > 0);
        let mag_zero: f32 = out_zero[10..p_zero]
            .iter()
            .map(|c| (c.re * c.re + c.im * c.im).sqrt())
            .sum::<f32>()
            / (p_zero - 10) as f32;
        assert!(
            (mag_zero - 1.0).abs() < 1e-3,
            "zero audio should give unit envelope, got {mag_zero}"
        );

        // Phase 2: full audio=1 → envelope 2.0.
        // Need enough input samples for the output buffer (step = 0.01
        // → 4096 outputs consume ~41 inputs). Let the interp prime past
        // the old 0.0 before we sample.
        let one_in = vec![1.0_f32; 100];
        let (out_one, _, p_one) = run(&mut m, &one_in, 4096);
        assert!(p_one > 0);
        // Skip the ramp-up past the linear-interp transition.
        let mag_one: f32 = out_one[2048..p_one]
            .iter()
            .map(|c| (c.re * c.re + c.im * c.im).sqrt())
            .sum::<f32>()
            / (p_one - 2048) as f32;
        assert!(
            (mag_one - 2.0).abs() < 0.05,
            "full audio should double envelope, got {mag_one}"
        );
    }

    #[test]
    fn offset_shifts_tone_above_centre() {
        // Constant unit envelope at +100 kHz offset should produce a
        // complex tone rotating at +100 kHz in the output spectrum.
        // Verify by checking the phase advance between successive
        // samples matches 2π · offset / out_rate.
        let params = AmModulatorParams {
            input_rate_hz: 8_000.0,
            output_rate_hz: 800_000.0,
            offset_hz: 100_000.0,
            mod_depth: 0.0,
        };
        let mut m = AmModulator::new(params).unwrap();
        let input = vec![0.0_f32; 32];
        let (out, _, produced) = run(&mut m, &input, 1024);
        assert!(produced > 8);
        // Skip the priming pull.
        let a = out[100];
        let b = out[101];
        // phase(b) - phase(a) should equal 2π · 100_000 / 800_000 = π/4.
        let expected = TAU / 8.0;
        let dphase = (b / a).arg();
        assert!(
            (dphase - expected).abs() < 1e-3,
            "per-sample phase advance: got {dphase}, want {expected}"
        );
    }

    #[test]
    fn state_persists_across_process_calls() {
        // Two short calls should produce the same output as one long
        // call — phase, frac, and the interp ring must carry over.
        let params = AmModulatorParams {
            input_rate_hz: 1_000.0,
            output_rate_hz: 4_000.0,
            offset_hz: 200.0,
            mod_depth: 0.5,
        };
        let input: Vec<f32> = (0..40)
            .map(|i| (TAU * 50.0 * i as f32 / 1_000.0).sin())
            .collect();
        let mut whole = AmModulator::new(params).unwrap();
        let (out_whole, _, p_whole) = run(&mut whole, &input, 512);

        let mut split = AmModulator::new(params).unwrap();
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
