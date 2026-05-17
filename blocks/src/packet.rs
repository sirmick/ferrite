//! Packet radio decoder — wraps multimon-ng's AFSK and FSK demods.
//!
//! Five decoders running in parallel on one 22050 Hz audio stream:
//! AFSK1200 (the APRS workhorse), three AFSK2400 timing variants
//! (MFJ-2400, TCM3105, PSKDET), and FSK9600 (G3RUH — used on amateur
//! satellites and high-rate APRS). All emit AX.25 frames after their
//! L1 demod; multimon turns those into one-line text per frame and
//! the wrapper hands them up as `String`s.
//!
//! ### Sample-rate contract
//!
//! Same as [`crate::pager`] — input must be 22050 Hz ± 1 Hz, with a
//! `RealF32Resamp` upstream of the FmDemod output. Off-rate audio
//! decodes as garbage; the block warns once and keeps running.
//!
//! ### Tracing target
//!
//! Decoded frames stream under `decoder::packet`. Lines come straight
//! from multimon's own formatter (`AFSK1200: ...`, `FSK9600: ...`)
//! so the leading tag identifies which physical layer caught the
//! frame. The Logs panel can mute the category like any other.
//!
//! ### Placement
//!
//! `Placement::NativeOnly` — multimon-ng is C; same constraints as
//! `PagerDemod`.

use anyhow::{bail, Result};
use ferrite_multimon_ng::{set_aprs_mode, Decoder, MultimonDemod};
use serde::Deserialize;

use crate::aprs::{parse_aprs, split_tnc2, AprsSpot};
use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutBuf, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Required input sample rate. Hard contract from multimon-ng's
/// AFSK / FSK decoders.
pub const PACKET_INPUT_RATE_HZ: u32 = 22_050;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct PacketDemodParams {
    /// Construction-time hint for the input rate (Hz). The block
    /// reads the live rate at `init()` / `update_rates()` and warns
    /// if it doesn't match `PACKET_INPUT_RATE_HZ`.
    pub sample_rate_hz: f32,
    /// APRS display mode (multimon `aprs_mode`, process-global, set at
    /// construction). When `true` (default — this block is
    /// APRS-primary and the `events` port / `ui:aprs` view need it)
    /// AX.25 UI/0xF0 frames print in compact TNC2 form
    /// `APRS: SRC>DEST,path:info` that `aprs.rs` parses. The trade-off
    /// is that multimon then prints *only* UI/0xF0 frames —
    /// connected-mode / BBS packet is suppressed. Set `false` for a
    /// general AX.25 monitor (verbose multi-line form, all frame
    /// types) at the cost of structured APRS events.
    pub aprs_mode: bool,
}

impl Default for PacketDemodParams {
    fn default() -> Self {
        Self {
            #[allow(clippy::cast_precision_loss)]
            sample_rate_hz: PACKET_INPUT_RATE_HZ as f32,
            aprs_mode: true,
        }
    }
}

pub struct PacketDemod {
    /// Five multimon decoders running in parallel on the same audio:
    /// AFSK1200, three AFSK2400 timing variants, and FSK9600. The
    /// AFSK2400 variants disagree on sync timing — running all three
    /// is the standard multimon-ng practice when the on-air timing
    /// isn't known up-front. ~1–2 % CPU each at 22 kHz.
    decoders: Vec<MultimonDemod>,
    warned_off_rate: bool,
    input_rate_hz: f64,
    /// UTF-8 newline-delimited JSON APRS spots queued for the `events`
    /// port (same transport as RDS / FT8 / ADS-B).
    events_out: Vec<u8>,
}

impl PacketDemod {
    pub fn new(params: PacketDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "packet_demod: sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        // TNC2 APRS display so the `events` parser sees
        // `APRS: SRC>DEST,path:info`. Process-global, AX.25-only —
        // doesn't disturb the POCSAG/FLEX/DTMF multimon blocks. Off
        // for a general AX.25 monitor (keeps connected-mode frames).
        set_aprs_mode(params.aprs_mode);
        Ok(Self {
            decoders: vec![
                MultimonDemod::new(Decoder::Afsk1200),
                MultimonDemod::new(Decoder::Afsk2400),
                MultimonDemod::new(Decoder::Afsk2400_2),
                MultimonDemod::new(Decoder::Afsk2400_3),
                MultimonDemod::new(Decoder::Fsk9600),
            ],
            warned_off_rate: false,
            input_rate_hz: f64::from(params.sample_rate_hz),
            events_out: Vec::new(),
        })
    }

    fn check_rate(&mut self, rate: f64) {
        if !self.warned_off_rate && (rate - f64::from(PACKET_INPUT_RATE_HZ)).abs() > 1.0 {
            tracing::warn!(
                target: "decoder::packet",
                "input rate {rate} Hz != required {PACKET_INPUT_RATE_HZ} Hz; \
                 add a RealF32Resamp upstream"
            );
            self.warned_off_rate = true;
        }
        self.input_rate_hz = rate;
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for PacketDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "PacketDemod",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            // Frames still stream as text via the `decoder::packet`
            // tracing target (now TNC2 form). The `events` port adds
            // parsed APRS records for the `ui:aprs` advanced view.
            outputs: &[PortSpec {
                name: "events",
                port_type: PortType::Events,
            }],
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
            ai_notes: "AX.25 packet decoder (1200 bps AFSK). Used by the APRS preset on 144.39 MHz (NA) / 144.80 MHz (EU); also receives raw ham packet on other VHF/UHF channels. Output: `tail decoder --category packet`.",
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

        // Same per-decoder push-then-drain pattern as PagerDemod —
        // multimon's per-thread output buffer would otherwise mix
        // lines from different decoders. All packet variants share
        // one tracing target since the leading "AFSK1200:"/"FSK9600:"
        // prefix already identifies the physical layer.
        for d in &mut self.decoders {
            d.push(src);
            for line in d.drain_lines() {
                // Text path unchanged (TNC2 form now that aprs_mode is
                // on). Additionally parse APRS frames for the events
                // port — non-APRS lines (FSK9600 raw, etc.) just don't
                // match split_tnc2 and are text-only, as before.
                if let Some((call, path, info)) = split_tnc2(&line) {
                    let p = parse_aprs(info);
                    AprsSpot {
                        call,
                        path,
                        kind: p.kind,
                        pos: p.pos,
                        name: p.name,
                        text: p.text,
                        raw: info,
                    }
                    .write_json(&mut self.events_out);
                }
                tracing::info!(target: "decoder::packet", "{line}");
            }
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;

        // Drain queued JSON into the events port. APRS frame rate is
        // low (a busy channel is a few per second, ~150 B each); any
        // remainder rides to the next tick.
        if !self.events_out.is_empty() {
            for port in io.outputs.iter_mut() {
                if port.name == "events" {
                    if let OutBuf::Events(dst) = &mut port.buf {
                        let take = self.events_out.len().min(dst.len());
                        if take > 0 {
                            dst[..take].copy_from_slice(&self.events_out[..take]);
                            self.events_out.drain(..take);
                            w.produced[0] = take;
                        }
                    }
                }
            }
        }

        Ok(w)
    }
}

impl BlockFactory for PacketDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: PacketDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(PacketDemod::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{PacketDemod, PacketDemodParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, PortMeta};

    fn run(block: &mut PacketDemod, samples: &[f32]) {
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
        assert!(PacketDemod::new(PacketDemodParams {
            sample_rate_hz: 0.0,
            ..PacketDemodParams::default()
        })
        .is_err());
    }

    #[test]
    fn silence_does_not_panic_or_emit() {
        let mut b = PacketDemod::new(PacketDemodParams::default()).unwrap();
        run(&mut b, &vec![0.0_f32; 22_050]);
    }
}
