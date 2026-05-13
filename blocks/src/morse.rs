//! Morse / CW decoder — wraps multimon-ng's `demod_morse`.
//!
//! Goertzel-based tone detector + dit/dah classifier. Eats 22050 Hz
//! mono audio carrying a CW tone and emits decoded text. Pair with
//! the SsbDemod CW chain (USB or LSB + BFO) so the detector sees a
//! stable audio tone — feeding straight Fm/Am-demodulated audio
//! works too but won't decode classic on-off-keyed HF CW the same
//! way.
//!
//! ### Sample-rate contract
//!
//! 22050 Hz, same as the rest of the multimon family. `RealF32Resamp`
//! upstream is the standard glue.
//!
//! ### Tracing target
//!
//! `decoder::cw` — distinct from packet/eas/pager so a busy CW band
//! doesn't drown the others. Multimon emits one log line per
//! decoded character group; the tracing layer collects them like any
//! other category.
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

/// Required input sample rate. Hard contract from `demod_morse`.
pub const MORSE_INPUT_RATE_HZ: u32 = 22_050;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct MorseDemodParams {
    pub sample_rate_hz: f32,
}

impl Default for MorseDemodParams {
    fn default() -> Self {
        Self {
            #[allow(clippy::cast_precision_loss)]
            sample_rate_hz: MORSE_INPUT_RATE_HZ as f32,
        }
    }
}

pub struct MorseDemod {
    decoder: MultimonDemod,
    warned_off_rate: bool,
    input_rate_hz: f64,
}

impl MorseDemod {
    pub fn new(params: MorseDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "morse_demod: sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        Ok(Self {
            decoder: MultimonDemod::new(Decoder::MorseCw),
            warned_off_rate: false,
            input_rate_hz: f64::from(params.sample_rate_hz),
        })
    }

    fn check_rate(&mut self, rate: f64) {
        if !self.warned_off_rate && (rate - f64::from(MORSE_INPUT_RATE_HZ)).abs() > 1.0 {
            tracing::warn!(
                target: "decoder::cw",
                "input rate {rate} Hz != required {MORSE_INPUT_RATE_HZ} Hz; \
                 add a RealF32Resamp upstream"
            );
            self.warned_off_rate = true;
        }
        self.input_rate_hz = rate;
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for MorseDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "MorseDemod",
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
                ai_notes: "Locked at 22.05 kHz post-demod.",
            }],
            ai_notes: "CW / Morse decoder. Auto-detects element timing (dots/dashes) from envelope on/off; tolerates ~5–40 wpm. Use after `SsbDemod` on HF ham bands. Output: `tail decoder --category cw`.",
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
            tracing::info!(target: "decoder::cw", "{line}");
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;
        Ok(w)
    }
}

impl BlockFactory for MorseDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: MorseDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(MorseDemod::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{MorseDemod, MorseDemodParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, PortMeta};

    fn run(block: &mut MorseDemod, samples: &[f32]) {
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
        assert!(MorseDemod::new(MorseDemodParams {
            sample_rate_hz: 0.0,
        })
        .is_err());
    }

    #[test]
    fn silence_does_not_panic_or_emit() {
        let mut b = MorseDemod::new(MorseDemodParams::default()).unwrap();
        run(&mut b, &vec![0.0_f32; 22_050]);
    }
}
