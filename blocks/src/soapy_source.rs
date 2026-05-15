//! Live `SoapySDR` IQ source — reads `Complex<f32>` samples off a real
//! device (RTL-SDR, RSP1A, RSPdx, Airspy, …) and emits them on an `IqF32`
//! port.
//!
//! ## Architecture
//!
//! A dedicated reader thread pulls from the Soapy `RxStream` and pushes
//! every sample into a bounded SPSC ring. `process()` pops exactly what
//! the downstream port asks for each tick. The ring sits between the
//! driver's USB cadence (which delivers in fixed-size blocks every few
//! milliseconds) and the scheduler's tick cadence (every 400µs at the
//! current default), so steady-state flow is **sample-for-sample 1:1**
//! with no dupes and no drops.
//!
//! When the scheduler does fall behind for long enough to fill the ring
//! (GC, IO stall, whatever), the reader drops incoming samples and
//! bumps [`SoapySource::ring_drops`] — explicit, countable, not silent.
//! Driver-reported overflows land in [`SoapySource::overflow_drops`]
//! and timestamp discontinuities (gaps between `stream.time_ns()`
//! readings and the expected delta from `sample_rate_hz`) in
//! [`SoapySource::timestamp_gaps`]. A capture that observes all three
//! counters at zero is provably lossless.
//!
//! ## Lifetime
//!
//! - [`SoapySource::new`] opens the device, configures it, and
//!   activates the Rx stream. Failure here surfaces at flowgraph
//!   instantiation rather than first tick.
//! - [`Block::init`] resets ring + counters and spawns the reader.
//! - [`Block::process`] pops from the ring; short reads zero-fill and
//!   bump [`SoapySource::underrun_samples`].
//! - [`Block::stop`] / `Drop` flips a stop flag, joins the reader, and
//!   deactivates the stream.
//!
//! ## Feature
//!
//! Gated on the crate's `soapysdr` feature. WASM builds never pull this
//! in; `ferrite-blocks` compiled without the feature simply does not
//! register the block, so presets referencing it fail validation with a
//! clear "unknown type" error.

#![cfg(feature = "soapysdr")]

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use num_complex::Complex;
use serde::Deserialize;
use soapysdr::{Device, Direction, ErrorCode, RxStream};

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, OutputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};
use crate::soapy_retry;
use crate::spsc_ring::IqRing;

/// Ring capacity in samples. 512 k complex samples = 4 MiB.
/// 256 ms of headroom at 2 MS/s, 26 ms at 20 MS/s — covers any
/// realistic scheduler hiccup short of a swap storm.
const RING_CAPACITY: usize = 524_288;

/// Samples the reader pulls per `stream.read()` call. Balances syscall
/// overhead against mutex-lock duration. 16 k samples ≈ 8 ms at
/// 2 MS/s — mutex held for a few µs, plenty short for 2.5 kHz tickers.
const READER_CHUNK: usize = 16_384;

/// Read timeout passed to Soapy, microseconds. Long enough to absorb
/// normal USB jitter, short enough that `stop` is responsive.
const READ_TIMEOUT_US: i64 = 100_000;

/// Counters bumped exclusively by the reader thread. `process()` reads
/// them lock-free so diagnostics can sample at any tick rate without
/// perturbing the hot path.
#[derive(Debug, Default)]
struct ReaderCounters {
    overflow_drops: AtomicU64,
    ring_drops: AtomicU64,
    timestamp_gaps: AtomicU64,
    samples_pushed: AtomicU64,
    /// Monotonic nanoseconds (from `CLOCK_MONOTONIC` via `Instant`,
    /// relative to an arbitrary epoch captured at block construction)
    /// of the most recent successful `ring.write()`. Zero until the
    /// first push. Stalled readers can be detected by comparing this
    /// to `now()` — a gap > a few ticks' worth of samples is a sign
    /// the driver hung (common with RTL-SDR on reset / USB glitches).
    last_sample_at_ns: AtomicU64,
}

/// Construction-time params. All fields are optional in the JSON preset;
/// missing fields fall back to [`Default`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SoapySourceParams {
    /// `SoapySDR` device args (e.g. `driver=rtlsdr,serial=0000001`).
    pub args: String,
    /// Driver-level sample rate, Hz. Not all drivers honour arbitrary
    /// values — the block reads back the actual rate after setting.
    pub sample_rate_hz: f64,
    /// RF centre frequency, Hz.
    pub center_freq_hz: f64,
    /// Optional analog filter bandwidth, Hz.
    pub bandwidth_hz: Option<f64>,
    /// Optional antenna port name (driver-specific; e.g. `LNA-H` on RSP1A).
    pub antenna: Option<String>,
    /// Optional manual gain, dB. If both `gain_db` and `agc=true` are
    /// set, the AGC setting is applied first then manual gain overrides.
    pub gain_db: Option<f64>,
    /// Optional AGC toggle. Drivers lacking AGC silently ignore the call.
    pub agc: Option<bool>,
    /// Driver-level automatic DC-offset tracking. Suppresses the LO
    /// leakage spike at the tuned centre on zero-IF SDRs (SDRplay
    /// above ~30 MHz). HackRF's driver doesn't expose this — there
    /// it's a no-op. Default `true`: leave on unless you have a
    /// specific reason (diagnostic A/B, a signal sitting exactly at
    /// carrier that the DC tracker is fighting, or a driver bug).
    pub dc_offset_correction: bool,
    /// Rx channel index. Most drivers only have channel 0.
    pub channel: usize,
    /// Driver-specific settings (`SoapySDRDevice_writeSetting`) — keys
    /// and values are passed through verbatim from
    /// `getSettingInfo`-discovered controls. Applied last so a setting
    /// can override an earlier standard call if it overlaps. Restart-on-
    /// change is the only update path today.
    pub settings: BTreeMap<String, String>,
}

impl Default for SoapySourceParams {
    fn default() -> Self {
        Self {
            args: String::new(),
            sample_rate_hz: 2_400_000.0,
            center_freq_hz: 100_000_000.0,
            bandwidth_hz: None,
            antenna: None,
            gain_db: None,
            agc: None,
            dc_offset_correction: true,
            channel: 0,
            settings: BTreeMap::new(),
        }
    }
}

/// Snapshot of the driver's current state, read back via Soapy getters
/// after each reconfigure. See [`SoapySource::readback`].
///
/// All fields are `Option` because drivers vary in what they implement —
/// some have no bandwidth control, some no AGC, some refuse to return a
/// gain when AGC is on. `serde(skip_serializing_if = "Option::is_none")`
/// keeps the wire shape compact so the UI sees only the fields the
/// driver actually reports.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct SoapyReadback {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sample_rate_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub center_freq_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bandwidth_hz: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agc: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub antenna: Option<String>,
}

type RingHandle = Arc<Mutex<IqRing>>;

// ---------------------------------------------------------------------------
// Gain convention helpers
// ---------------------------------------------------------------------------
//
// Soapy's "overall gain" element is reported in *forward* dB on most
// drivers (RTL-SDR, HackRF, Airspy, …) — higher value = more signal.
// SoapySDRPlay3 is the odd one out: its overall element ranges 0..66 dB
// but the value is gain *reduction* — raw `setGain(0)` is loudest,
// `setGain(66)` is quietest. The element ranges (`IFGR` 20–59, `RFGR`
// 0–27) are likewise in reduction terms.
//
// To keep the wire format uniform we adopt a single convention: the
// `gain_db` field on `SoapySource` params (and on `/api/source`) means
// **user-facing dB gain, higher = louder.** For drivers whose overall
// element is reduction-shaped we invert at the boundary here — both
// when writing (`set_gain`) and when reading back (`gain()`).
//
// This makes `ferrite-ctl tune --gain 50` mean "50 dB of gain" on every
// driver, eliminating the SDRplay-specific UI inversion that lived in
// the frontend and the AI-confusing wire asymmetry.

fn driver_has_inverted_gain(driver_key: &str) -> bool {
    // Match on the lowercase driver name. SoapySDRPlay3 advertises
    // `driver=sdrplay` regardless of which RSP model is attached.
    matches!(driver_key.to_ascii_lowercase().as_str(), "sdrplay")
}

/// Pure-math inversion against the driver's reported gain range —
/// `raw = min + max - user`. The map is symmetric (its own inverse),
/// so the same function converts user→raw and raw→user.
fn invert_gain(value: f64, min: f64, max: f64) -> f64 {
    min + max - value
}

/// Convert a user-facing gain value (high = loud) to the raw value the
/// driver's `setGain` expects. Identity for drivers without inverted
/// gain; for inverted drivers: `raw = min + max - user`.
fn user_gain_to_raw(device: &Device, dir: Direction, ch: usize, user_db: f64) -> f64 {
    let driver = device.driver_key().unwrap_or_default();
    if !driver_has_inverted_gain(&driver) {
        return user_db;
    }
    match device.gain_range(dir, ch).ok() {
        Some(r) => invert_gain(user_db, r.minimum, r.maximum),
        None => user_db,
    }
}

/// Inverse of [`user_gain_to_raw`]: convert the driver's reported gain
/// back to a user-facing value (high = loud). Same call shape because
/// the map is symmetric.
fn raw_gain_to_user(device: &Device, dir: Direction, ch: usize, raw_db: f64) -> f64 {
    user_gain_to_raw(device, dir, ch, raw_db)
}

pub struct SoapySource {
    #[allow(dead_code)]
    device: Device,
    #[allow(dead_code)]
    channel: usize,
    sample_rate_hz: f64,
    center_freq_hz: f64,
    /// Some before [`Block::init`]; taken and moved into the reader
    /// thread when init spawns it.
    stream: Option<RxStream<Complex<f32>>>,
    ring: RingHandle,
    counters: Arc<ReaderCounters>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    ticks: u64,
    /// Samples the process() hot path could not deliver because the
    /// ring was empty. Zero on a healthy run.
    underrun_samples: u64,
    /// Monotonic anchor shared with the reader thread so the block's
    /// main side can compute "seconds since last pushed sample" from
    /// the atomic counter the reader updates.
    start_instant: std::time::Instant,
}

impl SoapySource {
    /// Open the device, apply configuration, and activate the Rx stream.
    /// Blocking — call from `tokio::task::spawn_blocking` when invoking
    /// from an async context.
    ///
    /// Wraps the whole open → configure → activate sequence in a retry
    /// loop. SDRplay in particular sometimes fails `activateStream()`
    /// with `sdrplay_api_Fail` (surfaces as `NotSupported`) when opened
    /// shortly after the previous handle was released — the API service
    /// hasn't finished tearing the prior stream down. Dropping
    /// everything and retrying after a short backoff clears it.
    pub fn new(params: &SoapySourceParams) -> Result<Self> {
        // 2 attempts is enough: `open_with_retry` already catches the
        // makeStrArgs race internally (5× with 400 ms backoff), so the
        // only thing this outer loop guards against is a short-lived
        // "RX stream already opened" that self-clears in ~200 ms.
        // Configuration errors (SDRplay cliff, impossible bandwidth) are
        // no longer classified as transient (see `soapy_retry`) so they
        // fail fast on the first attempt instead of wasting ~1.5 s.
        const MAX_ATTEMPTS: usize = 2;
        const BACKOFF: Duration = Duration::from_millis(400);
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 0..MAX_ATTEMPTS {
            match Self::try_construct(params) {
                Ok(s) => return Ok(s),
                Err(err)
                    if attempt + 1 < MAX_ATTEMPTS
                        && soapy_retry::is_transient_open_chain(&soapy_retry::flatten_chain(
                            &err,
                        )) =>
                {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = MAX_ATTEMPTS,
                        error = %err,
                        "SoapySource construct failed transiently; retrying"
                    );
                    std::thread::sleep(BACKOFF);
                    last_err = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        let err = last_err.unwrap_or_else(|| anyhow!("SoapySource::new failed for unknown reason"));
        let chain = soapy_retry::flatten_chain(&err);
        match soapy_retry::hint_for_exhausted(&chain, params.args.as_str()) {
            Some(hint) => Err(err.context(hint)),
            None => Err(err),
        }
    }

    fn try_construct(params: &SoapySourceParams) -> Result<Self> {
        tracing::info!(
            args = %params.args,
            sample_rate_hz = params.sample_rate_hz,
            center_freq_hz = params.center_freq_hz,
            bandwidth_hz = ?params.bandwidth_hz,
            antenna = ?params.antenna,
            gain_db = ?params.gain_db,
            agc = ?params.agc,
            channel = params.channel,
            settings = ?params.settings,
            "SoapySource::try_construct params"
        );
        let device = open_with_retry(params.args.as_str())
            .with_context(|| format!("open SoapySDR device with args {:?}", params.args))?;
        let dir = Direction::Rx;
        let ch = params.channel;

        device
            .set_sample_rate(dir, ch, params.sample_rate_hz)
            .with_context(|| format!("set sample_rate={}", params.sample_rate_hz))?;
        device
            .set_frequency(dir, ch, params.center_freq_hz, ())
            .with_context(|| format!("set center_freq={}", params.center_freq_hz))?;

        // Driver-level automatic DC-offset tracking. Zero-IF SDRs
        // (HackRF everywhere, SDRplay above ~30 MHz) leak LO energy
        // onto the ADC as a bright spike at the tuned centre. Drivers
        // that implement Soapy's DC-offset-mode interface (SDRplay
        // does; SoapyHackRF does not) handle this in the analog /
        // early-DSP path. Gated by `params.dc_offset_correction` so
        // the operator can A/B or work around a buggy driver
        // implementation. Soft failure: if the driver reports support
        // but the actual call fails (rare), log and continue.
        match device.has_dc_offset_mode(dir, ch) {
            Ok(true) => {
                if let Err(err) = device.set_dc_offset_mode(dir, ch, params.dc_offset_correction) {
                    tracing::warn!(
                        ?err,
                        enable = params.dc_offset_correction,
                        "set_dc_offset_mode failed despite has_dc_offset_mode=true \
                         — driver may be buggy; leaving as-is"
                    );
                } else {
                    tracing::info!(
                        target: "driver",
                        enable = params.dc_offset_correction,
                        "DC-offset tracking {}",
                        if params.dc_offset_correction { "enabled" } else { "disabled" }
                    );
                }
            }
            Ok(false) => {
                // No driver-side correction available (HackRF, some RTL-SDR
                // backends). The off-tune-and-VFO-shift dance remains the
                // operational workaround on these. The `dc_offset_correction`
                // param has no effect — log a debug breadcrumb for parity.
                tracing::debug!(
                    target: "driver",
                    "driver has no DC-offset-mode support; \
                     `dc_offset_correction={}` is a no-op",
                    params.dc_offset_correction
                );
            }
            Err(err) => {
                tracing::debug!(?err, "has_dc_offset_mode query failed (ignored)");
            }
        }
        if let Some(bw) = params.bandwidth_hz {
            device
                .set_bandwidth(dir, ch, bw)
                .with_context(|| format!("set bandwidth={bw}"))?;
        }
        if let Some(ant) = &params.antenna {
            device
                .set_antenna(dir, ch, ant.as_bytes())
                .with_context(|| format!("set antenna={ant}"))?;
        }
        if let Some(agc) = params.agc {
            // Not all drivers expose an AGC mode; swallow an error here
            // and fall through to manual gain if supplied.
            let _ = device.set_gain_mode(dir, ch, agc);
        }
        if let Some(g) = params.gain_db {
            let raw = user_gain_to_raw(&device, dir, ch, g);
            device
                .set_gain(dir, ch, raw)
                .with_context(|| format!("set gain={g} (raw={raw})"))?;
        }
        for (key, value) in &params.settings {
            // Soapy's writeSetting is the catch-all for driver-specific
            // knobs surfaced via getSettingInfo. Treat failures as soft —
            // a stale key from a previous device shouldn't kill the open.
            if let Err(err) = device.write_setting::<&str>(key, value) {
                tracing::warn!(?err, key, value, "writeSetting failed (ignored)");
            }
        }

        let actual_rate = device.sample_rate(dir, ch).unwrap_or(params.sample_rate_hz);
        let actual_freq = device.frequency(dir, ch).unwrap_or(params.center_freq_hz);
        // Log when the driver snaps the requested rate to its supported
        // ladder — this is the signal for downstream rate-aware blocks
        // to rebuild. Picked up via tracing target so an operator can
        // isolate it with `RUST_LOG=flowdiag=info`.
        if (actual_rate - params.sample_rate_hz).abs() > 1.0 {
            tracing::warn!(
                target: "flowdiag",
                requested_rate_hz = params.sample_rate_hz,
                actual_rate_hz = actual_rate,
                "SoapySDR snapped sample rate to nearest supported",
            );
        } else {
            tracing::info!(
                target: "flowdiag",
                actual_rate_hz = actual_rate,
                "SoapySDR sample rate set",
            );
        }

        let mut stream: RxStream<Complex<f32>> = device
            .rx_stream::<Complex<f32>>(&[ch])
            .context("create Rx stream")?;
        if let Err(err) = stream.activate(None) {
            // SDRplay's daemon leaves the stream half-open if activate
            // fails — a follow-up deactivate clears the daemon-side
            // state so the next Device::new doesn't inherit the wedge.
            // Best-effort; the original error is what we report.
            let _ = stream.deactivate(None);
            return Err(anyhow::Error::new(err).context("activate Rx stream"));
        }

        Ok(Self {
            device,
            channel: ch,
            sample_rate_hz: actual_rate,
            center_freq_hz: actual_freq,
            stream: Some(stream),
            ring: Arc::new(Mutex::new(IqRing::new(RING_CAPACITY))),
            counters: Arc::new(ReaderCounters::default()),
            stop: Arc::new(AtomicBool::new(false)),
            reader: None,
            ticks: 0,
            underrun_samples: 0,
            start_instant: std::time::Instant::now(),
        })
    }

    /// Nanoseconds since the reader thread last successfully pushed a
    /// batch of samples into the ring, or `None` if no samples have
    /// landed yet (reader just spawned, or driver is wedged from the
    /// start). Driver hangs show up here as a monotonically increasing
    /// value that never resets — operator-facing code can log a warning
    /// when the gap exceeds a few tick periods.
    #[must_use]
    pub fn stalled_ns(&self) -> Option<u64> {
        let last = self.counters.last_sample_at_ns.load(Ordering::Relaxed);
        if last == 0 {
            return None;
        }
        let now = self.start_instant.elapsed().as_nanos() as u64;
        Some(now.saturating_sub(last))
    }

    /// Post-configure readback — what the hardware actually locked to.
    #[must_use]
    pub const fn sample_rate_hz(&self) -> f64 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    /// Samples dropped because the driver's internal ring overran
    /// (`ErrorCode::Overflow` from `stream.read`). Nonzero means the
    /// reader thread itself was too slow for the rate.
    #[must_use]
    pub fn overflow_drops(&self) -> u64 {
        self.counters.overflow_drops.load(Ordering::Relaxed)
    }

    /// Samples dropped because our in-process ring was full — the
    /// reader produced them but `process()` was too slow to consume.
    /// Nonzero means the pipeline fell behind its real-time clock.
    #[must_use]
    pub fn ring_drops(&self) -> u64 {
        self.counters.ring_drops.load(Ordering::Relaxed)
    }

    /// Count of reader reads whose timestamp drifted from the expected
    /// monotonic delta by more than one sample period. Nonzero means
    /// the driver believes it dropped samples between reads even when
    /// it didn't report an `Overflow`.
    #[must_use]
    pub fn timestamp_gaps(&self) -> u64 {
        self.counters.timestamp_gaps.load(Ordering::Relaxed)
    }

    /// Cumulative samples the reader successfully pushed into the ring.
    /// Useful to compute effective rate from outside the block.
    #[must_use]
    pub fn samples_pushed(&self) -> u64 {
        self.counters.samples_pushed.load(Ordering::Relaxed)
    }

    /// Samples `process()` could not deliver because the ring was
    /// empty — tick-side underruns. Nonzero typically means the
    /// scheduler is ticking faster than the reader can feed.
    #[must_use]
    pub fn underrun_samples(&self) -> u64 {
        self.underrun_samples
    }

    /// Query the driver for its current state. Used by the server after
    /// every `PATCH /api/source` so the UI's `params` reflect what the
    /// hardware is actually doing, not what the user last wrote — AGC
    /// varies IFGR under user gain, drivers silently clamp BW to the
    /// nearest filter, and some tune-steps round the centre frequency.
    ///
    /// Each field is best-effort: drivers that don't support a given
    /// getter leave the slot `None`. The UI treats `None` as "keep the
    /// optimistic value" rather than clobbering with a default.
    pub fn readback(&self) -> SoapyReadback {
        let dir = Direction::Rx;
        let ch = self.channel;
        SoapyReadback {
            sample_rate_hz: self.device.sample_rate(dir, ch).ok(),
            center_freq_hz: self.device.frequency(dir, ch).ok(),
            bandwidth_hz: self.device.bandwidth(dir, ch).ok(),
            gain_db: self
                .device
                .gain(dir, ch)
                .ok()
                .map(|raw| raw_gain_to_user(&self.device, dir, ch, raw)),
            agc: self.device.gain_mode(dir, ch).ok(),
            antenna: self.device.antenna(dir, ch).ok().filter(|s| !s.is_empty()),
        }
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for SoapySource {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "SoapySource",
            placement: Placement::NativeOnly,
            inputs: &[],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[
                ParamSpec {
                    key: "args",
                    label: "Device args",
                    kind: ParamKind::Text {
                        default: "driver=rtlsdr",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "SoapySDR device-args string (`driver=...,serial=...`). Selects the device when multiple are attached.",
                },
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Sample rate",
                    kind: ParamKind::Range {
                        min: 250_000.0,
                        max: 20_000_000.0,
                        step: 1.0,
                        default: 2_400_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Stream rate. Drivers expose a discrete set — see driver notes for valid values.",
                },
                ParamSpec {
                    key: "center_freq_hz",
                    label: "Centre frequency",
                    kind: ParamKind::Range {
                        min: 24_000_000.0,
                        max: 1_800_000_000.0,
                        step: 1.0,
                        default: 100_000_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "RF tuning. Park 50–100 kHz off your target on zero-IF drivers to dodge the DC spike; use the channelizer's `freq_shift_hz` to recover the carrier.",
                },
                ParamSpec {
                    key: "gain_db",
                    label: "Gain",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 60.0,
                        step: 0.5,
                        default: 20.0,
                        unit: "dB",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Overall SoapySDR gain element. Mutually exclusive with `agc_enable=true`; ferrite-ctl folds them atomically. Some drivers expose a separate LNA stage on top — see driver notes.",
                },
                ParamSpec {
                    key: "channel",
                    label: "Rx channel",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 7.0,
                        step: 1.0,
                        default: 0.0,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Receive channel index. 0 for single-tuner SDRs (most common case).",
                },
            ],
            ai_notes: "Hardware IQ source via SoapySDR. The actual radio. Live-tunable params: centre, gain, antenna, AGC, dc_offset_correction. `dc_offset_correction` (bool, default true) drives the driver's automatic DC-offset tracking — suppresses the LO-leakage spike at the tuned centre on zero-IF SDRs (SDRplay honours it; HackRF's driver doesn't expose DC-offset mode, so there it's a no-op — use the off-tune-and-VFO-shift dance). Leave on unless A/B-testing or working around a buggy tracker. Driver-specific gain stages, antenna ports, and notches are documented in the driver-specific operator notes appended to the system prompt.",
        }
    }

    /// Apply the live-tunable params (`center_freq_hz`, `gain_db`,
    /// `antenna`, `agc`, `dc_offset_correction`) directly against the
    /// open device, no stream restart. Anything outside that whitelist
    /// defers to a rebuild by returning `Ok(false)`.
    ///
    /// Most Soapy drivers serialise device-side calls internally, so
    /// concurrent `setFrequency`/`setGain` against an active `RxStream`
    /// is safe. The reader thread keeps pulling samples through the
    /// transition; downstream sees a smooth retune instead of an audio
    /// gap.
    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let Some(obj) = delta.as_object() else {
            return Ok(false);
        };
        const LIVE_KEYS: &[&str] = &[
            "center_freq_hz",
            "gain_db",
            "antenna",
            "agc",
            "dc_offset_correction",
        ];
        if !obj.keys().all(|k| LIVE_KEYS.contains(&k.as_str())) {
            return Ok(false);
        }
        let dir = Direction::Rx;
        let ch = self.channel;
        if let Some(v) = obj.get("center_freq_hz").and_then(|v| v.as_f64()) {
            self.device
                .set_frequency(dir, ch, v, ())
                .with_context(|| format!("live set_frequency={v}"))?;
            self.center_freq_hz = self.device.frequency(dir, ch).unwrap_or(v);
        }
        if let Some(v) = obj.get("gain_db").and_then(|v| v.as_f64()) {
            let raw = user_gain_to_raw(&self.device, dir, ch, v);
            self.device
                .set_gain(dir, ch, raw)
                .with_context(|| format!("live set_gain={v} (raw={raw})"))?;
        }
        if let Some(v) = obj.get("antenna").and_then(|v| v.as_str()) {
            self.device
                .set_antenna(dir, ch, v.as_bytes())
                .with_context(|| format!("live set_antenna={v}"))?;
        }
        if let Some(v) = obj.get("agc").and_then(|v| v.as_bool()) {
            // Drivers without AGC return Err — surface as soft-failure
            // rather than killing the live path.
            if let Err(err) = self.device.set_gain_mode(dir, ch, v) {
                tracing::warn!(?err, agc = v, "live set_gain_mode failed (ignored)");
            }
        }
        if let Some(v) = obj.get("dc_offset_correction").and_then(|v| v.as_bool()) {
            // Drivers without DC-offset mode (HackRF) return Err — soft-fail
            // exactly like AGC so the live path stays alive.
            if let Err(err) = self.device.set_dc_offset_mode(dir, ch, v) {
                tracing::warn!(?err, enable = v, "live set_dc_offset_mode failed (ignored)");
            }
        }
        Ok(true)
    }

    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        Some(self.sample_rate_hz)
    }

    fn output_center_freq_hz(&self, _port: usize) -> Option<f64> {
        Some(self.center_freq_hz)
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        if self.reader.is_some() {
            return Err(anyhow!("SoapySource::init called more than once"));
        }
        let stream = self
            .stream
            .take()
            .ok_or_else(|| anyhow!("SoapySource stream missing at init — was new() called?"))?;

        if let Ok(mut ring) = self.ring.lock() {
            ring.reset();
        }
        self.ticks = 0;
        self.underrun_samples = 0;

        let reader_ring = self.ring.clone();
        let reader_counters = self.counters.clone();
        let reader_stop = self.stop.clone();
        let rate_hz = self.sample_rate_hz;
        let reader_anchor = self.start_instant;
        let reader = thread::Builder::new()
            .name("soapy-rx".into())
            .spawn(move || {
                run_reader(
                    stream,
                    rate_hz,
                    &reader_ring,
                    &reader_counters,
                    &reader_stop,
                    reader_anchor,
                );
            })
            .context("spawn soapy reader thread")?;
        self.reader = Some(reader);
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(out) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_iq_f32_mut)
        else {
            return Ok(Work::new());
        };

        self.ticks = self.ticks.saturating_add(1);
        let want = out.len();
        let got = match self.ring.lock() {
            Ok(mut ring) => ring.read(out),
            Err(_) => return Err(anyhow!("soapy ring mutex poisoned")),
        };
        if got < want {
            self.underrun_samples = self.underrun_samples.saturating_add((want - got) as u64);
        }

        // Report only the samples we actually produced. Zero-filling the
        // tail and reporting `want` would splice periodic zero-runs into
        // the stream — visible downstream as evenly-spaced FFT peaks
        // ("picket fence"). Better to emit a short tick; the scheduler
        // runs again next iteration with more ring data.
        let mut w = Work::new();
        w.produced[0] = got;
        Ok(w)
    }

    fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
        Ok(())
    }
}

impl Drop for SoapySource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

impl BlockFactory for SoapySource {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: SoapySourceParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(SoapySource::new(&p)?))
    }
}

fn run_reader(
    mut stream: RxStream<Complex<f32>>,
    rate_hz: f64,
    ring: &RingHandle,
    counters: &Arc<ReaderCounters>,
    stop: &Arc<AtomicBool>,
    start_instant: std::time::Instant,
) {
    let mut staging: Vec<Complex<f32>> = vec![Complex::new(0.0, 0.0); READER_CHUNK];
    // Timestamp bookkeeping: the expected next time_ns, updated after
    // each successful read. Gap detection skips the first read and any
    // read where the driver reports a zero timestamp (not all drivers
    // populate it — RTL-SDR notably does not).
    let mut expected_time_ns: i64 = 0;
    let ns_per_sample = if rate_hz > 0.0 {
        1_000_000_000.0_f64 / rate_hz
    } else {
        0.0
    };
    let gap_tolerance_ns = (ns_per_sample * 2.0) as i64;

    while !stop.load(Ordering::Relaxed) {
        let result = {
            let dst: &mut [Complex<f32>] = &mut staging[..];
            let mut buffers: [&mut [Complex<f32>]; 1] = [dst];
            stream.read(&mut buffers, READ_TIMEOUT_US)
        };
        match result {
            Ok(0) => {
                thread::sleep(Duration::from_millis(1));
            }
            Ok(n) => {
                let t = stream.time_ns();
                if t > 0 {
                    if expected_time_ns > 0 {
                        let delta = (t - expected_time_ns).abs();
                        if delta > gap_tolerance_ns {
                            counters.timestamp_gaps.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    expected_time_ns = t + (n as f64 * ns_per_sample) as i64;
                }

                let pushed = match ring.lock() {
                    Ok(mut r) => r.write(&staging[..n]),
                    Err(_) => {
                        tracing::warn!("soapy ring mutex poisoned; ending reader");
                        let _ = stream.deactivate(None);
                        return;
                    }
                };
                counters
                    .samples_pushed
                    .fetch_add(pushed as u64, Ordering::Relaxed);
                if pushed > 0 {
                    let now_ns = start_instant.elapsed().as_nanos() as u64;
                    counters.last_sample_at_ns.store(now_ns, Ordering::Relaxed);
                }
                if pushed < n {
                    counters
                        .ring_drops
                        .fetch_add((n - pushed) as u64, Ordering::Relaxed);
                }
            }
            Err(err) if err.code == ErrorCode::Timeout => {
                // Normal idle — try again. Brief sleep so the driver
                // has a chance to fill its internal ring if the pipe
                // was momentarily starved.
                thread::sleep(Duration::from_millis(1));
            }
            Err(err) if err.code == ErrorCode::Overflow => {
                counters.overflow_drops.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("soapy overflow");
                // Invalidate the expected-time tracker — we can't
                // compute a sane delta across the gap.
                expected_time_ns = 0;
            }
            Err(err) => {
                tracing::warn!(?err, "soapy read error; ending reader");
                let _ = stream.deactivate(None);
                return;
            }
        }
    }
    let _ = stream.deactivate(None);
}

/// Open a SoapySDR device with a short retry loop for the
/// "device deletion in-progress" race — a previous handle still
/// tearing down inside the driver. Shared constants + classifier with
/// the raw-FFI probe path live in [`crate::soapy_retry`].
fn open_with_retry(args: &str) -> Result<Device> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..soapy_retry::OPEN_MAX_ATTEMPTS {
        match Device::new(args) {
            Ok(dev) => return Ok(dev),
            Err(err) => {
                let msg = format!("{err}");
                if soapy_retry::is_transient_make_chain(&msg)
                    && attempt + 1 < soapy_retry::OPEN_MAX_ATTEMPTS
                {
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = soapy_retry::OPEN_MAX_ATTEMPTS,
                        "SoapySDR device busy releasing; retrying"
                    );
                    std::thread::sleep(soapy_retry::OPEN_BACKOFF);
                    last_err = Some(anyhow!(err));
                    continue;
                }
                return Err(anyhow!(err));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("Device::new failed for unknown reason")))
}

#[cfg(test)]
mod tests {
    use super::{driver_has_inverted_gain, invert_gain, SoapySource, SoapySourceParams};
    use crate::block::{Block, Placement, PortType};

    #[test]
    fn sdrplay_is_the_only_inverted_driver() {
        assert!(driver_has_inverted_gain("sdrplay"));
        assert!(driver_has_inverted_gain("SDRplay")); // case-insensitive
        assert!(!driver_has_inverted_gain("rtlsdr"));
        assert!(!driver_has_inverted_gain("hackrf"));
        assert!(!driver_has_inverted_gain("airspy"));
        assert!(!driver_has_inverted_gain(""));
    }

    #[test]
    fn gain_inversion_round_trips() {
        // SDRplay's overall element range is 0..66.
        let (min, max) = (0.0, 66.0);
        // User asks for "50 dB of gain" (high = loud); raw on the wire
        // becomes 16 (low = loud for an inverted driver). Round-trip
        // through the symmetric map yields the original.
        let raw = invert_gain(50.0, min, max);
        assert!((raw - 16.0).abs() < f64::EPSILON);
        let user = invert_gain(raw, min, max);
        assert!((user - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn gain_inversion_boundary_values() {
        let (min, max) = (0.0, 66.0);
        // Loudest user-facing = highest raw on a non-inverted driver,
        // and the lowest raw on an inverted driver.
        assert_eq!(invert_gain(0.0, min, max), 66.0);
        assert_eq!(invert_gain(66.0, min, max), 0.0);
        // Midpoint maps to itself.
        assert_eq!(invert_gain(33.0, min, max), 33.0);
    }

    #[test]
    fn spec_is_native_only_iq_out() {
        let s = SoapySource::spec();
        assert_eq!(s.type_name, "SoapySource");
        assert!(matches!(s.placement, Placement::NativeOnly));
        assert_eq!(s.inputs.len(), 0);
        assert_eq!(s.outputs.len(), 1);
        assert_eq!(s.outputs[0].name, "out");
        assert!(matches!(s.outputs[0].port_type, PortType::IqF32));
    }

    #[test]
    fn params_round_trip_through_json() {
        let src = serde_json::json!({
            "args": "driver=rtlsdr,serial=0000001",
            "sample_rate_hz": 2_400_000.0,
            "center_freq_hz": 100_100_000.0,
            "bandwidth_hz": 2_000_000.0,
            "gain_db": 20.0,
            "agc": false,
            "channel": 0,
        });
        let p: SoapySourceParams = serde_json::from_value(src).unwrap();
        assert_eq!(p.args, "driver=rtlsdr,serial=0000001");
        assert!((p.sample_rate_hz - 2_400_000.0).abs() < f64::EPSILON);
        assert_eq!(p.bandwidth_hz, Some(2_000_000.0));
        assert_eq!(p.agc, Some(false));
        assert_eq!(p.channel, 0);
    }

    #[test]
    fn defaults_fill_in_omitted_fields() {
        let p: SoapySourceParams = serde_json::from_value(serde_json::json!({})).unwrap();
        // Matches Default::default().
        assert_eq!(p.args, "");
        assert!(p.bandwidth_hz.is_none());
        assert!(p.gain_db.is_none());
        assert!(p.agc.is_none());
    }

    /// Real-hardware soak: open → init → pump → stop → drop in a loop,
    /// varying centre freq + gain each iteration. Reproduces the
    /// "RX stream already opened" / "device deletion in-progress"
    /// races that surface when params change rapidly. Gated on
    /// `FERRITE_TEST_DEVICE_ARGS`; iterations override via
    /// `FERRITE_TEST_DEVICE_ITERS` (default 20). Inter-iteration delay
    /// is `FERRITE_TEST_DEVICE_GAP_MS` (default 1000).
    ///
    /// ```sh
    /// FERRITE_TEST_DEVICE_ARGS=driver=hackrf \
    ///     cargo test -p ferrite-blocks --features soapysdr \
    ///     device_cycle_soak -- --ignored --nocapture
    /// ```
    ///
    /// **Known SDRplay limitation**: `sdrplay_apiService` mishandles
    /// rapid open/close cycles, failing `activateStream()` with
    /// `sdrplay_api_Fail` on roughly 50–70% of iterations regardless
    /// of inter-call gap (tested up to 3 s). The block-level retry
    /// loop in `SoapySource::new` cannot work around it — the daemon
    /// stays wedged across multiple Device::make/unmake cycles. Real
    /// fix is `systemctl restart sdrplay` between sessions. RTL-SDR
    /// and HackRF do not exhibit this.
    #[test]
    #[ignore = "requires hardware; set FERRITE_TEST_DEVICE_ARGS to run"]
    fn device_cycle_soak() {
        use crate::block::{BlockIo, InitCtx, OutBuf, OutputPort, PortMeta};
        use num_complex::Complex;
        use std::time::Duration;

        let args = std::env::var("FERRITE_TEST_DEVICE_ARGS")
            .expect("set FERRITE_TEST_DEVICE_ARGS=driver=… to run this test");
        let iters: usize = std::env::var("FERRITE_TEST_DEVICE_ITERS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(20);

        // Conservative rate every supported SDR can actually produce
        // (RTL caps at 3.2 MS/s). Freq + gain rotate through a handful
        // of plausible bands to force the driver to retune each cycle.
        let rate_hz = 2_000_000.0_f64;
        let rotations: &[(f64, f64)] = &[
            (100_100_000.0, 20.0),   // FM broadcast
            (433_920_000.0, 25.0),   // EU ISM
            (868_350_000.0, 30.0),   // EU SRD
            (915_000_000.0, 15.0),   // US ISM
            (1_090_000_000.0, 35.0), // ADS-B
        ];

        // Quiet time between open attempts. SDRplay's `sdrplay_apiService`
        // daemon needs ~1s after a stream is deactivated before the next
        // `Device::new` will activate cleanly — without the gap roughly
        // half of opens fail with `sdrplay_api_Fail`. Tunable via the env
        // var so a stress run can probe the lower bound; defaults match
        // realistic UI-driven cadence.
        let inter_iter_ms: u64 = std::env::var("FERRITE_TEST_DEVICE_GAP_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1000);

        let mut total_pushed: u64 = 0;
        let mut iter_failures: Vec<(usize, String)> = Vec::new();

        for i in 0..iters {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(inter_iter_ms));
            }
            let (freq, gain) = rotations[i % rotations.len()];
            eprintln!("[iter {i}/{iters}] rate={rate_hz:.0} freq={freq:.0} gain={gain}");
            let params = SoapySourceParams {
                args: args.clone(),
                sample_rate_hz: rate_hz,
                center_freq_hz: freq,
                gain_db: Some(gain),
                channel: 0,
                ..Default::default()
            };
            let mut src = match SoapySource::new(&params) {
                Ok(s) => s,
                Err(err) => {
                    iter_failures.push((i, format!("new: {err:#}")));
                    continue;
                }
            };
            let mut ctx = InitCtx {
                input_meta: &[],
                output_meta: &[],
                frames_hint: 4096,
            };
            if let Err(err) = src.init(&mut ctx) {
                iter_failures.push((i, format!("init: {err:#}")));
                continue;
            }

            // Pump for ~200 ms total — long enough to confirm samples
            // flow at this rate (~400 k samples should land in the ring).
            let mut buf = vec![Complex::new(0.0_f32, 0.0); 4096];
            for _ in 0..40 {
                std::thread::sleep(Duration::from_millis(5));
                let mut outputs = [OutputPort {
                    name: "out",
                    meta: PortMeta {
                        sample_rate_hz: rate_hz,
                        center_freq_hz: freq,
                    },
                    buf: OutBuf::IqF32(&mut buf),
                }];
                let mut io = BlockIo {
                    inputs: &mut [],
                    outputs: &mut outputs,
                };
                if let Err(err) = src.process(&mut io) {
                    iter_failures.push((i, format!("process: {err:#}")));
                    break;
                }
            }

            let pushed = src.samples_pushed();
            let underruns = src.underrun_samples();
            let ring_drops = src.ring_drops();
            let overflow = src.overflow_drops();
            let gaps = src.timestamp_gaps();
            eprintln!(
                "  pushed={pushed} underruns={underruns} ring_drops={ring_drops} \
                 overflow={overflow} ts_gaps={gaps}"
            );
            if pushed == 0 {
                iter_failures.push((i, "no samples pushed".to_string()));
            }
            total_pushed = total_pushed.saturating_add(pushed);

            if let Err(err) = src.stop() {
                iter_failures.push((i, format!("stop: {err:#}")));
            }
            drop(src);
        }

        eprintln!(
            "device_cycle_soak: {} iterations OK, {} failed, {} samples total",
            iters - iter_failures.len(),
            iter_failures.len(),
            total_pushed
        );
        for (i, msg) in &iter_failures {
            eprintln!("  iter {i}: {msg}");
        }
        assert!(
            iter_failures.is_empty(),
            "{} iteration(s) failed",
            iter_failures.len()
        );
    }
}
