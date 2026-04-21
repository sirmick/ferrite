//! RealF32 counterpart to [`crate::decimator::Decimator`].
//!
//! Integer-factor decimator for real-valued streams, built from the same
//! Hann-windowed sinc LPF design. Exists so that signal chains that have
//! already left the complex-baseband domain — typically a demodulator
//! output at channel rate — can be rate-reduced to audio rate without
//! the aliasing that arises from decimating IQ *before* discrimination.

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};
use crate::decimator::design_lpf;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct RealF32DecimatorParams {
    pub factor: usize,
    pub num_taps: usize,
    pub cutoff_normalized: f32,
}

impl RealF32DecimatorParams {
    #[must_use]
    pub fn for_factor(factor: usize) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let cutoff_normalized = if factor == 0 {
            0.4
        } else {
            0.4 / factor as f32
        };
        Self {
            factor,
            num_taps: 8 * factor + 1,
            cutoff_normalized,
        }
    }
}

impl Default for RealF32DecimatorParams {
    fn default() -> Self {
        Self::for_factor(4)
    }
}

pub struct RealF32Decimator {
    factor: usize,
    taps: Vec<f32>,
    delay: Vec<f32>,
    delay_idx: usize,
    phase: usize,
}

impl RealF32Decimator {
    pub fn new(params: RealF32DecimatorParams) -> Result<Self> {
        if params.factor == 0 {
            bail!("real decimator factor must be >= 1");
        }
        if params.num_taps < 3 {
            bail!("real decimator num_taps must be >= 3");
        }
        if !(params.cutoff_normalized > 0.0 && params.cutoff_normalized < 0.5) {
            bail!(
                "real decimator cutoff_normalized must be in (0, 0.5), got {}",
                params.cutoff_normalized
            );
        }
        let taps = design_lpf(params.num_taps, params.cutoff_normalized);
        Ok(Self {
            factor: params.factor,
            delay: vec![0.0; taps.len()],
            taps,
            delay_idx: 0,
            phase: 0,
        })
    }

    #[must_use]
    pub fn taps(&self) -> &[f32] {
        &self.taps
    }

    #[must_use]
    pub const fn factor(&self) -> usize {
        self.factor
    }

    fn fir_output(&self) -> f32 {
        let n = self.taps.len();
        let mut acc = 0.0_f32;
        let mut idx = self.delay_idx;
        for &t in &self.taps {
            acc += self.delay[idx] * t;
            idx += 1;
            if idx == n {
                idx = 0;
            }
        }
        acc
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for RealF32Decimator {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "RealF32Decimator",
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
                    key: "factor",
                    label: "Decimation factor",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 1024.0,
                        step: 1.0,
                        default: 4.0,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                },
                ParamSpec {
                    key: "num_taps",
                    label: "FIR length",
                    kind: ParamKind::Range {
                        min: 3.0,
                        max: 2048.0,
                        step: 2.0,
                        default: 33.0,
                        unit: "taps",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                },
                ParamSpec {
                    key: "cutoff_normalized",
                    label: "Cutoff (× input rate)",
                    kind: ParamKind::Range {
                        min: 0.001,
                        max: 0.499,
                        step: 0.001,
                        default: 0.1,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                },
            ],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn relative_rate(&self, _in_port: usize, _out_port: usize) -> (u32, u32) {
        #[allow(clippy::cast_possible_truncation)]
        let factor = self.factor as u32;
        (1, factor.max(1))
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

        let n = self.taps.len();
        let mut consumed = 0;
        let mut produced = 0;
        for &x in src {
            if self.phase + 1 == self.factor && produced == dst.len() {
                break;
            }
            self.delay[self.delay_idx] = x;
            self.delay_idx += 1;
            if self.delay_idx == n {
                self.delay_idx = 0;
            }
            consumed += 1;
            self.phase += 1;
            if self.phase == self.factor {
                self.phase = 0;
                dst[produced] = self.fir_output();
                produced += 1;
            }
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;
        w.produced[0] = produced;
        Ok(w)
    }
}

impl BlockFactory for RealF32Decimator {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: RealF32DecimatorParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(RealF32Decimator::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{RealF32Decimator, RealF32DecimatorParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;

    fn run(dec: &mut RealF32Decimator, input: &[f32], out_len: usize) -> (Vec<f32>, usize, usize) {
        let mut out = vec![0.0_f32; out_len];
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
        let w = dec.process(&mut io).unwrap();
        (out, w.consumed[0], w.produced[0])
    }

    #[test]
    fn constructor_rejects_bad_params() {
        assert!(RealF32Decimator::new(RealF32DecimatorParams {
            factor: 0,
            num_taps: 33,
            cutoff_normalized: 0.1
        })
        .is_err());
        assert!(RealF32Decimator::new(RealF32DecimatorParams {
            factor: 2,
            num_taps: 2,
            cutoff_normalized: 0.1
        })
        .is_err());
        assert!(RealF32Decimator::new(RealF32DecimatorParams {
            factor: 2,
            num_taps: 33,
            cutoff_normalized: 0.0
        })
        .is_err());
        assert!(RealF32Decimator::new(RealF32DecimatorParams {
            factor: 2,
            num_taps: 33,
            cutoff_normalized: 0.5
        })
        .is_err());
    }

    #[test]
    fn dc_passes_with_unit_gain() {
        let mut dec = RealF32Decimator::new(RealF32DecimatorParams::for_factor(4)).unwrap();
        let input = vec![1.0_f32; 4 * dec.taps().len()];
        let (out, _, produced) = run(&mut dec, &input, input.len() / 4 + 4);
        for s in &out[out.len() - 4..produced.min(out.len())] {
            assert!((s - 1.0).abs() < 1e-4, "out = {s}");
        }
    }

    #[test]
    fn rate_invariant_holds_across_calls() {
        let mut dec = RealF32Decimator::new(RealF32DecimatorParams::for_factor(3)).unwrap();
        let chunks: [usize; 6] = [1, 2, 3, 4, 7, 11];
        let total_in: usize = chunks.iter().sum();
        let mut total_out = 0;
        for &n in &chunks {
            let input = vec![0.25_f32; n];
            let (_, consumed, produced) = run(&mut dec, &input, n + 1);
            assert_eq!(consumed, n);
            total_out += produced;
        }
        assert_eq!(total_out, total_in / 3);
    }

    #[test]
    fn in_band_tone_survives_out_of_band_tone_is_rejected() {
        // factor = 4: output Nyquist at 0.125 of input rate. Real tone at
        // 0.05 is well in-band; 0.30 is well above cutoff.
        let params = RealF32DecimatorParams {
            factor: 4,
            num_taps: 65,
            cutoff_normalized: 0.11,
        };

        let measure_gain = |tone_hz_norm: f32| -> f32 {
            let mut dec = RealF32Decimator::new(params).unwrap();
            let n = 4096_usize;
            let input: Vec<f32> = (0..n)
                .map(|i| (TAU * tone_hz_norm * i as f32).cos())
                .collect();
            let (out, _, p) = run(&mut dec, &input, n / 4 + 4);
            let warm = params.num_taps / params.factor + 4;
            let ms: f32 = out[warm..p].iter().map(|s| s * s).sum::<f32>() / (p - warm) as f32;
            ms.sqrt()
        };

        // Real cosine has amplitude 1 but RMS is 1/sqrt(2) ≈ 0.707.
        let in_band = measure_gain(0.05);
        let out_of_band = measure_gain(0.30);
        assert!(
            in_band > 0.6,
            "in-band tone attenuated: {in_band} (expected > 0.6)"
        );
        assert!(
            out_of_band < 0.05,
            "out-of-band tone leaked through: {out_of_band} (expected < 0.05)"
        );
    }

    #[test]
    fn output_backpressure_pauses_cleanly() {
        let mut dec = RealF32Decimator::new(RealF32DecimatorParams::for_factor(4)).unwrap();
        let input = vec![1.0_f32; 40];
        let (_, consumed_a, produced_a) = run(&mut dec, &input, 2);
        assert_eq!(produced_a, 2);
        assert!(consumed_a <= input.len());
        let (_, consumed_b, produced_b) = run(&mut dec, &input[consumed_a..], 20);
        assert_eq!(produced_a + produced_b, 10);
        assert_eq!(consumed_a + consumed_b, input.len());
    }
}
