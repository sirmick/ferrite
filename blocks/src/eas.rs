//! EAS (Emergency Alert System / SAME) decoder — wraps multimon-ng's
//! `demod_eas`.
//!
//! SAME data bursts ride at the head of US/Canadian emergency
//! announcements: NOAA Weather Radio (162.4–162.55 MHz NBFM), TV/
//! radio EAS tests, AM/FM RBDS-EAS. Each burst carries a header
//! describing the alert kind, the affected county codes (FIPS), and
//! a duration. Decoded headers stream as one log line per burst.
//!
//! ### Sample-rate contract
//!
//! 22050 Hz NBFM-demodulated audio. Same plumbing as
//! [`crate::pager`] / [`crate::packet`]: a `RealF32Resamp` upstream
//! brings the FmDemod output to multimon's native rate.
//!
//! ### Tracing target
//!
//! `decoder::eas` — alerts are infrequent (a quiet NWR site gets a
//! header maybe once an hour for the routine weekly test, plus
//! whatever real activations arrive). Worth its own log category so
//! the user can leave it on while muting noisier decoders.
//!
//! ### Placement
//!
//! `Placement::NativeOnly`.

use anyhow::{bail, Result};
use ferrite_multimon_ng::{Decoder, MultimonDemod};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

/// Required input sample rate. Hard contract from `demod_eas`.
pub const EAS_INPUT_RATE_HZ: u32 = 22_050;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct EasDemodParams {
    pub sample_rate_hz: f32,
}

impl Default for EasDemodParams {
    fn default() -> Self {
        Self {
            #[allow(clippy::cast_precision_loss)]
            sample_rate_hz: EAS_INPUT_RATE_HZ as f32,
        }
    }
}

pub struct EasDemod {
    decoder: MultimonDemod,
    warned_off_rate: bool,
    input_rate_hz: f64,
}

impl EasDemod {
    pub fn new(params: EasDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "eas_demod: sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        Ok(Self {
            decoder: MultimonDemod::new(Decoder::Eas),
            warned_off_rate: false,
            input_rate_hz: f64::from(params.sample_rate_hz),
        })
    }

    fn check_rate(&mut self, rate: f64) {
        if !self.warned_off_rate && (rate - f64::from(EAS_INPUT_RATE_HZ)).abs() > 1.0 {
            tracing::warn!(
                target: "decoder::eas",
                "input rate {rate} Hz != required {EAS_INPUT_RATE_HZ} Hz; \
                 add a RealF32Resamp upstream"
            );
            self.warned_off_rate = true;
        }
        self.input_rate_hz = rate;
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for EasDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "EasDemod",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[],
            params: &[ParamSpec {
                key: "sample_rate_hz",
                label: "Input sample rate",
                kind: ParamKind::EnumNumeric {
                    values: &[22_050.0],
                    default: 22_050.0,
                    unit: "Hz",
                },
                reconfig_scope: ReconfigureScope::SourceRestart,
                ai_notes: "Locked at 22.05 kHz — the standard SAME AFSK rate.",
            }],
            ai_notes: "EAS / SAME decoder for Emergency Alert System headers on NOAA Weather Radio (162.4–162.55 MHz) and broadcast TV/radio. Riding atop FM/NBFM audio; load the `nwr` preset for typical weather use. Output: `tail decoder --category eas`.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 {
                self.check_rate(rate);
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.warned_off_rate = false;
                self.check_rate(rate);
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

        let consumed = src.len();
        if consumed == 0 {
            return Ok(Work::new());
        }

        self.decoder.push(src);
        for line in self.decoder.drain_lines() {
            tracing::info!(target: "decoder::eas", "{line}");
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;
        Ok(w)
    }
}

impl BlockFactory for EasDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: EasDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(EasDemod::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{EasDemod, EasDemodParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, PortMeta};

    fn run(block: &mut EasDemod, samples: &[f32]) {
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(samples),
        }];
        let mut outputs: [crate::block::OutputPort; 0] = [];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let _ = block.process(&mut io).unwrap();
    }

    #[test]
    fn rejects_bad_params() {
        assert!(EasDemod::new(EasDemodParams {
            sample_rate_hz: 0.0,
        })
        .is_err());
    }

    #[test]
    fn silence_does_not_panic_or_emit() {
        let mut b = EasDemod::new(EasDemodParams::default()).unwrap();
        run(&mut b, &vec![0.0_f32; 22_050]);
    }
}
