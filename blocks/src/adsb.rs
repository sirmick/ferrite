//! Mode S / ADS-B decoder at 1090 MHz, wraps the vendored dump1090.
//!
//! ### Sample-rate contract
//!
//! Hard 2 MS/s on the IQ input. dump1090's preamble correlator and bit
//! slicer are sized for that rate exactly; off-rate IQ decodes as
//! garbage. The block warns once at init if the upstream rate disagrees
//! and keeps running so the user can still see the warning in the log
//! pane and fix the source config.
//!
//! ### Tracing target
//!
//! Decoded frames stream under `decoder::adsb`. Lines are produced by
//! `displayModesMessage` inside dump1090.c (we redirected its `printf`
//! into the shim's per-thread capture); each Mode S frame yields a
//! cluster of lines (`*HEX...;`, `CRC: ok`, `DF 17: ADS-B message.`,
//! per-field details). The Logs panel category badge shows
//! `decoder::adsb` so they're greppable alongside `decoder::packet`,
//! `decoder::flex`, etc.
//!
//! ### Placement
//!
//! `Placement::NativeOnly` — dump1090 is C, same constraint as the
//! multimon-backed blocks.
//!
//! ### Scope
//!
//! v1 emits only the text stream. The C-side `Modes.aircrafts` linked
//! list is being maintained behind the scenes (we patched
//! `useModesMessage` to always call `interactiveReceiveData`), so a
//! follow-up that exposes a structured snapshot for a UI table is a
//! drop-in addition to this same block — bind a getter in the C shim,
//! call it from `process()`, and emit a separate JSON-event port.

use anyhow::{bail, Result};
use ferrite_dump1090::{Dump1090, ADSB_INPUT_RATE_HZ};
use serde::Deserialize;

use crate::aircraft_spot::AircraftSpot;
use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutBuf, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct AdsbDemodParams {
    /// Construction-time hint for the input rate (Hz). The block reads
    /// the live rate at `init()` / `update_rates()` and warns if it
    /// doesn't match [`ADSB_INPUT_RATE_HZ`].
    pub sample_rate_hz: f32,
}

impl Default for AdsbDemodParams {
    fn default() -> Self {
        Self {
            #[allow(clippy::cast_precision_loss)]
            sample_rate_hz: ADSB_INPUT_RATE_HZ as f32,
        }
    }
}

pub struct AdsbDemod {
    dec: Dump1090,
    warned_off_rate: bool,
    input_rate_hz: f64,
    /// UTF-8 newline-delimited JSON aircraft rows queued for the
    /// `events` port (same transport as RDS / FT8).
    events_out: Vec<u8>,
    /// Samples consumed since the last aircraft snapshot. The snapshot
    /// is a full list, not incremental, so we throttle it to ~1 Hz
    /// (every `ADSB_INPUT_RATE_HZ` samples) rather than flooding the
    /// events port every tick.
    samples_since_snap: u64,
}

impl AdsbDemod {
    pub fn new(params: AdsbDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "adsb_demod: sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        Ok(Self {
            dec: Dump1090::new(),
            warned_off_rate: false,
            input_rate_hz: f64::from(params.sample_rate_hz),
            events_out: Vec::new(),
            samples_since_snap: 0,
        })
    }

    fn check_rate(&mut self, rate: f64) {
        // Tolerance is wider than the ±1 Hz used in `packet.rs` because
        // SDR drivers report 2 MS/s with kHz-scale rounding (RTL-SDR
        // actually delivers 2,000,142 Hz at "2 MHz" requested), and
        // dump1090's correlator tolerates that. Anything off by more
        // than ~10 kHz starts losing decodes.
        if !self.warned_off_rate && (rate - f64::from(ADSB_INPUT_RATE_HZ)).abs() > 10_000.0 {
            tracing::warn!(
                target: "decoder::adsb",
                "input rate {rate} Hz differs from required {ADSB_INPUT_RATE_HZ} Hz \
                 by more than 10 kHz; configure the source for 2 MS/s"
            );
            self.warned_off_rate = true;
        }
        self.input_rate_hz = rate;
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for AdsbDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "AdsbDemod",
            placement: Placement::NativeOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            // Text frames still stream via the `decoder::adsb` tracing
            // target (unchanged). The `events` port additionally
            // streams a ~1 Hz structured aircraft snapshot for the
            // `ui:adsb` advanced view, over the same `PortType::Events`
            // transport RDS / FT8 use.
            outputs: &[PortSpec {
                name: "events",
                port_type: PortType::Events,
            }],
            params: &[ParamSpec {
                key: "sample_rate_hz",
                label: "Input sample rate",
                kind: ParamKind::EnumNumeric {
                    values: &[2_000_000.0],
                    default: 2_000_000.0,
                    unit: "Hz",
                },
                reconfig_scope: ReconfigureScope::SourceRestart,
                ai_notes: "Locked at 2 MS/s — the rate the Mode-S preamble + 1 µs PPM bit detector expects.",
            }],
            ai_notes: "ADS-B / Mode-S decoder. Tune to 1090 MHz, load the `adsb` preset, watch `tail decoder --category adsb` for aircraft positions + identifiers. Requires line-of-sight to airliners and a VHF/UHF whip antenna.",
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
        for line in self.dec.drain_lines() {
            tracing::info!(target: "decoder::adsb", "{line}");
        }

        // Throttled full-list snapshot for the `ui:adsb` view. dump1090
        // maintains the aircraft list continuously; we sample it ~1 Hz
        // (a full list each time — the store upserts by icao + ages
        // rows), independent of the text path above.
        self.samples_since_snap += consumed as u64;
        if self.samples_since_snap >= u64::from(ADSB_INPUT_RATE_HZ) {
            self.samples_since_snap = 0;
            for a in self.dec.aircraft_snapshot() {
                AircraftSpot {
                    icao: a.icao,
                    flight: &a.flight,
                    pos: a.position,
                    alt_ft: a.altitude_ft,
                    gs_kt: a.speed_kt,
                    trk_deg: a.track_deg,
                    msgs: a.messages,
                    age_s: a.age_s,
                }
                .write_json(&mut self.events_out);
            }
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;

        // Drain queued JSON into the events port. A 1 Hz snapshot of a
        // few hundred aircraft is a handful of KB; any remainder rides
        // to the next tick (IQ flows continuously).
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

impl BlockFactory for AdsbDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: AdsbDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(AdsbDemod::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{AdsbDemod, AdsbDemodParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, PortMeta};
    use num_complex::Complex;

    fn run(block: &mut AdsbDemod, samples: &[Complex<f32>]) {
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(samples),
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
        assert!(AdsbDemod::new(AdsbDemodParams {
            sample_rate_hz: 0.0,
        })
        .is_err());
    }

    #[test]
    fn silence_does_not_panic_or_emit() {
        let mut b = AdsbDemod::new(AdsbDemodParams::default()).unwrap();
        // Two batches' worth of zero IQ at 2 MS/s.
        let zeros = vec![Complex::new(0.0_f32, 0.0_f32); 600_000];
        run(&mut b, &zeros);
    }
}
