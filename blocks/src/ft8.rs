//! `Ft8Demod` — wraps vendored `ft8_lib` for live FT8 / FT4 decode.
//!
//! Slot-based weak-signal digital modes. The block consumes 12 kHz
//! mono audio (USB-demodulated, lightly cleaned), feeds upstream's
//! `monitor_t` rolling waterfall, and at every UTC slot boundary
//! drains accumulated candidates through the LDPC decoder. Decoded
//! messages stream out:
//!
//! - `tracing::info!(target = "decoder::ft8", ...)` — what the UI's
//!   logs panel and `/api/decoder/recent` see; same shape as the
//!   multimon-ng decoders.
//! - One newline-terminated JSON object per message on the `events`
//!   output port — for a future `ui:ft8` panel that wants the
//!   structured fields (callsigns, grid square, SNR).
//!
//! ### Sample-rate contract
//!
//! Hard-pinned to **12 kHz** (FT8 / FT4 specification + ft8_lib's
//! monitor sizing). `RealF32Resamp` upstream is mandatory if the
//! demod chain produces 48 kHz audio (typical SSB chain).
//!
//! ### Slot timing
//!
//! - **FT8** — 15-second slots, transmissions begin at UTC seconds
//!   where `s % 15 == 0`. We track wall-clock seconds (via
//!   `SystemTime`) and decode + reset the waterfall on each
//!   boundary.
//! - **FT4** — 7.5-second slots; same boundary detection at half
//!   the period.
//!
//! Crystal drift between the SDR's clock and host clock is fine — FT8
//! tolerates ±2.5 s of timing slop per slot.
//!
//! ### Placement
//!
//! `Placement::Either` — ft8_lib compiles cleanly to
//! `wasm32-unknown-unknown` via our `libc-stubs/` + wasi-libc layout
//! (same pattern as `multimon-ng`), so a preset author can place the
//! block on the browser side if they want per-tab decode. The
//! shipped `ft8.json` opts for `placement: "node"` instead so the
//! LDPC work happens once on the server and the decoded text fans
//! out via the existing log stream — cheaper for multi-tab UX.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use ferrite_ft8::{Monitor, MonitorConfig, Protocol};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

/// Required input sample rate. Hard-pinned by ft8_lib's monitor
/// sizing — feeding off-rate audio gives garbage waterfall rows.
pub const FT8_INPUT_RATE_HZ: u32 = 12_000;

#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Ft8Mode {
    #[default]
    Ft8,
    Ft4,
}

impl Ft8Mode {
    /// Slot length in milliseconds — the wall-clock period each
    /// transmitter aligns its TX window to. UTC boundaries land at
    /// `t % slot_ms == 0`.
    fn slot_ms(self) -> u64 {
        match self {
            // FT8 = 15 s. FT4 spec is 7.5 s; we use ms for fractional
            // safety so the integer mod arithmetic still aligns.
            Ft8Mode::Ft8 => 15_000,
            Ft8Mode::Ft4 => 7_500,
        }
    }
    /// Length of the actual transmission window inside a slot, in
    /// milliseconds. FT8: 79 symbols × 160 ms = 12.64 s. FT4: 153
    /// symbols × 48 ms ≈ 7.34 s. Audio after this point in each slot
    /// is dead air (TX guard) — we stop feeding the monitor and drop
    /// samples until the next slot starts.
    fn active_ms(self) -> u64 {
        match self {
            Ft8Mode::Ft8 => 12_640,
            Ft8Mode::Ft4 => 5_040,
        }
    }
    fn protocol(self) -> Protocol {
        match self {
            Ft8Mode::Ft8 => Protocol::Ft8,
            Ft8Mode::Ft4 => Protocol::Ft4,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct Ft8DemodParams {
    /// Mode selector: `"ft8"` (default) or `"ft4"`. Live-tunable via
    /// `apply_live_params` so a user can flip between the two without
    /// restarting the pipeline.
    pub mode: Ft8Mode,
    /// Construction-time hint for the input rate. Init() reads the
    /// scheduler-negotiated rate and warns once if it's not 12 kHz.
    pub sample_rate_hz: f32,
    /// Maximum candidates to feed through the LDPC decoder per slot.
    /// Upstream's demo uses 40; lower is faster, higher catches more
    /// weak signals at quadratic cost.
    pub max_candidates: u32,
}

impl Default for Ft8DemodParams {
    fn default() -> Self {
        Self {
            mode: Ft8Mode::Ft8,
            #[allow(clippy::cast_precision_loss)]
            sample_rate_hz: FT8_INPUT_RATE_HZ as f32,
            max_candidates: 40,
        }
    }
}

pub struct Ft8Demod {
    params: Ft8DemodParams,
    /// ft8_lib monitor — `None` until init() lands a valid rate.
    monitor: Option<Monitor>,
    /// Sample-rate-mismatch warning fires once per misconfigure.
    warned_off_rate: bool,
    /// Block-aligned scratch — accumulates the partial block left over
    /// between `process()` calls. ft8_lib wants exactly `block_size`
    /// samples per `monitor_process` invocation.
    scratch: Vec<f32>,
    /// UTC slot index currently being filled. Computed as
    /// `unix_ms / slot_ms`. When this rolls over (i.e. wall-clock
    /// crossed into a new slot) we decode the just-completed slot's
    /// waterfall, reset the monitor, and start filling the new slot.
    /// `None` until the first sample lands and we observe the time.
    active_slot: Option<u64>,
    /// Whether we've already drained decodes for `active_slot` —
    /// happens once we cross from the active TX window into the
    /// per-slot dead zone (see `Ft8Mode::active_ms`). Avoids spending
    /// LDPC time over and over on the same waterfall as samples
    /// continue to arrive in the dead zone before the slot rolls.
    decoded_this_slot: bool,
    input_rate_hz: f64,
}

impl Ft8Demod {
    pub fn new(params: Ft8DemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "ft8_demod: sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        Ok(Self {
            params,
            monitor: None,
            warned_off_rate: false,
            scratch: Vec::new(),
            active_slot: None,
            decoded_this_slot: false,
            input_rate_hz: f64::from(params.sample_rate_hz),
        })
    }

    fn build_monitor(&mut self) -> Result<()> {
        let cfg = MonitorConfig {
            sample_rate: FT8_INPUT_RATE_HZ as i32,
            protocol: self.params.mode.protocol(),
            ..MonitorConfig::ft8_default()
        };
        let mon = Monitor::new(&cfg)
            .ok_or_else(|| anyhow::anyhow!("ft8_demod: Monitor allocation failed"))?;
        self.scratch.reserve(mon.block_size());
        self.monitor = Some(mon);
        Ok(())
    }

    /// Drain the waterfall on a slot boundary, emit each decoded
    /// message via tracing under `decoder::ft8` (or `::ft4`), reset
    /// the monitor so the next slot starts clean.
    fn drain_decodes(&mut self) -> usize {
        let Some(mon) = self.monitor.as_mut() else {
            return 0;
        };
        // Diagnostic: how full is the waterfall at decode time? An
        // aligned FT8 slot fills 79 blocks; FT4 fills 158. If this
        // logs as ~5 the audio chain isn't actually feeding samples
        // during the active window. If it's `max_blocks`-saturated
        // (~93 for FT8) the monitor wrapped and we're decoding the
        // *previous* slot's leftovers.
        let blocks_filled = mon.blocks_filled();
        let blocks_max = mon.blocks_max();
        let messages = mon.decode_slot(self.params.max_candidates as usize);
        match self.params.mode {
            Ft8Mode::Ft8 => tracing::info!(
                target: "decoder::ft8",
                blocks_filled,
                blocks_max,
                candidates_max = self.params.max_candidates,
                msgs = messages.len(),
                "drain — pre-decode waterfall state",
            ),
            Ft8Mode::Ft4 => tracing::info!(
                target: "decoder::ft4",
                blocks_filled,
                blocks_max,
                candidates_max = self.params.max_candidates,
                msgs = messages.len(),
                "drain — pre-decode waterfall state",
            ),
        }
        // tracing's `target:` arg has to be a string literal — split
        // on the protocol so each mode lands on its own log target the
        // UI can filter independently.
        for d in &messages {
            match self.params.mode {
                Ft8Mode::Ft8 => tracing::info!(
                    target: "decoder::ft8",
                    freq_hz = d.freq_hz,
                    snr_db = d.snr_db,
                    ldpc_errors = d.ldpc_errors,
                    "{}",
                    d.text,
                ),
                Ft8Mode::Ft4 => tracing::info!(
                    target: "decoder::ft4",
                    freq_hz = d.freq_hz,
                    snr_db = d.snr_db,
                    ldpc_errors = d.ldpc_errors,
                    "{}",
                    d.text,
                ),
            }
        }
        mon.reset();
        messages.len()
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for Ft8Demod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "Ft8Demod",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            // No output port — same shape as the multimon-ng-backed
            // decoders (PocsagDemod, PacketDemod, EasDemod). Decoded
            // messages reach the UI via the `decoder::ft{4,8}` tracing
            // targets that LogBroadcast pumps over `/ws/logs` and
            // `/api/decoder/recent`.
            outputs: &[],
            params: &[
                ParamSpec {
                    key: "mode",
                    label: "Protocol",
                    kind: ParamKind::EnumString {
                        values: &["ft8", "ft4"],
                        default: "ft8",
                    },
                    // Switching protocol rebuilds the monitor (FT4
                    // and FT8 differ in symbol rate); not live.
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "",
                },
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Audio rate",
                    kind: ParamKind::Range {
                        min: 12_000.0,
                        max: 12_000.0,
                        step: 1.0,
                        default: 12_000.0,
                        unit: "Hz",
                    },
                    // Hard-pinned to 12 kHz; the runtime would refuse
                    // any other value, but exposing the param keeps
                    // the schema explicit.
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "",
                },
                ParamSpec {
                    key: "max_candidates",
                    label: "Max candidates",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 200.0,
                        step: 1.0,
                        default: 40.0,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "",
                },
            ],
            ai_notes: "",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 {
                self.input_rate_hz = rate;
                let off = (rate - f64::from(FT8_INPUT_RATE_HZ)).abs();
                if off > 1.0 && !self.warned_off_rate {
                    tracing::warn!(
                        target: "decoder::ft8",
                        rate_hz = rate,
                        expected = FT8_INPUT_RATE_HZ,
                        "ft8_demod: input rate doesn't match — decoder will produce garbage. Add a RealF32Resamp upstream."
                    );
                    self.warned_off_rate = true;
                }
            }
        }
        if self.monitor.is_none() {
            self.build_monitor()?;
        }
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let mut work = Work::new();
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
        else {
            return Ok(work);
        };
        if src.is_empty() {
            return Ok(work);
        }

        // Always consume what we're given so the scheduler doesn't
        // back-pressure upstream — even when we're in the dead zone
        // and dropping samples.
        work.consumed[0] = src.len();

        let Some(mon) = self.monitor.as_mut() else {
            return Ok(work);
        };
        let block = mon.block_size();
        if block == 0 {
            return Ok(work);
        }

        // Slot tracking: every UTC slot is `slot_ms` long, and the
        // FT8 transmission only occupies the first `active_ms` of it.
        // We feed the monitor during the active window, drain on slot
        // rollover, and drop samples during the dead zone after the
        // TX window closed.
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let slot_ms = self.params.mode.slot_ms();
        let active_ms = self.params.mode.active_ms();
        let current_slot = now_ms / slot_ms;
        let time_in_slot = now_ms - current_slot * slot_ms;

        // Slot rollover: drain the just-completed slot (if we hadn't
        // already mid-dead-zone), reset monitor + scratch, switch to
        // the new slot.
        let active = self.active_slot.unwrap_or(current_slot);
        if active != current_slot {
            if !self.decoded_this_slot {
                // Decoded covers the case where the slot rolled over
                // before we got into the dead zone (small process()
                // ticks at the end of the active window, etc.).
                let _ = self.drain_decodes();
            }
            if let Some(mon) = self.monitor.as_mut() {
                mon.reset();
            }
            self.scratch.clear();
            self.decoded_this_slot = false;
            self.active_slot = Some(current_slot);
        } else if self.active_slot.is_none() {
            self.active_slot = Some(current_slot);
        }

        // Are we in the active TX window of the current slot?
        if time_in_slot < active_ms {
            // Active window — feed samples to the monitor.
            self.scratch.extend_from_slice(src);
            // Re-borrow after the slot-rollover branch above.
            if let Some(mon) = self.monitor.as_mut() {
                while self.scratch.len() >= block {
                    let chunk: Vec<f32> = self.scratch.drain(..block).collect();
                    if let Err(e) = mon.process_block(&chunk) {
                        tracing::warn!(target: "decoder::ft8", error = %e, "process_block failed");
                        break;
                    }
                }
            }
        } else if !self.decoded_this_slot {
            // Just entered the dead zone for this slot — TX window is
            // closed, decode the accumulated waterfall, mark this
            // slot drained. We don't reset the monitor here; the next
            // slot rollover does that. Sample-drop also happens
            // implicitly: the `else if` arm doesn't extend scratch.
            let n_msgs = self.drain_decodes();
            self.decoded_this_slot = true;
            // INFO so a quiet slot still emits a heartbeat the user
            // can see in the activity panel — confirms decoder is
            // alive even when the band is dead.
            tracing::info!(
                target: "decoder::ft8",
                decoded = n_msgs,
                slot_unix_ms = current_slot * slot_ms,
                "slot drained"
            );
        }
        // Else: dead zone, already decoded — silently drop samples.

        Ok(work)
    }
}

impl BlockFactory for Ft8Demod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: Ft8DemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(Ft8Demod::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{Ft8Demod, Ft8DemodParams, Ft8Mode};
    use crate::block::Block;

    #[test]
    fn spec_is_either_real_in_no_outputs() {
        let s = Ft8Demod::spec();
        assert_eq!(s.type_name, "Ft8Demod");
        // Placement::Either — the C lib compiles to both native and
        // wasm32 (build.rs handles the wasm32 toolchain branch the
        // same way multimon-ng does). Preset author picks the side.
        assert!(matches!(s.placement, crate::block::Placement::Either));
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(
            s.outputs.len(),
            0,
            "FT8 emits via tracing — same as the multimon-ng decoders"
        );
        assert_eq!(s.inputs[0].port_type, crate::block::PortType::RealF32);
    }

    #[test]
    fn rejects_bad_rate() {
        let bad = Ft8DemodParams {
            sample_rate_hz: 0.0,
            ..Ft8DemodParams::default()
        };
        assert!(Ft8Demod::new(bad).is_err());
    }

    #[test]
    fn ft8_default_params_construct() {
        let p = Ft8DemodParams::default();
        assert!(matches!(p.mode, Ft8Mode::Ft8));
        let _ = Ft8Demod::new(p).unwrap();
    }
}
