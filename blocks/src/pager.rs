//! Pager (POCSAG + FLEX) decoder — wraps `multimon-ng`'s
//! `demod_poc12`, `demod_flex`, and `demod_flex_next`.
//!
//! Takes NBFM-demodulated audio at exactly 22050 Hz (POCSAG1200's
//! native rate inside multimon-ng — FLEX shares the same plumbing)
//! and pumps it through the C decoder. Decoded lines route to the
//! project's tracing log split per protocol family: POCSAG goes to
//! `decoder::pocsag`, FLEX goes to `decoder::flex` so the log panel
//! can mute one without losing the other.
//!
//! ### Sample-rate contract
//!
//! Input must be 22050 Hz ± 1 Hz. multimon's bit-timing is hard-
//! coded against the rate; off-rate audio decodes as garbage. The
//! preset is expected to put a `RealF32Resamp` directly upstream of
//! this block to bring the FmDemod output to 22050. If the live
//! rate differs from the contract, the block runs but emits a one-
//! shot warning (no decode work) — easier to diagnose than silent
//! garbage.
//!
//! ### Placement
//!
//! `Placement::NativeOnly` — multimon-ng is a C library and the
//! tracing infra it logs through lives on the server side anyway.
//! Browser-placed decoders would need a different log emit path.

use anyhow::{bail, Result};
use ferrite_multimon_ng::{pocsag as pocsag_cfg, Decoder, MultimonDemod};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

/// Required input sample rate. Hard contract from multimon-ng's
/// `demod_poc12`.
pub const POCSAG_INPUT_RATE_HZ: u32 = 22_050;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct PagerDemodParams {
    /// Construction-time hint for the input rate (Hz). The block
    /// reads the live rate at `init()` / `update_rates()` and warns
    /// if it doesn't match `POCSAG_INPUT_RATE_HZ`.
    pub sample_rate_hz: f32,
    /// Surface partial-CRC decodes too — useful while bringing up a
    /// new install ("the decoder did see POCSAG-shaped activity, the
    /// message just had too many errors to repair"). Off for normal
    /// operation since busy carriers spam the log with garbled
    /// fragments. Note: this is a vendor *global*; toggling it on
    /// any PagerDemod instance affects all of them in-process.
    pub show_partial: bool,
}

impl Default for PagerDemodParams {
    fn default() -> Self {
        Self {
            #[allow(clippy::cast_precision_loss)]
            sample_rate_hz: POCSAG_INPUT_RATE_HZ as f32,
            show_partial: true,
        }
    }
}

pub struct PagerDemod {
    /// Five multimon decoders running in parallel on the same
    /// 22050 Hz audio stream — POCSAG512, 1200, 2400, FLEX, FLEX_NEXT.
    /// Each tries to lock its own bit timing / sync; whichever
    /// matches the carrier produces messages, the others sit
    /// silent. Cheap enough (a few % CPU per rate at 22 kHz) to
    /// always run all of them. FLEX largely replaced POCSAG on US
    /// carrier networks, so any "paging band" preset wants both.
    decoders: Vec<MultimonDemod>,
    /// Tracks whether we've already logged the off-rate warning, so
    /// it doesn't spam every tick.
    warned_off_rate: bool,
    input_rate_hz: f64,
}

impl PagerDemod {
    pub fn new(params: PagerDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "pager_demod: sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        // Apply the vendor-global config once at construction. The
        // `show_partial` knob is shared across every PagerDemod
        // instance in the process — last writer wins; a single block
        // is the common case so this is effectively per-block.
        pocsag_cfg::set_show_partial_decodes(params.show_partial);
        Ok(Self {
            decoders: vec![
                MultimonDemod::new(Decoder::Pocsag512),
                MultimonDemod::new(Decoder::Pocsag1200),
                MultimonDemod::new(Decoder::Pocsag2400),
                MultimonDemod::new(Decoder::Flex),
                MultimonDemod::new(Decoder::FlexNext),
            ],
            warned_off_rate: false,
            input_rate_hz: f64::from(params.sample_rate_hz),
        })
    }

    fn check_rate(&mut self, rate: f64) {
        if !self.warned_off_rate && (rate - f64::from(POCSAG_INPUT_RATE_HZ)).abs() > 1.0 {
            tracing::warn!(
                target: "decoder::pocsag",
                "input rate {rate} Hz != required {POCSAG_INPUT_RATE_HZ} Hz; \
                 add a RealF32Resamp upstream"
            );
            self.warned_off_rate = true;
        }
        self.input_rate_hz = rate;
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for PagerDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "PagerDemod",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[],
            params: &[
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Input sample rate",
                    kind: ParamKind::EnumNumeric {
                        values: &[22_050.0],
                        default: 22_050.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Locked at 22.05 kHz — the rate the POCSAG/FLEX bit detectors expect.",
                },
                ParamSpec {
                    key: "show_partial",
                    label: "Show partial decodes",
                    kind: ParamKind::Toggle { default: true },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Surface decodes that pass framing but fail BCH error-check. Useful for spotting traffic on encrypted channels; turn off for a clean log.",
                },
            ],
            ai_notes: "POCSAG + FLEX paging decoder. Tune to commercial paging channels (US: 929–932 MHz, 152–162 MHz; UK: 153 MHz; varies regionally) on NBFM. Output: `tail decoder --category pocsag` and `--category flex`.",
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
                // Reset the warning so a re-negotiated rate is
                // re-evaluated; if we drifted off-rate, the user gets
                // a fresh warning instead of a stale one.
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

        // Push samples into every decoder sequentially. The shim
        // drain buffer is per-thread; pushing-then-draining each
        // decoder serialises access so two rates can't tangle their
        // output bytes together. multimon's per-decoder process()
        // is also serialised by the runtime tick loop, so there's
        // no concurrency to worry about here.
        //
        // Tracing target is split per protocol family so the Logs
        // panel category dropdown can mute POCSAG and FLEX
        // independently. tracing requires `target:` to be a string
        // literal; we branch on the family rather than computing a
        // runtime string.
        for d in &mut self.decoders {
            d.push(src);
            let lines = d.drain_lines();
            if lines.is_empty() {
                continue;
            }
            match d.kind() {
                Decoder::Pocsag512 | Decoder::Pocsag1200 | Decoder::Pocsag2400 => {
                    for line in lines {
                        tracing::info!(target: "decoder::pocsag", "{line}");
                    }
                }
                Decoder::Flex | Decoder::FlexNext => {
                    for line in lines {
                        tracing::info!(target: "decoder::flex", "{line}");
                    }
                }
                // PagerDemod only ever constructs the paging variants above
                // (see `Self::new`); this arm exists so the wrapper can grow
                // new `Decoder` kinds (packet/EAS/morse/dtmf) without
                // breaking pager.rs's match exhaustiveness.
                other => {
                    debug_assert!(false, "pager: unexpected decoder kind {other:?}");
                    for line in lines {
                        tracing::info!(target: "decoder::pocsag", "{line}");
                    }
                }
            }
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;
        Ok(w)
    }
}

impl BlockFactory for PagerDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: PagerDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(PagerDemod::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{PagerDemod, PagerDemodParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, PortMeta};

    fn run(block: &mut PagerDemod, samples: &[f32]) {
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
        assert!(PagerDemod::new(PagerDemodParams {
            sample_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn silence_does_not_panic_or_emit() {
        // 1 s of silence at 22050 Hz: pumps cleanly through the C
        // decoder, no decoded messages. The smoke-test that the
        // wrapper, BlockIo plumbing, and tracing emit path all hang
        // together.
        let mut b = PagerDemod::new(PagerDemodParams::default()).unwrap();
        run(&mut b, &vec![0.0_f32; 22_050]);
    }
}
