//! FM broadcast de-emphasis — inverse of the transmitter's pre-emphasis
//! high-shelf. Standard one-pole IIR lowpass at `f_c = 1/(2π·τ)`.
//!
//! FM broadcasts pre-emphasize audio above ~2.1 kHz (US) or ~3.2 kHz
//! (Europe) so the signal spends more of its deviation budget on treble,
//! which is where the noise sits. The receiver has to apply the matched
//! de-emphasis or high frequencies sound artificially hissy — broadcast
//! FM without de-emphasis is a signature "too much treble" sound.
//!
//! ### IIR design
//!
//! First-order lowpass, exponential smoother form:
//!
//! ```text
//! α = dt / (τ + dt)        where dt = 1/fs
//! y[n] = α·x[n] + (1 − α)·y[n−1]
//! ```
//!
//! This matches the analog single-pole RC pre-emphasis exactly in the
//! bilinear-transform sense for the frequencies we care about (below
//! ~15 kHz). A higher-order match is not worth the complexity: the
//! audio band ends at 15 kHz anyway, well below the Nyquist of our
//! typical 48 kHz output.
//!
//! ### Common time constants
//!
//! | Region | τ      | Corner |
//! |--------|--------|--------|
//! | US     | 75 µs  | 2.12 kHz |
//! | Europe | 50 µs  | 3.18 kHz |
//!
//! Default is US. Change `tau_us` for Europe (50).
//!
//! ### Rate awareness
//!
//! Reads the scheduler's input rate at `init()` / `update_rates()` and
//! recomputes `α` so the corner frequency stays fixed at `1/(2π·τ)`
//! regardless of whether the upstream chain lands at 48 kHz, 48001 Hz
//! (from the resampler's fractional ratio), or anything else.

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct FmDeEmphasisParams {
    /// Time constant in microseconds. 75 = US, 50 = Europe.
    pub tau_us: f32,
    /// Input sample rate (Hz). Construction-time hint; init wins.
    pub sample_rate_hz: f32,
}

impl Default for FmDeEmphasisParams {
    fn default() -> Self {
        Self {
            tau_us: 75.0,
            sample_rate_hz: 48_000.0,
        }
    }
}

pub struct FmDeEmphasis {
    params: FmDeEmphasisParams,
    alpha: f32,
    prev: f32,
    /// Last-applied input rate so `update_rates` can skip the
    /// `(-dt/τ).exp()` recompute when nothing changed.
    input_rate_hz: f64,
}

impl FmDeEmphasis {
    pub fn new(params: FmDeEmphasisParams) -> Result<Self> {
        if !(params.tau_us.is_finite() && params.tau_us > 0.0) {
            bail!("fm_deemph: tau_us must be > 0 (got {})", params.tau_us);
        }
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "fm_deemph: sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        let mut s = Self {
            params,
            alpha: 0.0,
            prev: 0.0,
            input_rate_hz: 0.0,
        };
        s.recompute_for_rate(f64::from(params.sample_rate_hz));
        Ok(s)
    }

    fn recompute_for_rate(&mut self, input_rate_hz: f64) {
        let dt = 1.0 / input_rate_hz;
        let tau = f64::from(self.params.tau_us) * 1e-6;
        #[allow(clippy::cast_possible_truncation)]
        let alpha = (dt / (tau + dt)) as f32;
        self.alpha = alpha;
        self.input_rate_hz = input_rate_hz;
    }

    #[must_use]
    pub const fn alpha(&self) -> f32 {
        self.alpha
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for FmDeEmphasis {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "FmDeEmphasis",
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
                    key: "tau_us",
                    label: "Time constant",
                    kind: ParamKind::EnumNumeric {
                        // The only two values that exist in the wild —
                        // expose as a picker rather than a free slider
                        // so users don't accidentally type `10` and
                        // wonder why everything sounds muffled.
                        values: &[50.0, 75.0],
                        default: 75.0,
                        unit: "µs",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Input sample rate",
                    kind: ParamKind::Range {
                        min: 1_000.0,
                        max: 1_000_000.0,
                        step: 1.0,
                        default: 48_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
            ],
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.recompute_for_rate(rate);
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.recompute_for_rate(rate);
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
        let a = self.alpha;
        let one_minus_a = 1.0 - a;
        let mut y = self.prev;
        for i in 0..n {
            y = a * src[i] + one_minus_a * y;
            dst[i] = y;
        }
        self.prev = y;

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for FmDeEmphasis {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: FmDeEmphasisParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(FmDeEmphasis::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{FmDeEmphasis, FmDeEmphasisParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;

    fn run(block: &mut FmDeEmphasis, input: &[f32]) -> Vec<f32> {
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
        let w = block.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], input.len());
        assert_eq!(w.produced[0], input.len());
        out
    }

    #[test]
    fn rejects_bad_params() {
        assert!(FmDeEmphasis::new(FmDeEmphasisParams {
            tau_us: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(FmDeEmphasis::new(FmDeEmphasisParams {
            sample_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn dc_passes_through() {
        let mut b = FmDeEmphasis::new(FmDeEmphasisParams::default()).unwrap();
        let input = vec![0.4_f32; 512];
        let out = run(&mut b, &input);
        // First-order IIR settles at steady-state = input for constant
        // input. Skip the initial few samples while `prev=0` winds up.
        for (i, y) in out.iter().enumerate().skip(64) {
            assert!((y - 0.4).abs() < 1e-3, "out[{i}] = {y}");
        }
    }

    #[test]
    fn attenuates_above_corner() {
        // Analog first-order LPF magnitude at f = 10·f_c should be
        // ~1/sqrt(1 + 100) ≈ 0.0995 (−20 dB). We allow a ~30% margin
        // to cover the digital corner-frequency pre-warping.
        let fs = 48_000.0_f32;
        let tau_us = 75.0_f32;
        let f_c = 1.0 / (TAU * (tau_us * 1e-6));
        let f_test = 10.0 * f_c; // 21.2 kHz — above audio band but
                                 // well below Nyquist at 48 kHz.
        let n = 4096;
        let input: Vec<f32> = (0..n)
            .map(|i| (TAU * f_test * i as f32 / fs).cos())
            .collect();
        let mut b = FmDeEmphasis::new(FmDeEmphasisParams {
            tau_us,
            sample_rate_hz: fs,
        })
        .unwrap();
        let out = run(&mut b, &input);
        let rms =
            |x: &[f32]| -> f32 { (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt() };
        let warm = 512;
        let ratio = rms(&out[warm..]) / rms(&input[warm..]);
        assert!(
            ratio < 0.15,
            "10× corner should attenuate below 0.15, got {ratio}"
        );
    }

    #[test]
    fn state_persists_across_calls() {
        let params = FmDeEmphasisParams::default();
        let input: Vec<f32> = (0..256)
            .map(|i| (TAU * 1_000.0 * i as f32 / 48_000.0).cos())
            .collect();
        let mut whole = FmDeEmphasis::new(params).unwrap();
        let out_whole = run(&mut whole, &input);

        let mut split = FmDeEmphasis::new(params).unwrap();
        let first = run(&mut split, &input[..128]);
        let second = run(&mut split, &input[128..]);
        let mut joined = first;
        joined.extend_from_slice(&second);

        for (i, (a, b)) in out_whole.iter().zip(joined.iter()).enumerate() {
            assert!((a - b).abs() < 1e-6, "mismatch at {i}: whole={a} split={b}");
        }
    }
}
