//! RealF32 counterpart to [`crate::decimator::Decimator`], backed by
//! liquid-dsp's `firdecim_rrrf`.
//!
//! Integer-factor decimator for real-valued streams. Same windowed-
//! sinc LPF design as `Decimator` (we share the `design_lpf` helper),
//! but the FIR + decimation engine is liquid's polyphase
//! implementation rather than the previous hand-rolled scalar loop.
//! That cuts the per-sample tap multiplication count by ~`factor`× —
//! liquid only computes the taps that contribute to the output sample
//! it's about to emit.
//!
//! Public API (params, ports, registry name `RealF32Decimator`) is
//! unchanged. Existing presets and tests work as-is.

use anyhow::{bail, Result};
use ferrite_liquid_dsp::Firdecim;
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
    inner: Firdecim,
    /// Scratch buffer of size `factor` so we can pass a contiguous
    /// slice to liquid's `_execute` (which wants exactly `factor`
    /// samples per call). Filled across `process()` invocations.
    chunk: Vec<f32>,
    chunk_len: usize,
    /// Input rate cached at init, used to report `output_rate_hz =
    /// input / factor` during rate propagation. `0.0` pre-init.
    input_rate_hz: f64,
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
        // factor=1 is the degenerate "no decimation" case. liquid's
        // firdecim wants `factor >= 2`; we handle it by leaving
        // `inner` configured for factor=2 but never delivering an
        // output (the chunk stays at length 0 forever) — actually
        // simpler: just synthesise a passthrough by routing through
        // a Firdecim with factor=max(2, factor). For factor=1 we
        // skip the decimator and pass samples through directly.
        // Existing tests don't exercise factor=1; keeping it as an
        // honest "factor=1 means no-op passthrough" shape.
        let factor_u32 = u32::try_from(params.factor.max(2))
            .map_err(|_| anyhow::anyhow!("factor exceeds u32"))?;
        let inner = Firdecim::from_taps(factor_u32, &taps)
            .map_err(|e| anyhow::anyhow!("firdecim_rrrf: {e}"))?;
        Ok(Self {
            factor: params.factor,
            taps,
            inner,
            chunk: vec![0.0; params.factor.max(1)],
            chunk_len: 0,
            input_rate_hz: 0.0,
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

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 {
                self.input_rate_hz = rate;
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 {
                self.input_rate_hz = rate;
            }
        }
        Ok(())
    }

    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        if self.input_rate_hz <= 0.0 || self.factor == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some(self.input_rate_hz / self.factor as f64)
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

        // factor=1 degenerate case: passthrough.
        if self.factor == 1 {
            let n = src.len().min(dst.len());
            dst[..n].copy_from_slice(&src[..n]);
            let mut w = Work::new();
            w.consumed[0] = n;
            w.produced[0] = n;
            return Ok(w);
        }

        let mut consumed = 0;
        let mut produced = 0;
        for &x in src {
            // Reserve room for one more output before consuming the
            // next input that would complete a chunk — the scheduler
            // will re-call us when there's space.
            if self.chunk_len + 1 == self.factor && produced == dst.len() {
                break;
            }
            self.chunk[self.chunk_len] = x;
            self.chunk_len += 1;
            consumed += 1;
            if self.chunk_len == self.factor {
                let y = self.inner.execute(&self.chunk[..self.factor]);
                dst[produced] = y;
                produced += 1;
                self.chunk_len = 0;
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
        let in_band = measure_gain(0.05);
        let out_of_band = measure_gain(0.30);
        assert!(in_band > 0.6, "in-band tone attenuated: {in_band}");
        assert!(
            out_of_band < 0.05,
            "out-of-band tone leaked through: {out_of_band}"
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
