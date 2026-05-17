//! `WsprDemod` — wraps the vendored `wsprd` for live WSPR decode.
//!
//! WSPR (Weak Signal Propagation Reporter) is a 2-minute-slot
//! beacon mode: 162 symbols of 4-FSK at 1.4648 baud in a ~6 Hz
//! occupied bandwidth, K=32 r=½ convolutional FEC, carrying a
//! compressed callsign + 4-char Maidenhead grid + power (dBm). It's
//! the de-facto HF propagation-sounding mode.
//!
//! ### Audio contract
//!
//! Input is **12 kHz mono real audio**, USB-demodulated, the same
//! chain `Ft8Demod` uses. The WSPR sub-band sits ~1500 Hz into that
//! audio passband. The decode core (`ferrite_wsprd`) wants complex
//! baseband at 375 Hz, so this block carries the front-end:
//!
//! 1. NCO mix-down by 1500 Hz → complex at 12 kHz.
//! 2. Low-pass + ÷32 decimate (`FirdecimCx`) → 375 Hz I/Q.
//! 3. −3 dB peak-normalize the captured window (the vendored detector
//!    keys its sync thresholds off absolute amplitude — without this
//!    it decodes nothing; established empirically against the
//!    reference signal).
//!
//! Keeping the front-end here mirrors the ft8 split (vendored C is
//! the decode core only; the audio path stays in Rust).
//!
//! ### Slot timing
//!
//! 120-second UTC-aligned slots; transmissions begin ~1 s into an
//! even minute and run ~110.6 s. We track wall-clock slot index and
//! decode the just-completed window on each rollover. The decoder
//! does its own ±2 s time-sync search, so exact alignment isn't
//! required — capturing the right ~2-minute period is.
//!
//! ### Placement
//!
//! `Placement::Either` — `wsprd`'s FFTW dependency is shimmed to the
//! same `kiss_fft` ft8_lib uses, so it cross-compiles to
//! `wasm32-unknown-unknown`. Shipped presets place it `node` so the
//! decode happens once server-side and fans out via the log stream.

// `web_time` re-exports `std::time` on native and uses the JS clock on
// wasm32 (where `std::time::SystemTime::now()` panics) — lets the
// UTC-slot-aligned decoder run browser-side.
use web_time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};
use ferrite_liquid_dsp::{FirdecimCx, Nco};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutBuf, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};
use crate::digital_spot::DigitalSpot;

/// Required input sample rate. WSPR audio chain is pinned here; the
/// front-end mix + ÷32 decimate assumes exactly 12 kHz in.
pub const WSPR_AUDIO_RATE_HZ: u32 = 12_000;
/// Audio-domain centre of the WSPR sub-band (Hz) we mix to baseband.
const WSPR_AUDIO_CENTER_HZ: f32 = 1_500.0;
/// 12 kHz → 375 Hz.
const DECIM: u32 = WSPR_AUDIO_RATE_HZ / 375;
/// WSPR slot length (ms). TX is ~110.6 s of this UTC-aligned window.
const SLOT_MS: u64 = 120_000;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct WsprDemodParams {
    /// Construction-time hint for the input rate. `init()` reads the
    /// scheduler-negotiated rate and warns once if it isn't 12 kHz.
    pub sample_rate_hz: f32,
    /// Dial frequency in Hz — tags the decoded spots' metadata only
    /// (does not affect the decode). Lets the log line show the RF
    /// frequency the operator was parked on.
    pub dial_freq_hz: i32,
}

impl Default for WsprDemodParams {
    fn default() -> Self {
        Self {
            #[allow(clippy::cast_precision_loss)]
            sample_rate_hz: WSPR_AUDIO_RATE_HZ as f32,
            dial_freq_hz: 0,
        }
    }
}

/// Hamming-windowed-sinc low-pass, `DECIM` phases × `taps_per_phase`,
/// cutoff `fc_hz` at `fs_hz`. Anti-alias for the ÷32 decimation; the
/// WSPR analysis band is well inside 375/2 Hz so a modest design is
/// plenty (the decoder does its own fine frequency search).
fn lowpass_taps(fs_hz: f32, fc_hz: f32, taps_per_phase: usize) -> Vec<f32> {
    let n = DECIM as usize * taps_per_phase;
    let fc = fc_hz / fs_hz; // normalised cutoff (cycles/sample)
    let mid = (n - 1) as f32 / 2.0;
    let mut taps = vec![0.0f32; n];
    let mut sum = 0.0f32;
    for (i, t) in taps.iter_mut().enumerate() {
        let x = i as f32 - mid;
        let sinc = if x.abs() < 1e-6 {
            2.0 * fc
        } else {
            (2.0 * std::f32::consts::PI * fc * x).sin() / (std::f32::consts::PI * x)
        };
        let w = 0.54 - 0.46 * (2.0 * std::f32::consts::PI * i as f32 / (n - 1) as f32).cos();
        *t = sinc * w;
        sum += *t;
    }
    for t in &mut taps {
        *t /= sum; // unity DC gain
    }
    taps
}

pub struct WsprDemod {
    params: WsprDemodParams,
    nco: Nco,
    decim: FirdecimCx,
    /// ÷32 input staging — `FirdecimCx::execute` consumes exactly
    /// `DECIM` complex samples per output sample.
    stage: Vec<(f32, f32)>,
    /// Captured 375 Hz complex window for the current slot.
    i_buf: Vec<f32>,
    q_buf: Vec<f32>,
    /// UTC slot index (`unix_ms / SLOT_MS`) currently being filled.
    /// `None` until the first sample lands.
    active_slot: Option<u64>,
    warned_off_rate: bool,
    input_rate_hz: f64,
    /// UTF-8 newline-delimited JSON spots queued for drain on the
    /// `events` output port (same transport as RDS / Ft8Demod).
    events_out: Vec<u8>,
}

impl WsprDemod {
    pub fn new(params: WsprDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "wspr_demod: sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        let mut nco = Nco::new().map_err(|e| anyhow::anyhow!("wspr_demod nco: {e}"))?;
        // mix_down_one does y = x·exp(-jθ); +omega pulls the +1500 Hz
        // tone down to baseband (the two rotations cancel).
        #[allow(clippy::cast_precision_loss)]
        let omega = 2.0 * std::f32::consts::PI * WSPR_AUDIO_CENTER_HZ / WSPR_AUDIO_RATE_HZ as f32;
        nco.set_frequency(omega);
        #[allow(clippy::cast_precision_loss)]
        let taps = lowpass_taps(WSPR_AUDIO_RATE_HZ as f32, 150.0, 8);
        let decim = FirdecimCx::from_taps(DECIM, &taps)
            .map_err(|e| anyhow::anyhow!("wspr_demod firdecim: {e}"))?;
        Ok(Self {
            params,
            nco,
            decim,
            stage: Vec::with_capacity(DECIM as usize),
            i_buf: Vec::with_capacity(ferrite_wsprd::WSPR_WINDOW_SAMPLES),
            q_buf: Vec::with_capacity(ferrite_wsprd::WSPR_WINDOW_SAMPLES),
            active_slot: None,
            warned_off_rate: false,
            input_rate_hz: f64::from(params.sample_rate_hz),
            events_out: Vec::new(),
        })
    }

    /// Decode the captured window, emit each spot under
    /// `decoder::wspr`, clear the buffers for the next slot.
    fn drain_decodes(&mut self, slot_unix_ms: u64) {
        // −3 dB peak normalize (front-end requirement — see module
        // docs). Done on a copy so the buffers can just be cleared.
        let mut max_sig = 1e-24f32;
        for (&iv, &qv) in self.i_buf.iter().zip(self.q_buf.iter()) {
            max_sig = max_sig.max(iv.abs()).max(qv.abs());
        }
        let scale = 0.5 / max_sig;
        let i: Vec<f32> = self.i_buf.iter().map(|v| v * scale).collect();
        let q: Vec<f32> = self.q_buf.iter().map(|v| v * scale).collect();

        let spots = ferrite_wsprd::decode_window(&i, &q, self.params.dial_freq_hz);
        tracing::info!(
            target: "decoder::wspr",
            samples = self.i_buf.len(),
            spots = spots.len(),
            slot_unix_ms,
            "slot drained",
        );
        for s in &spots {
            tracing::info!(
                target: "decoder::wspr",
                freq_hz = s.freq_hz,
                snr_db = s.snr_db,
                dt_s = s.dt_s,
                drift_hz = s.drift_hz,
                fano_cycles = s.fano_cycles,
                call = %s.callsign,
                grid = %s.grid,
                pwr_dbm = %s.power_dbm,
                "{}",
                s.message,
            );
        }

        // Structured spots for the `ui:ft8` advanced view (WSPR
        // shares the FT8/FT4 view — beacon rows, no DX call). Separate
        // pass so the tracing path above is byte-for-byte unchanged.
        let utc = slot_unix_ms / 1000;
        for s in &spots {
            #[allow(clippy::cast_possible_truncation)]
            let freq = s.freq_hz as f32;
            #[allow(clippy::cast_possible_truncation)]
            let drift = s.drift_hz.round() as i32;
            DigitalSpot {
                mode: "wspr",
                utc,
                de: &s.callsign,
                dx: None,
                grid: if s.grid.is_empty() {
                    None
                } else {
                    Some(s.grid.as_str())
                },
                snr: s.snr_db,
                dt: s.dt_s,
                freq,
                msg: &s.message,
                pwr_dbm: s.power_dbm.trim().parse::<i32>().ok(),
                drift_hz: Some(drift),
            }
            .write_json(&mut self.events_out);
        }

        self.i_buf.clear();
        self.q_buf.clear();
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for WsprDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "WsprDemod",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            // Decodes still reach the logs panel via the
            // `decoder::wspr` tracing target (unchanged). The `events`
            // port additionally streams structured spots for the
            // shared `ui:ft8` advanced view.
            outputs: &[PortSpec {
                name: "events",
                port_type: PortType::Events,
            }],
            params: &[
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
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Hard-pinned at 12 kHz. The channelizer's output_rate_hz must match; an SsbDemod (USB) feeds this.",
                },
                ParamSpec {
                    key: "dial_freq_hz",
                    label: "Dial frequency",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 30.0e6,
                        step: 1.0,
                        default: 0.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "RF dial frequency, for spot metadata only — does not affect decode. Set it so the logged spots show the band you were on.",
                },
            ],
            ai_notes: "WSPR weak-signal propagation decoder (vendored wsprd). 2-minute UTC-aligned slots. Standard dial frequencies (USB demod, 1500 Hz audio centre): 0.4742 / 1.8366 / 3.5686 / 7.0386 / 10.1387 / 14.0956 / 18.1046 / 21.0946 / 28.1246 MHz. Decodes appear every 2 min — wait ≥4 min before declaring \"no traffic.\" Output: `tail decoder --category wspr`.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 {
                self.input_rate_hz = rate;
                let off = (rate - f64::from(WSPR_AUDIO_RATE_HZ)).abs();
                if off > 1.0 && !self.warned_off_rate {
                    tracing::warn!(
                        target: "decoder::wspr",
                        rate_hz = rate,
                        expected = WSPR_AUDIO_RATE_HZ,
                        "wspr_demod: input rate doesn't match 12 kHz — decoder will produce garbage. Add a RealF32Resamp upstream."
                    );
                    self.warned_off_rate = true;
                }
            }
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
        // Always consume so the scheduler doesn't back-pressure
        // upstream, even while we're between slots.
        work.consumed[0] = src.len();

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let current_slot = now_ms / SLOT_MS;
        let active = self.active_slot.unwrap_or(current_slot);
        if active != current_slot {
            // Slot rolled — decode the just-completed window if we
            // captured a plausible amount (a partial first slot on
            // startup isn't worth the LDPC time).
            if self.i_buf.len() >= ferrite_wsprd::WSPR_WINDOW_SAMPLES / 2 {
                self.drain_decodes(active * SLOT_MS);
            } else {
                self.i_buf.clear();
                self.q_buf.clear();
            }
        }
        self.active_slot = Some(current_slot);

        // Front-end: mix 1500 Hz → baseband, ÷32 decimate to 375 Hz.
        for &s in src.iter() {
            let (re, im) = self.nco.mix_down_one(s, 0.0);
            self.stage.push((re, im));
            if self.stage.len() == DECIM as usize {
                let (oi, oq) = self.decim.execute(&self.stage);
                self.stage.clear();
                if self.i_buf.len() < ferrite_wsprd::WSPR_WINDOW_SAMPLES {
                    self.i_buf.push(oi);
                    self.q_buf.push(oq);
                }
            }
        }

        // Drain queued JSON spots into the events port. Slots are
        // 2 min apart and a slot yields a handful of ~120-byte spots,
        // so the buffer fits a slot's worth with room to spare; any
        // remainder rides along to the next tick.
        if !self.events_out.is_empty() {
            for port in io.outputs.iter_mut() {
                if port.name == "events" {
                    if let OutBuf::Events(dst) = &mut port.buf {
                        let take = self.events_out.len().min(dst.len());
                        if take > 0 {
                            dst[..take].copy_from_slice(&self.events_out[..take]);
                            self.events_out.drain(..take);
                            work.produced[0] = take;
                        }
                    }
                }
            }
        }

        Ok(work)
    }
}

impl BlockFactory for WsprDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: WsprDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(WsprDemod::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{WsprDemod, WsprDemodParams};
    use crate::block::Block;

    #[test]
    fn spec_real_in_events_out() {
        let s = WsprDemod::spec();
        assert_eq!(s.type_name, "WsprDemod");
        assert!(matches!(s.placement, crate::block::Placement::Either));
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.inputs[0].port_type, crate::block::PortType::RealF32);
        assert_eq!(s.outputs.len(), 1);
        assert_eq!(s.outputs[0].name, "events");
        assert_eq!(s.outputs[0].port_type, crate::block::PortType::Events);
    }

    #[test]
    fn rejects_bad_rate() {
        let bad = WsprDemodParams {
            sample_rate_hz: 0.0,
            ..WsprDemodParams::default()
        };
        assert!(WsprDemod::new(bad).is_err());
    }

    #[test]
    fn default_params_construct() {
        let _ = WsprDemod::new(WsprDemodParams::default()).unwrap();
    }
}
