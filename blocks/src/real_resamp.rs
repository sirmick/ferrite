//! Fractional resampler for real float streams — wraps liquid-dsp's
//! `msresamp_rrrf` in a Ferrite block.
//!
//! Arbitrary rate ratios: e.g. 200 kS/s → 48 kS/s (ratio 0.24), 240
//! kS/s → 48 kS/s (ratio 0.2), or any upsample. The ratio comes from
//! `output_rate_hz / input_rate_hz` at init time, so the preset just
//! declares the target audio rate and the block snaps to whatever the
//! source's actual channelizer output is — no preset rewrite needed
//! when the SDR ladder snaps the source rate.

use anyhow::{bail, Result};
use ferrite_liquid_dsp::MsResamp;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct RealF32ResampParams {
    /// Target output rate in Hz. The block reads its actual input rate
    /// from `InitCtx` at init time and computes `ratio = output_rate_hz
    /// / input_rate_hz`. Must be > 0.
    pub output_rate_hz: f64,
    /// Stop-band attenuation for the internal cascaded half-band +
    /// Farrow stages, in dB. 60 dB is a good default for audio; bump to
    /// 80 dB for tight channels, drop to 40 dB if CPU matters more than
    /// out-of-band rejection.
    pub stopband_db: f32,
}

impl Default for RealF32ResampParams {
    fn default() -> Self {
        Self {
            output_rate_hz: 48_000.0,
            stopband_db: 60.0,
        }
    }
}

pub struct RealF32Resamp {
    params: RealF32ResampParams,
    /// Lazily materialised inside `init()` when the actual input rate
    /// is known. Kept in an `Option` so reconfigure can rebuild without
    /// losing the surrounding block instance.
    inner: Option<MsResamp>,
    /// Scratch for one `execute` call. Sized to
    /// `MsResamp::max_output_for(input_len)`.
    scratch: Vec<f32>,
}

impl RealF32Resamp {
    pub fn new(params: RealF32ResampParams) -> Result<Self> {
        if !(params.output_rate_hz.is_finite() && params.output_rate_hz > 0.0) {
            bail!(
                "RealF32Resamp output_rate_hz must be > 0, got {}",
                params.output_rate_hz
            );
        }
        if !(params.stopband_db.is_finite() && params.stopband_db > 0.0) {
            bail!(
                "RealF32Resamp stopband_db must be > 0, got {}",
                params.stopband_db
            );
        }
        Ok(Self {
            params,
            inner: None,
            scratch: Vec::new(),
        })
    }

    fn rebuild_for_input_rate(&mut self, input_rate_hz: f64) -> Result<()> {
        if !(input_rate_hz.is_finite() && input_rate_hz > 0.0) {
            bail!("RealF32Resamp init: input rate must be > 0");
        }
        #[allow(clippy::cast_possible_truncation)]
        let rate = (self.params.output_rate_hz / input_rate_hz) as f32;
        let inner = MsResamp::new(rate, self.params.stopband_db)
            .map_err(|e| anyhow::anyhow!("msresamp_rrrf: {e}"))?;
        self.inner = Some(inner);
        self.scratch.clear();
        Ok(())
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for RealF32Resamp {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "RealF32Resamp",
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
                    key: "output_rate_hz",
                    label: "Output rate",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 10.0e6,
                        step: 1.0,
                        default: 48_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "Target sample rate. Typically the audio rate downstream blocks expect (e.g. 48 kHz for AudioSink).",
                },
                ParamSpec {
                    key: "stopband_db",
                    label: "Stop-band attenuation",
                    kind: ParamKind::Range {
                        min: 20.0,
                        max: 120.0,
                        step: 1.0,
                        default: 60.0,
                        unit: "dB",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Anti-alias filter rejection. 60 dB is the standard default; raise for cleaner output, lower if CPU is tight.",
                },
            ],
            ai_notes: "Polyphase real-valued resampler. Sits between a post-demod audio chain and an output rate that doesn't match (e.g. demod at 240 kHz → AudioSink at 48 kHz).",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        let input_rate = ctx
            .input_rate("in")
            .filter(|r| *r > 0.0)
            .ok_or_else(|| anyhow::anyhow!("RealF32Resamp init: input rate unknown"))?;
        self.rebuild_for_input_rate(input_rate)?;
        let ratio = self.params.output_rate_hz / input_rate;
        tracing::info!(
            target: "flowdiag",
            block = "RealF32Resamp",
            input_rate_hz = input_rate,
            output_rate_hz = self.params.output_rate_hz,
            ratio,
            "resamp init",
        );
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        let input_rate = ctx
            .input_rate("in")
            .filter(|r| *r > 0.0)
            .ok_or_else(|| anyhow::anyhow!("RealF32Resamp update_rates: input rate unknown"))?;
        self.rebuild_for_input_rate(input_rate)?;
        let ratio = self.params.output_rate_hz / input_rate;
        tracing::info!(
            target: "flowdiag",
            block = "RealF32Resamp",
            input_rate_hz = input_rate,
            output_rate_hz = self.params.output_rate_hz,
            ratio,
            "resamp rate update",
        );
        Ok(())
    }

    fn relative_rate(&self, _in_port: usize, _out_port: usize) -> (u32, u32) {
        // Fractional resampler — scheduler sizing uses 1:1 as the
        // coarse ratio. `process()` below self-throttles via `nx_cap`
        // so it never overflows the output buffer regardless of the
        // actual ratio, so a conservative rate here is safe.
        (1, 1)
    }

    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        // Declared target — downstream propagation uses this rather
        // than the upstream rate, since the block's whole purpose is
        // to pin the output to this value regardless of input.
        Some(self.params.output_rate_hz)
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(inner) = self.inner.as_mut() else {
            return Ok(Work::new());
        };
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
        if src.is_empty() || dst.is_empty() {
            return Ok(Work::new());
        }
        // Bound the input we feed liquid this call so the max-output
        // upper bound fits `dst.len()` with room to spare. Derived
        // from `max_output_for`'s formula: `out ≤ 1 + 2·r·nx + 8`, so
        // `nx ≤ (dst.len() - 9) / (2·r)`. When ratio is tiny (big
        // downsample) this leaves `nx` huge; we clamp to src.len().
        let rate = inner.rate() as f64;
        let nx_cap = if rate > 0.0 {
            let cap = ((dst.len().saturating_sub(9)) as f64 / (2.0 * rate)).floor() as usize;
            cap.max(1)
        } else {
            1
        };
        let nx = src.len().min(nx_cap).min(src.len());
        if nx == 0 {
            return Ok(Work::new());
        }
        let max_out = inner.max_output_for(nx);
        if self.scratch.len() < max_out {
            self.scratch.resize(max_out, 0.0);
        }
        let produced = inner.execute(&src[..nx], &mut self.scratch[..max_out]);
        let written = produced.min(dst.len());
        dst[..written].copy_from_slice(&self.scratch[..written]);

        let mut w = Work::new();
        w.consumed[0] = nx;
        w.produced[0] = written;
        Ok(w)
    }
}

impl BlockFactory for RealF32Resamp {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: RealF32ResampParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(RealF32Resamp::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{RealF32Resamp, RealF32ResampParams};
    use crate::block::{Block, BlockIo, InBuf, InitCtx, InputPort, OutBuf, OutputPort, PortMeta};

    fn init_at(rate_hz: f64, out_rate_hz: f64) -> RealF32Resamp {
        let mut r = RealF32Resamp::new(RealF32ResampParams {
            output_rate_hz: out_rate_hz,
            stopband_db: 60.0,
        })
        .unwrap();
        let meta = [(
            "in",
            PortMeta {
                sample_rate_hz: rate_hz,
                center_freq_hz: 0.0,
            },
        )];
        let mut ctx = InitCtx {
            input_meta: &meta,
            output_meta: &[],
            frames_hint: 1024,
        };
        r.init(&mut ctx).unwrap();
        r
    }

    #[test]
    fn construct_rejects_invalid_params() {
        assert!(RealF32Resamp::new(RealF32ResampParams {
            output_rate_hz: 0.0,
            stopband_db: 60.0,
        })
        .is_err());
        assert!(RealF32Resamp::new(RealF32ResampParams {
            output_rate_hz: 48_000.0,
            stopband_db: 0.0,
        })
        .is_err());
    }

    #[test]
    fn init_requires_known_input_rate() {
        let mut r = RealF32Resamp::new(RealF32ResampParams::default()).unwrap();
        let mut ctx = InitCtx {
            input_meta: &[],
            output_meta: &[],
            frames_hint: 1024,
        };
        assert!(r.init(&mut ctx).is_err());
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn downsamples_200k_to_48k_at_expected_ratio() {
        // 200 kS/s → 48 kS/s is the wbfm-at-2MSPS case. Drive the
        // block across a few calls until all 10 000 input samples are
        // consumed, summing produced counts. Expect ~2400 out (ratio
        // 0.24) ± ~2% for startup transient. The `nx_cap` inside
        // `process` clamps a single call's input to what the output
        // slice can safely hold, so a single `run` might only consume
        // part of the input — mirror the scheduler by looping.
        let mut r = init_at(200_000.0, 48_000.0);
        let n_in = 10_000;
        let input = vec![0.5_f32; n_in];
        let mut consumed_total = 0;
        let mut produced_total = 0;
        let mut guard = 0;
        while consumed_total < input.len() && guard < 32 {
            guard += 1;
            let mut out = vec![0.0_f32; 4096];
            let mut inputs = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&input[consumed_total..]),
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
            let w = r.process(&mut io).unwrap();
            if w.consumed[0] == 0 && w.produced[0] == 0 {
                break;
            }
            consumed_total += w.consumed[0];
            produced_total += w.produced[0];
        }
        let expected = (n_in as f64 * 0.24).round() as usize;
        let diff = (produced_total as isize - expected as isize).unsigned_abs();
        assert!(
            diff * 50 <= expected,
            "expected ~{expected}, got {produced_total}"
        );
        assert_eq!(consumed_total, n_in);
    }
}
