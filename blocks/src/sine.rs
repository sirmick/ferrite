//! Complex sinusoid source — the project's first concrete [`Block`].
//!
//! Produces a constant-amplitude complex exponential at `tone_freq_abs_hz`
//! relative to `center_freq_hz`, sampled at `rate_hz`, into an `iq_f32`
//! output port.
//!
//! ### Why absolute frequency
//!
//! The tone is parameterised in **absolute Hz**, not as an offset from
//! the center. Tuning `center_freq_hz` therefore drags the tone across
//! the waterfall — exactly the way a physical signal behaves when you
//! retune a real radio. This is the intended demo UX and it's also what
//! hardware sources will look like once Phase C lands, so the mental
//! model lines up.
//!
//! ### Phase accumulator
//!
//! State is a `u32` **Q0.32 phase register** advanced by a `u32`
//! increment per sample. This is the standard trick: integer wrapping
//! is exact, so phase never drifts no matter how long the stream runs.
//! Only the conversion to radians for `sin_cos` touches floating point.

use anyhow::Result;
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, OutputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

/// Reciprocal of 2³². The value `2³²` is exactly representable in f32
/// (it's a power of two), so this constant has no rounding — dividing
/// a `u32` phase register by it yields an exact fractional turn in
/// `[0, 1)`.
const INV_PHASE_SCALE: f32 = 1.0_f32 / 4_294_967_296.0_f32;

/// Construction-time params. Every field is also present in the block's
/// param schema and addressable by key via [`SineSource::set_param`].
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct SineSourceParams {
    pub rate_hz: f64,
    pub center_freq_hz: f64,
    pub tone_freq_abs_hz: f64,
    pub amplitude: f32,
}

impl Default for SineSourceParams {
    fn default() -> Self {
        Self {
            rate_hz: 2_000_000.0,
            center_freq_hz: 100_000_000.0,
            tone_freq_abs_hz: 100_001_000.0,
            amplitude: 0.25,
        }
    }
}

pub struct SineSource {
    params: SineSourceParams,
    phase: u32,
    phase_inc: u32,
}

impl SineSource {
    #[must_use]
    pub fn new(params: SineSourceParams) -> Self {
        let mut s = Self {
            params,
            phase: 0,
            phase_inc: 0,
        };
        s.recompute_phase_inc();
        s
    }

    /// Q0.32 phase increment from the current rate and tone/center offset.
    /// Aliasing across ±Nyquist is handled naturally by `u32` wraparound.
    fn recompute_phase_inc(&mut self) {
        let offset = self.params.tone_freq_abs_hz - self.params.center_freq_hz;
        let cycles_per_sample = offset / self.params.rate_hz;
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let scaled = (cycles_per_sample * f64::from(u32::MAX) + cycles_per_sample) as i64;
        // Wrap into u32 modularly: preserves sign and aliasing semantics.
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        {
            self.phase_inc = (scaled as i32 as u32).wrapping_add(0);
        }
    }

    pub fn set_center_freq(&mut self, hz: f64) {
        self.params.center_freq_hz = hz;
        self.recompute_phase_inc();
    }

    pub fn set_tone_freq_abs(&mut self, hz: f64) {
        self.params.tone_freq_abs_hz = hz;
        self.recompute_phase_inc();
    }

    pub fn set_amplitude(&mut self, amp: f32) {
        self.params.amplitude = amp;
    }

    #[must_use]
    pub const fn phase(&self) -> u32 {
        self.phase
    }

    #[must_use]
    pub const fn phase_inc(&self) -> u32 {
        self.phase_inc
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for SineSource {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "SineSource",
            placement: Placement::Either,
            inputs: &[],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[
                ParamSpec {
                    key: "rate_hz",
                    label: "Sample rate",
                    kind: ParamKind::EnumNumeric {
                        values: &[
                            250_000.0,
                            1_000_000.0,
                            2_000_000.0,
                            2_400_000.0,
                            8_000_000.0,
                        ],
                        default: 2_000_000.0,
                        unit: "S/s",
                    },
                    // Source rate ripples through every downstream rate
                    // negotiation — treat as a source restart.
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
                ParamSpec {
                    key: "center_freq_hz",
                    label: "Center frequency",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 6_000_000_000.0,
                        step: 1.0,
                        default: 100_000_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "tone_freq_abs_hz",
                    label: "Tone frequency",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 6_000_000_000.0,
                        step: 1.0,
                        default: 100_001_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "amplitude",
                    label: "Amplitude",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1.0,
                        step: 0.001,
                        default: 0.25,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
            ],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(out) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_iq_f32_mut)
        else {
            return Ok(Work::new());
        };
        let amp = self.params.amplitude;
        let inc = self.phase_inc;
        let mut phase = self.phase;
        for s in out.iter_mut() {
            #[allow(clippy::cast_precision_loss)]
            let frac = phase as f32 * INV_PHASE_SCALE;
            let theta = frac * core::f32::consts::TAU;
            let (sin, cos) = theta.sin_cos();
            *s = Complex::new(amp * cos, amp * sin);
            phase = phase.wrapping_add(inc);
        }
        self.phase = phase;
        let n = out.len();
        let mut w = Work::new();
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for SineSource {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: SineSourceParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(SineSource::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{SineSource, SineSourceParams};
    use crate::block::{Block, BlockIo, OutBuf, OutputPort, PortMeta, Work};
    use num_complex::Complex;

    const fn default_params() -> SineSourceParams {
        SineSourceParams {
            rate_hz: 4.0,
            center_freq_hz: 0.0,
            tone_freq_abs_hz: 0.0,
            amplitude: 0.5,
        }
    }

    fn process_n(src: &mut SineSource, n: usize) -> Vec<Complex<f32>> {
        let mut buf = vec![Complex::new(0.0, 0.0); n];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut buf),
        }];
        let mut io = BlockIo {
            inputs: &mut [],
            outputs: &mut outputs,
        };
        let w: Work = src.process(&mut io).unwrap();
        assert_eq!(w.produced[0], n);
        buf
    }

    #[test]
    fn dc_tone_matches_amplitude() {
        // Tone exactly at center → phase_inc == 0 → every sample (amp, 0).
        let mut src = SineSource::new(default_params());
        assert_eq!(src.phase_inc(), 0);
        let out = process_n(&mut src, 16);
        for s in out {
            assert!((s.re - 0.5).abs() < 1e-6);
            assert!(s.im.abs() < 1e-6);
        }
    }

    #[test]
    fn quarter_rate_offset_cycles_iq() {
        // offset = rate/4 → phase_inc = 2^30 → 4-sample cycle:
        //   (a,0), (0,a), (-a,0), (0,-a).
        let params = SineSourceParams {
            rate_hz: 4.0,
            tone_freq_abs_hz: 1.0,
            ..default_params()
        };
        let mut src = SineSource::new(params);
        assert_eq!(src.phase_inc(), 1 << 30);
        let out = process_n(&mut src, 8);
        let a = 0.5_f32;
        let expect: [Complex<f32>; 4] = [
            Complex::new(a, 0.0),
            Complex::new(0.0, a),
            Complex::new(-a, 0.0),
            Complex::new(0.0, -a),
        ];
        for (i, s) in out.iter().enumerate() {
            let e = expect[i % 4];
            assert!((s.re - e.re).abs() < 1e-6, "re @ {i}: {} vs {}", s.re, e.re);
            assert!((s.im - e.im).abs() < 1e-6, "im @ {i}: {} vs {}", s.im, e.im);
        }
    }

    #[test]
    fn phase_survives_many_samples() {
        // After 2^20 samples at phase_inc = 2^30 we've wrapped 256 full
        // turns. Phase accumulator must land exactly back at 0.
        let params = SineSourceParams {
            rate_hz: 4.0,
            tone_freq_abs_hz: 1.0,
            ..default_params()
        };
        let mut src = SineSource::new(params);
        let _ = process_n(&mut src, 1 << 20);
        assert_eq!(src.phase(), 0);
    }

    #[test]
    fn runtime_updates_recompute_phase_inc() {
        let mut src = SineSource::new(default_params());
        assert_eq!(src.phase_inc(), 0);
        src.set_tone_freq_abs(1.0);
        assert_eq!(src.phase_inc(), 1 << 30);
        src.set_center_freq(1.0);
        assert_eq!(src.phase_inc(), 0);
    }

    #[test]
    fn negative_offset_wraps() {
        // offset = -rate/4 → phase_inc = wrapping_neg(2^30) = 0xC000_0000.
        let params = SineSourceParams {
            rate_hz: 4.0,
            center_freq_hz: 1.0,
            tone_freq_abs_hz: 0.0,
            ..default_params()
        };
        let src = SineSource::new(params);
        assert_eq!(src.phase_inc(), 0xC000_0000);
    }
}
