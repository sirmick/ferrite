//! ISM-band device decoder, wraps the vendored rtl_433.
//!
//! ~320 device decoders covering 315/433.92/868/915 MHz ISM sensors:
//! weather stations (Acurite, La Crosse, Oregon Scientific, Ambient
//! Weather, Fine Offset), TPMS, garage-door remotes, smoke detectors,
//! Honeywell security sensors, AMR water/electric meters (ERT, SCM),
//! soil-moisture probes, and a long tail of niche devices. See
//! upstream's `SUPPORTED_PROTOCOLS.md` for the full catalogue.
//!
//! ### Sample-rate contract
//!
//! 250 kS/s is upstream's default and matches the bit timing every
//! shipped decoder was tuned against. 1 MS/s is accepted and unlocks a
//! handful of high-baud decoders at ~4× the CPU cost; anything else
//! reads garbage at the bit slicer. The block warns once at `init()`
//! if the upstream rate is off; the user fixes the channelizer or
//! source.
//!
//! ### Tracing target
//!
//! Decoded device frames emit under `decoder::rtl_433` as one JSON
//! object per event. Frame schema is per-device (consult upstream
//! docs); every record has at minimum a `model` field.
//!
//! ### Decoder set
//!
//! [`Rtl433Demod::new`] takes a `decoder_set` selector mirroring
//! upstream's `r_device.disabled` field:
//!
//! - `"default"` — ~220 stable decoders (upstream's `-R`-less default)
//! - `"extended"` — adds experimental / niche / noisy decoders
//! - `"all"` — adds broken / hidden decoders (parity-only)
//!
//! Default is `"default"` for the lowest false-positive rate.
//!
//! ### Placement
//!
//! `Placement::Either`. The native build links against system libm; the
//! WASM build threads through `libc-stubs/include` + wasi-libc and
//! produces a ~4 MB module. Presets default to `placement: "node"` for
//! the CPU + bundle-size win; users who want the decoder browser-side
//! flip the placement on the block.
//!
//! ### Audio
//!
//! No audio output. Presets that want an audible "is something
//! chirping?" track tee the IQ to a parallel
//! `FmDemod → Resample → AudioNrMono → AudioSink` chain gated on
//! `when: { audio: true }`. See `pager.json` for the pattern.

use anyhow::{bail, Result};
use ferrite_rtl_433::{DecoderSet, Rtl433Demod as Decoder};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

/// rtl_433's native sample rate. The block warns if the upstream rate
/// differs by more than 1 Hz.
pub const RTL433_INPUT_RATE_HZ: u32 = 250_000;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Rtl433DemodParams {
    /// Construction-time hint for the input rate (Hz). The block reads
    /// the live rate at `init()` / `update_rates()` and warns if it
    /// doesn't match.
    pub sample_rate_hz: f32,
    /// Which subset of upstream's 320 decoders to register. See the
    /// module docs.
    pub decoder_set: String,
}

impl Default for Rtl433DemodParams {
    fn default() -> Self {
        Self {
            #[allow(clippy::cast_precision_loss)]
            sample_rate_hz: RTL433_INPUT_RATE_HZ as f32,
            decoder_set: "default".to_string(),
        }
    }
}

fn parse_decoder_set(s: &str) -> Result<DecoderSet> {
    match s {
        "default" => Ok(DecoderSet::Default),
        "extended" => Ok(DecoderSet::Extended),
        "all" => Ok(DecoderSet::All),
        other => bail!(
            "rtl_433_demod: decoder_set must be one of \
             default / extended / all (got {other:?})"
        ),
    }
}

pub struct Rtl433Demod {
    dec: Decoder,
    warned_off_rate: bool,
    input_rate_hz: f64,
    construct_rate_hz: u32,
}

impl Rtl433Demod {
    pub fn new(params: Rtl433DemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "rtl_433_demod: sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        let decoder_set = parse_decoder_set(&params.decoder_set)?;
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rate = params.sample_rate_hz as u32;
        let dec = Decoder::new(rate, decoder_set)
            .ok_or_else(|| anyhow::anyhow!("rtl_433_demod: shim init failed (allocation?)"))?;
        Ok(Self {
            dec,
            warned_off_rate: false,
            input_rate_hz: f64::from(params.sample_rate_hz),
            construct_rate_hz: rate,
        })
    }

    fn check_rate(&mut self, rate: f64) {
        if !self.warned_off_rate && (rate - f64::from(self.construct_rate_hz)).abs() > 1.0 {
            tracing::warn!(
                target: "decoder::rtl_433",
                "input rate {rate} Hz != construct rate {} Hz; \
                 add a Channelizer with output_rate_hz={} upstream",
                self.construct_rate_hz,
                self.construct_rate_hz
            );
            self.warned_off_rate = true;
        }
        self.input_rate_hz = rate;
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for Rtl433Demod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "Rtl433Demod",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[],
            params: &[
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Input sample rate",
                    kind: ParamKind::EnumNumeric {
                        values: &[250_000.0, 1_000_000.0],
                        default: 250_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Locked at 250 kHz (upstream default — fits every shipped decoder) or 1 MHz (~4× CPU, unlocks a few high-baud devices). Anything else reads as garbage at the bit slicer.",
                },
                ParamSpec {
                    key: "decoder_set",
                    label: "Decoder set",
                    kind: ParamKind::EnumString {
                        values: &["default", "extended", "all"],
                        default: "default",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "default = ~220 stable decoders (upstream's `-R`-less default, lowest false-positive rate). extended adds experimental/niche/noisy ones. all adds broken/hidden too — parity-only, not for live use.",
                },
            ],
            ai_notes: "ISM-band device decoder. Tune to 433.92 MHz US/EU consumer/weather/TPMS, or 915 MHz US smart-meter / ERT / sensor band. ~320 protocols covering weather stations (Acurite, LaCrosse, Ambient Weather), TPMS, garage doors, AMR meters, soil moisture, etc. Output: `tail decoder --category rtl_433` — each frame is one JSON record with at minimum a `model` field. Plentiful in any US suburban RX environment; first decode usually arrives within 60 s of an antenna pointing skyward.",
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
            .and_then(InputPort::as_iq_f32)
        else {
            return Ok(Work::new());
        };

        let consumed = src.len();
        if consumed == 0 {
            return Ok(Work::new());
        }

        self.dec.push_iq(src);
        while let Some(event) = self.dec.drain_event() {
            tracing::info!(target: "decoder::rtl_433", "{event}");
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;
        Ok(w)
    }
}

impl BlockFactory for Rtl433Demod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: Rtl433DemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(Rtl433Demod::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_decoder_set, Rtl433Demod, Rtl433DemodParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, PortMeta};
    use ferrite_rtl_433::DecoderSet;
    use num_complex::Complex;

    #[test]
    fn rejects_bad_rate() {
        let bad = Rtl433DemodParams {
            sample_rate_hz: 0.0,
            decoder_set: "default".into(),
        };
        assert!(Rtl433Demod::new(bad).is_err());
    }

    #[test]
    fn rejects_bad_decoder_set() {
        let bad = Rtl433DemodParams {
            sample_rate_hz: 250_000.0,
            decoder_set: "everything".into(),
        };
        assert!(Rtl433Demod::new(bad).is_err());
    }

    #[test]
    fn decoder_set_parses_all_variants() {
        assert_eq!(parse_decoder_set("default").unwrap(), DecoderSet::Default);
        assert_eq!(parse_decoder_set("extended").unwrap(), DecoderSet::Extended);
        assert_eq!(parse_decoder_set("all").unwrap(), DecoderSet::All);
    }

    #[test]
    fn silence_emits_no_events() {
        // ~0.1 s at 250 kHz — pulse detect should see no packages.
        let mut block = Rtl433Demod::new(Rtl433DemodParams::default()).unwrap();
        let samples = vec![Complex::<f32>::new(0.0, 0.0); 25_000];

        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&samples),
        }];
        let mut outputs: [crate::block::OutputPort; 0] = [];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let work = block.process(&mut io).unwrap();
        assert_eq!(work.consumed[0], samples.len());
    }
}
