//! Marine AIS (Automatic Identification System) decoder, wraps the
//! vendored aisdecoder (from `rtl-ais`).
//!
//! ### Sample-rate contract
//!
//! Hard 48 kHz on each leg. aisdecoder's GMSK clock-recovery PLL is
//! sized for exactly 5 samples per bit at 9600 baud (= 48000 Hz);
//! off-rate audio decodes as garbage. The block warns once at init if
//! the upstream rate disagrees and keeps running so the warning is
//! visible in the log.
//!
//! ### Two channels
//!
//! AIS uses two interleaved channels 50 kHz apart — Channel A at
//! 161.975 MHz and Channel B at 162.025 MHz. Most vessels alternate
//! between them; running both decoders in parallel typically captures
//! 60–70 % more frames than either alone. The block has two input
//! ports `ch_a` and `ch_b`, both `real_f32` at 48 kHz; the wrapping
//! preset puts a separate Channelizer + FmDemod + Resamp chain on each
//! AIS frequency.
//!
//! ### Tracing target
//!
//! Decoded sentences stream under `decoder::ais` as raw NMEA AIVDM
//! lines, one tracing event per `!AIVDM,...,*hh` sentence. The lines
//! are the same shape every NMEA-aware downstream tool (gpsd, OpenCPN,
//! Marine Traffic uploaders) consumes.
//!
//! ### Placement
//!
//! `Placement::NativeOnly` — aisdecoder is C, same constraint as the
//! multimon and dump1090 blocks.

use anyhow::{bail, Result};
use ferrite_rtl_ais::{RtlAis, AIS_INPUT_RATE_HZ};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct AisDemodParams {
    /// Construction-time hint for the input rate (Hz). The block
    /// reads the live rate at `init()` and warns if it doesn't match
    /// [`AIS_INPUT_RATE_HZ`] on either leg.
    pub sample_rate_hz: f32,
}

impl Default for AisDemodParams {
    fn default() -> Self {
        Self {
            #[allow(clippy::cast_precision_loss)]
            sample_rate_hz: AIS_INPUT_RATE_HZ as f32,
        }
    }
}

pub struct AisDemod {
    dec: RtlAis,
    warned_off_rate: bool,
    input_rate_hz: f64,
}

impl AisDemod {
    pub fn new(params: AisDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "ais_demod: sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        Ok(Self {
            dec: RtlAis::new(),
            warned_off_rate: false,
            input_rate_hz: f64::from(params.sample_rate_hz),
        })
    }

    fn check_rate(&mut self, rate: f64) {
        // 5 Hz tolerance — RealF32Resamp's MsResamp lands the target
        // rate to fractional accuracy, well under that. Anything off
        // by more than a few Hz means the chain isn't actually at
        // 48 kHz and the GMSK PLL will drift.
        if !self.warned_off_rate && (rate - f64::from(AIS_INPUT_RATE_HZ)).abs() > 5.0 {
            tracing::warn!(
                target: "decoder::ais",
                "input rate {rate} Hz != required {AIS_INPUT_RATE_HZ} Hz; \
                 add a RealF32Resamp upstream targeting 48 kHz"
            );
            self.warned_off_rate = true;
        }
        self.input_rate_hz = rate;
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for AisDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "AisDemod",
            placement: Placement::NativeOnly,
            inputs: &[
                PortSpec {
                    name: "ch_a",
                    port_type: PortType::RealF32,
                },
                PortSpec {
                    name: "ch_b",
                    port_type: PortType::RealF32,
                },
            ],
            outputs: &[],
            params: &[ParamSpec {
                key: "sample_rate_hz",
                label: "Input sample rate",
                kind: ParamKind::EnumNumeric {
                    values: &[48_000.0],
                    default: 48_000.0,
                    unit: "Hz",
                },
                reconfig_scope: ReconfigureScope::SourceRestart,
                ai_notes: "Locked at 48 kHz — the post-channelizer audio rate AIS GMSK detection expects.",
            }],
            ai_notes: "AIS ship-tracking decoder (GMSK, 9600 bps). Two channels: 161.975 MHz and 162.025 MHz. Load the `ais` preset, watch `tail decoder --category ais` for MMSI / position / course. Best near coastlines.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        // Both legs must arrive at the same rate. If only one is
        // declared we use that; if both, prefer ch_a's value (the
        // resampler upstream of each ought to lock both to 48 kHz
        // anyway).
        if let Some(rate) = ctx.input_rate("ch_a").or_else(|| ctx.input_rate("ch_b")) {
            if rate > 0.0 {
                self.check_rate(rate);
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("ch_a").or_else(|| ctx.input_rate("ch_b")) {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.warned_off_rate = false;
                self.check_rate(rate);
            }
        }
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let ch_a = io
            .inputs
            .iter()
            .find(|p| p.name == "ch_a")
            .and_then(InputPort::as_real_f32);
        let ch_b = io
            .inputs
            .iter()
            .find(|p| p.name == "ch_b")
            .and_then(InputPort::as_real_f32);
        let (ch_a, ch_b) = match (ch_a, ch_b) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ok(Work::new()),
        };

        let n = ch_a.len().min(ch_b.len());
        if n == 0 {
            return Ok(Work::new());
        }

        self.dec.push_audio(&ch_a[..n], &ch_b[..n]);
        for line in self.dec.drain_lines() {
            tracing::info!(target: "decoder::ais", "{line}");
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.consumed[1] = n;
        Ok(w)
    }
}

impl BlockFactory for AisDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: AisDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(AisDemod::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{AisDemod, AisDemodParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, PortMeta};

    fn run(block: &mut AisDemod, ch_a: &[f32], ch_b: &[f32]) {
        let mut inputs = [
            InputPort {
                name: "ch_a",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(ch_a),
            },
            InputPort {
                name: "ch_b",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(ch_b),
            },
        ];
        let mut outputs: [crate::block::OutputPort; 0] = [];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let _ = block.process(&mut io).unwrap();
    }

    #[test]
    fn rejects_bad_params() {
        assert!(AisDemod::new(AisDemodParams {
            sample_rate_hz: 0.0,
        })
        .is_err());
    }

    #[test]
    fn silence_does_not_panic_or_emit() {
        let mut b = AisDemod::new(AisDemodParams::default()).unwrap();
        let zeros = vec![0.0_f32; 4_800];
        run(&mut b, &zeros, &zeros);
    }
}
