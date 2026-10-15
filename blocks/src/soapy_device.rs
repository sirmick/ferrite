//! The SoapySDR radio handle shared by the source blocks — the open /
//! configure / live-adjust / readback / safe-release lifecycle that
//! [`crate::SoapySource`] (continuous IQ) and `SweepSource`
//! (stepped panorama sweep) both need, so the hard-won driver handling
//! (open retries, SDRplay gain inversion, HackRF gain split, LNA ladder
//! readback, leak-don't-unmake on a hung driver) lives once.
//!
//! [`RadioHandle::open`] opens + configures the device and returns it
//! with an *activated* Rx stream; the owning block drives the stream on
//! its own thread and calls [`RadioHandle::release`] (with the reader's
//! hung verdict) when it is done.

#![cfg(feature = "soapysdr")]

use std::{
    collections::BTreeMap,
    mem::ManuallyDrop,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Mutex,
    },
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use num_complex::Complex;
use serde::Deserialize;
use soapysdr::{Device, Direction, ErrorCode, RxStream};

use crate::soapy_retry;

/// Counters bumped exclusively by the reader thread. `process()` reads
/// them lock-free so diagnostics can sample at any tick rate without
/// perturbing the hot path.
#[derive(Debug, Default)]
pub(crate) struct ReaderCounters {
    pub(crate) overflow_drops: AtomicU64,
    pub(crate) ring_drops: AtomicU64,
    /// Count of ring-overflow **events** — each time the reader had to
    /// evict older samples to stay current (drop-oldest). `ring_drops`
    /// is the total *samples* skipped across all events; this is how many
    /// distinct discontinuities the downstream stream carries.
    pub(crate) ring_gaps: AtomicU64,
    pub(crate) timestamp_gaps: AtomicU64,
    pub(crate) samples_pushed: AtomicU64,
    /// Monotonic nanoseconds (from `CLOCK_MONOTONIC` via `Instant`,
    /// relative to an arbitrary epoch captured at block construction)
    /// of the most recent driver **delivery** — any `Ok(n>0)` read,
    /// even when a full ring dropped the samples (a backpressured
    /// device is still alive). Zero until the first read. Stalled
    /// readers are detected by comparing this to `now()` — a gap >
    /// a few ticks' worth of samples is a sign the driver hung (common
    /// with RTL-SDR on reset / USB glitches).
    pub(crate) last_sample_at_ns: AtomicU64,
    /// Set by the reader when the device stops delivering for
    /// [`HUNG_STALL`] (a wedged driver / unresponsive service). Read in
    /// the owning block's `Drop` to decide whether `SoapySDRDevice_unmake`
    /// is safe to call: against a wedged SDRplay service `ReleaseDevice()`
    /// throws a C++ `runtime_error` that can't unwind through Rust and
    /// aborts the whole process — so on a hung device we leak the handle
    /// (the service restart frees it) rather than crash.
    pub(crate) hung: AtomicBool,
    /// Set when the reader thread ended on a fatal read error — the
    /// stream is gone for good and only a reopen brings samples back.
    pub(crate) reader_ended: AtomicBool,
    /// The fatal read error that ended the reader, for health/MCP.
    pub(crate) reader_error: Mutex<Option<String>>,
}

impl ReaderCounters {
    /// Nanoseconds since the reader last saw a delivery, or `None`
    /// before the first one. `start` is the anchor the reader stamps
    /// `last_sample_at_ns` against.
    pub(crate) fn stalled_ns(&self, start: std::time::Instant) -> Option<u64> {
        let last = self.last_sample_at_ns.load(Ordering::Relaxed);
        if last == 0 {
            return None;
        }
        let now = start.elapsed().as_nanos() as u64;
        Some(now.saturating_sub(last))
    }

    /// Radio liveness snapshot for the supervisor / `health` record.
    pub(crate) fn health(&self, start: std::time::Instant, remote: bool) -> SourceHealth {
        SourceHealth {
            remote,
            hung: self.hung.load(Ordering::Relaxed),
            reader_ended: self.reader_ended.load(Ordering::Relaxed),
            reader_error: self.reader_error.lock().ok().and_then(|g| g.clone()),
            stalled_ms: self.stalled_ns(start).map(|ns| ns / 1_000_000),
            age_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            samples_pushed: self.samples_pushed.load(Ordering::Relaxed),
            overflow_drops: self.overflow_drops.load(Ordering::Relaxed),
            ring_drops: self.ring_drops.load(Ordering::Relaxed),
            ring_gaps: self.ring_gaps.load(Ordering::Relaxed),
        }
    }
}

/// No samples delivered for this long ⇒ treat the driver as hung. Long
/// enough that a healthy active stream (which delivers continuously, even
/// noise) never trips it; short enough to catch a wedge before teardown.
pub(crate) const HUNG_STALL: Duration = Duration::from_secs(3);

/// The slice of an Rx stream the reader loop ([`run_reader`]) actually
/// drives: read a chunk, ask for its timestamp, tear down. Extracting it
/// behind a trait lets the overflow / hung / recovery state machine run
/// against a scripted fake in tests — the hardware path is [`SoapyRx`].
pub(crate) trait RxStreamLike {
    /// Read up to `buf.len()` samples into `buf`. `Ok(n)` = delivered
    /// (`n` may be 0, a spurious wake), or a classified error.
    fn read(&mut self, buf: &mut [Complex<f32>], timeout_us: i64) -> Result<usize, RxReadError>;
    /// Driver timestamp of the most recent read, ns (0 = unavailable —
    /// not every driver populates it; RTL-SDR notably does not).
    fn time_ns(&mut self) -> i64;
    /// Best-effort stream teardown; we're ending, so errors are dropped.
    fn deactivate(&mut self);
}

/// A failed [`RxStreamLike::read`], collapsed to the three cases the
/// reader treats differently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RxReadError {
    /// Nothing arrived within the timeout — normal idle.
    Timeout,
    /// The driver's internal buffer overran (we fell behind); the stream
    /// stays usable, only sample continuity is lost.
    Overflow,
    /// A hard error — device gone / wedged. Ends the reader; the string
    /// is the driver's message, surfaced in the log.
    Fatal(String),
}

/// Hardware adapter: the real SoapySDR Rx stream behind [`RxStreamLike`].
pub(crate) struct SoapyRx(pub(crate) RxStream<Complex<f32>>);

impl RxStreamLike for SoapyRx {
    fn read(&mut self, buf: &mut [Complex<f32>], timeout_us: i64) -> Result<usize, RxReadError> {
        let mut buffers: [&mut [Complex<f32>]; 1] = [buf];
        self.0
            .read(&mut buffers, timeout_us)
            .map_err(|err| match err.code {
                ErrorCode::Timeout => RxReadError::Timeout,
                ErrorCode::Overflow => RxReadError::Overflow,
                _ => RxReadError::Fatal(format!("{err}")),
            })
    }
    fn time_ns(&mut self) -> i64 {
        self.0.time_ns()
    }
    fn deactivate(&mut self) {
        let _ = self.0.deactivate(None);
    }
}

/// Construction-time params. All fields are optional in the JSON preset;
/// missing fields fall back to [`Default`].
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
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
    /// Gain control mode: `"auto"` (the single `gain_db` dial — on HackRF
    /// a footgun-free LNA→VGA→AMP split) or `"manual"` (explicit per-stage
    /// `gain_elements`). Default `"auto"`.
    pub gain_mode: String,
    /// Manual per-stage gains, dB, keyed by the driver's gain-element name
    /// (`source_capabilities` → `rx_channels[].gains[].name`; HackRF: LNA /
    /// VGA / AMP). Applied when `gain_mode = "manual"`.
    pub gain_elements: BTreeMap<String, f64>,
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
            gain_mode: "auto".to_string(),
            gain_elements: BTreeMap::new(),
            dc_offset_correction: true,
            channel: 0,
            settings: BTreeMap::new(),
        }
    }
}

/// Snapshot of the driver's current state, read back via Soapy getters
/// after each reconfigure. See [`RadioHandle::readback`].
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
    /// Realised gain, dB, user-facing (higher = louder), on the same
    /// scale as the `gain_db` param. On SDRplay the LNA stage is counted
    /// at its *attenuation in dB* for the tuned band (see [`Self::lna`]),
    /// not at its state index — so it equals the dial only while the LNA
    /// state is 0, and reads lower (even negative) once attenuation is in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_db: Option<f64>,
    /// The `gain_db` *param* value the hardware currently corresponds to
    /// — the dial position SoapySDR's element sum reports (SDRplay:
    /// `86 − IFGR − RFGR`, LNA state counted as a step). What the UI
    /// slider follows and what a stored `gain_db` is compared against;
    /// same number as `gain_db` on every other driver.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dial_db: Option<f64>,
    /// SDRplay only: the tuner's LNA state, reported separately from the
    /// dB figure. `None` on other drivers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lna: Option<LnaReadback>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agc: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub antenna: Option<String>,
    /// Realized per-stage gains, dB, keyed by element name (HackRF: LNA /
    /// VGA / AMP). Lets the UI show the actual stage split and light an
    /// "AMP on" indicator. Empty for drivers with no named gain elements.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub gain_elements: BTreeMap<String, f64>,
}

/// SDRplay LNA state in the readback. The `RFGR` element is an *index*
/// into a per-band attenuator ladder (SDRplay API specification gain
/// tables, [`ferrite_sdr_tables::sdrplay_lna_table`]), so it is reported
/// here with its real dB value instead of being summed into `gain_db`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LnaReadback {
    /// The RSP model the ladder is for (`hardware_key`: `RSPdx`, …).
    pub hardware: String,
    /// LNA state index (`RFGR`, `rfgain_sel`): 0 = full LNA gain.
    pub state: u32,
    /// Number of valid states in this band's ladder, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub states: Option<u32>,
    /// Attenuation this state inserts in the tuned band, dB. `None` when
    /// no ladder is known for the model/band (then `gain_db` falls back
    /// to counting the state as dB, the SoapySDR convention) or the
    /// state is beyond the band's ladder.
    pub atten_db: Option<f64>,
    /// RF band the ladder applies to, Hz `[lo, hi)`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub band_hz: Option<[f64; 2]>,
}

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

pub(crate) fn driver_has_inverted_gain(driver_key: &str) -> bool {
    // Match on the lowercase driver name. SoapySDRPlay3 advertises
    // `driver=sdrplay` regardless of which RSP model is attached.
    matches!(driver_key.to_ascii_lowercase().as_str(), "sdrplay")
}

/// Pure-math inversion against the driver's reported gain range —
/// `raw = min + max - user`. The map is symmetric (its own inverse),
/// so the same function converts user→raw and raw→user.
pub(crate) fn invert_gain(value: f64, min: f64, max: f64) -> f64 {
    min + max - value
}

/// Convert a user-facing gain value (high = loud) to the raw value the
/// driver's `setGain` expects. Identity for drivers without inverted
/// gain; for inverted drivers: `raw = min + max - user`.
pub(crate) fn user_gain_to_raw(device: &Device, dir: Direction, ch: usize, user_db: f64) -> f64 {
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
pub(crate) fn raw_gain_to_user(device: &Device, dir: Direction, ch: usize, raw_db: f64) -> f64 {
    user_gain_to_raw(device, dir, ch, raw_db)
}

// ---------------------------------------------------------------------------
// HackRF gain distribution
// ---------------------------------------------------------------------------
//
// HackRF has three RX gain stages with very different roles: AMP (a +14 dB
// front-end amp, easily overloaded), LNA/IF (0–40 dB, 8 dB grid — sets the
// noise figure), and VGA/baseband (0–62 dB, 2 dB grid — fills the ADC,
// doesn't change SNR). SoapyHackRF's own overall `setGain` silently flips
// the AMP on at ≥52 dB, which desenses a strong-signal scene. We replace
// that with a footgun-free split: front-gain first (LNA), then VGA, and the
// AMP only in the top 14 dB — and we report the realized stages back so the
// UI can show an "AMP on" indicator instead of springing it on the operator.

pub(crate) const HACKRF_LNA_MAX_DB: f64 = 40.0;
pub(crate) const HACKRF_LNA_STEP_DB: f64 = 8.0;
pub(crate) const HACKRF_VGA_MAX_DB: f64 = 62.0;
pub(crate) const HACKRF_VGA_STEP_DB: f64 = 2.0;
pub(crate) const HACKRF_AMP_DB: f64 = 14.0;
/// Auto-dial ceiling = LNA + VGA + AMP.
pub(crate) const HACKRF_GAIN_MAX_DB: f64 = HACKRF_LNA_MAX_DB + HACKRF_VGA_MAX_DB + HACKRF_AMP_DB;

pub(crate) fn driver_is_hackrf(driver_key: &str) -> bool {
    driver_key.eq_ignore_ascii_case("hackrf")
}

/// Distribute a single `0..=116` dB dial across HackRF's `(LNA, VGA, AMP)`
/// stages, footgun-free: LNA first (noise figure), then VGA (ADC fill),
/// AMP only in the top 14 dB. Each output is snapped to its stage's
/// hardware grid; the function is monotonic in the dial.
pub(crate) fn hackrf_auto_split(dial_db: f64) -> (f64, f64, f64) {
    let dial = dial_db.clamp(0.0, HACKRF_GAIN_MAX_DB);
    // AMP engages only above the LNA+VGA span (0..=102); below that the
    // dial is pure front+baseband and the AMP stays off.
    let amp = if dial > HACKRF_LNA_MAX_DB + HACKRF_VGA_MAX_DB {
        HACKRF_AMP_DB
    } else {
        0.0
    };
    let budget = dial - amp; // 0..=102
    let lna = ((budget / HACKRF_LNA_STEP_DB).floor() * HACKRF_LNA_STEP_DB).min(HACKRF_LNA_MAX_DB);
    let vga =
        (((budget - lna) / HACKRF_VGA_STEP_DB).floor() * HACKRF_VGA_STEP_DB).min(HACKRF_VGA_MAX_DB);
    (lna, vga, amp)
}

/// Apply the gain intent to the device. `manual` sets each named element
/// directly (caps-driven per-stage control, any driver); `auto` uses the
/// footgun-free split on HackRF or the driver's overall element elsewhere.
pub(crate) fn apply_gain(
    device: &Device,
    dir: Direction,
    ch: usize,
    manual: bool,
    gain_db: Option<f64>,
    elements: &BTreeMap<String, f64>,
) -> Result<()> {
    if manual {
        for (name, &val) in elements {
            device
                .set_gain_element(dir, ch, name.as_str(), val)
                .with_context(|| format!("set gain element {name}={val}"))?;
        }
        return Ok(());
    }
    let Some(g) = gain_db else {
        return Ok(());
    };
    let driver = device.driver_key().unwrap_or_default();
    if driver_is_hackrf(&driver) {
        let (lna, vga, amp) = hackrf_auto_split(g);
        device
            .set_gain_element(dir, ch, "LNA", lna)
            .context("set LNA")?;
        device
            .set_gain_element(dir, ch, "VGA", vga)
            .context("set VGA")?;
        device
            .set_gain_element(dir, ch, "AMP", amp)
            .context("set AMP")?;
    } else {
        let raw = user_gain_to_raw(device, dir, ch, g);
        device
            .set_gain(dir, ch, raw)
            .with_context(|| format!("set gain={g} (raw={raw})"))?;
    }
    Ok(())
}

/// Liveness of the radio behind a source block, for the daemon's
/// supervisor and the `health` record. All counters are cumulative since
/// the block was built.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceHealth {
    /// Opened through SoapyRemote.
    pub remote: bool,
    /// The reader stopped seeing deliveries for the hung window.
    pub hung: bool,
    /// The reader thread ended on a fatal read error.
    pub reader_ended: bool,
    pub reader_error: Option<String>,
    /// Milliseconds since the last delivery; `None` before the first.
    pub stalled_ms: Option<u64>,
    /// Milliseconds since the block was built (for "never delivered").
    pub age_ms: u64,
    pub samples_pushed: u64,
    pub overflow_drops: u64,
    pub ring_drops: u64,
    pub ring_gaps: u64,
}

/// An open, configured SoapySDR device plus the gain intent and the
/// driver facts the readback needs. Owned by exactly one source block.
pub struct RadioHandle {
    // `ManuallyDrop` so [`Self::release`] can *skip*
    // `SoapySDRDevice_unmake` when the driver is hung. Derefs to `Device`.
    device: ManuallyDrop<Device>,
    pub(crate) channel: usize,
    /// What the hardware locked to (read back after configure).
    pub(crate) sample_rate_hz: f64,
    pub(crate) center_freq_hz: f64,
    /// Live gain intent, mirrored so a partial live delta (just the dial,
    /// or one stage) re-applies the whole picture coherently.
    gain_mode: String,
    gain_db: Option<f64>,
    gain_elements: BTreeMap<String, f64>,
    /// Opened through SoapyRemote (`driver=remote`). Decides release's
    /// leak-vs-unmake on a hung driver; see [`soapy_retry::is_remote_args`].
    pub(crate) is_remote: bool,
    /// Probed once at open (each is a driver round trip, remote-able):
    /// the Soapy driver key, the hardware model (`RSPdx`, …) and whether
    /// SDRplay HDR mode is on — the inputs the readback's LNA ladder
    /// lookup needs. Settings changes restart the source, so `hdr` can't
    /// go stale.
    driver_key: String,
    hardware_key: String,
    hdr: bool,
    released: bool,
}

/// The radio keys [`RadioHandle::apply_live`] handles without a restart.
pub(crate) const RADIO_LIVE_KEYS: &[&str] = &[
    "center_freq_hz",
    "gain_db",
    "gain_mode",
    "gain_elements",
    "antenna",
    "agc",
    "dc_offset_correction",
];

impl RadioHandle {
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
    pub fn open(params: &SoapySourceParams) -> Result<(Self, RxStream<Complex<f32>>)> {
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
            match Self::try_open(params) {
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
                        "SoapySDR open failed transiently; retrying"
                    );
                    std::thread::sleep(BACKOFF);
                    last_err = Some(err);
                }
                Err(err) => return Err(err),
            }
        }
        let err =
            last_err.unwrap_or_else(|| anyhow!("RadioHandle::open failed for unknown reason"));
        let chain = soapy_retry::flatten_chain(&err);
        match soapy_retry::hint_for_exhausted(&chain, params.args.as_str()) {
            Some(hint) => Err(err.context(hint)),
            None => Err(err),
        }
    }

    fn try_open(params: &SoapySourceParams) -> Result<(Self, RxStream<Complex<f32>>)> {
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
            "RadioHandle::open params"
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
        let gain_manual = params.gain_mode.eq_ignore_ascii_case("manual");
        apply_gain(
            &device,
            dir,
            ch,
            gain_manual,
            params.gain_db,
            &params.gain_elements,
        )
        .context("configure gain")?;
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
        let driver_key = device.driver_key().unwrap_or_default();
        let hardware_key = device.hardware_key().unwrap_or_default();
        // RSPdx HDR mode changes the sub-2 MHz LNA ladder; read it back
        // after the settings landed (the driver's default applies when
        // the operator never wrote `hdr_ctrl`).
        let hdr = driver_has_inverted_gain(&driver_key)
            && device
                .read_setting("hdr_ctrl")
                .map(|v| v.eq_ignore_ascii_case("true"))
                .unwrap_or(false);
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

        Ok((
            Self {
                device: ManuallyDrop::new(device),
                channel: ch,
                sample_rate_hz: actual_rate,
                center_freq_hz: actual_freq,
                gain_mode: params.gain_mode.clone(),
                gain_db: params.gain_db,
                gain_elements: params.gain_elements.clone(),
                is_remote: soapy_retry::is_remote_args(&params.args),
                driver_key,
                hardware_key,
                hdr,
                released: false,
            },
            stream,
        ))
    }

    /// The device, for driver calls the owning block makes directly
    /// (e.g. the sweep's `set_frequency` per step).
    #[must_use]
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Retune the LO and read back what the driver locked to.
    pub fn set_center_freq(&mut self, hz: f64) -> Result<()> {
        self.device
            .set_frequency(Direction::Rx, self.channel, hz, ())
            .with_context(|| format!("set_frequency={hz}"))?;
        self.center_freq_hz = self
            .device
            .frequency(Direction::Rx, self.channel)
            .unwrap_or(hz);
        Ok(())
    }

    /// Apply the live radio keys in `obj` (a subset of
    /// [`RADIO_LIVE_KEYS`]; others are ignored — the caller filters)
    /// directly against the open device, no stream restart.
    ///
    /// Most Soapy drivers serialise device-side calls internally, so
    /// concurrent `setFrequency`/`setGain` against an active `RxStream`
    /// is safe; the reader keeps pulling samples through the change.
    pub fn apply_live(&mut self, obj: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
        let dir = Direction::Rx;
        let ch = self.channel;
        if let Some(v) = obj
            .get("center_freq_hz")
            .and_then(serde_json::Value::as_f64)
        {
            self.set_center_freq(v)
                .with_context(|| format!("live set_frequency={v}"))?;
        }
        // AGC first, gain second. SDRplay ignores a manual IFGR while AGC
        // is on ("Not updating IFGR gain because AGC is enabled"), so the
        // `{agc:false, gain_db:N}` pair only lands if the mode flips before
        // the dial is written. And flipping AGC off on its own must re-push
        // the stored dial: the hardware is sitting at AGC's last pick, not
        // at the value the operator believes is set.
        let mut gain_touched = false;
        if let Some(v) = obj.get("agc").and_then(serde_json::Value::as_bool) {
            // Drivers without AGC return Err — surface as soft-failure
            // rather than killing the live path.
            if let Err(err) = self.device.set_gain_mode(dir, ch, v) {
                tracing::warn!(?err, agc = v, "live set_gain_mode failed (ignored)");
            }
            if !v {
                gain_touched = true;
            }
        }
        // Gain: any of mode / dial / per-stage may arrive in the delta;
        // update the mirrored intent then re-apply the whole picture so a
        // partial change stays coherent (e.g. flipping to manual after
        // dragging a stage, or moving the dial in auto).
        if let Some(v) = obj.get("gain_mode").and_then(serde_json::Value::as_str) {
            self.gain_mode = v.to_string();
            gain_touched = true;
        }
        if let Some(v) = obj.get("gain_db").and_then(serde_json::Value::as_f64) {
            self.gain_db = Some(v);
            gain_touched = true;
        }
        if let Some(map) = obj
            .get("gain_elements")
            .and_then(serde_json::Value::as_object)
        {
            for (name, val) in map {
                if let Some(db) = val.as_f64() {
                    self.gain_elements.insert(name.clone(), db);
                }
            }
            gain_touched = true;
        }
        if gain_touched {
            let manual = self.gain_mode.eq_ignore_ascii_case("manual");
            apply_gain(
                &self.device,
                dir,
                ch,
                manual,
                self.gain_db,
                &self.gain_elements,
            )
            .context("live gain")?;
        }
        if let Some(v) = obj.get("antenna").and_then(serde_json::Value::as_str) {
            self.device
                .set_antenna(dir, ch, v.as_bytes())
                .with_context(|| format!("live set_antenna={v}"))?;
        }
        if let Some(v) = obj
            .get("dc_offset_correction")
            .and_then(serde_json::Value::as_bool)
        {
            // Drivers without DC-offset mode (HackRF) return Err — soft-fail
            // exactly like AGC so the live path stays alive.
            if let Err(err) = self.device.set_dc_offset_mode(dir, ch, v) {
                tracing::warn!(?err, enable = v, "live set_dc_offset_mode failed (ignored)");
            }
        }
        Ok(())
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
        let center_freq_hz = self.device.frequency(dir, ch).ok();
        let antenna = self.device.antenna(dir, ch).ok().filter(|s| !s.is_empty());
        let gain_elements: BTreeMap<String, f64> = self
            .device
            .list_gains(dir, ch)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|name| {
                let v = self.device.gain_element(dir, ch, name.as_str()).ok()?;
                Some((name, v))
            })
            .collect();
        // SoapySDR's overall gain is Σ(element − min) — the dial position
        // (inverted for SDRplay). Correct in dB for every driver whose
        // elements are dB; on SDRplay the `RFGR` element is an LNA state
        // index, so split it out: `gain_db` swaps the index for the
        // band's real attenuation, `lna` carries the state itself.
        let dial_db = self
            .device
            .gain(dir, ch)
            .ok()
            .map(|raw| raw_gain_to_user(&self.device, dir, ch, raw));
        let lna = sdrplay_lna_readback(
            &self.driver_key,
            &self.hardware_key,
            center_freq_hz.unwrap_or(self.center_freq_hz),
            self.hdr,
            antenna.as_deref(),
            &gain_elements,
        );
        let gain_db = match (dial_db, &lna) {
            (Some(dial), Some(l)) => Some(gain_db_with_lna(dial, l)),
            (dial, _) => dial,
        };
        SoapyReadback {
            sample_rate_hz: self.device.sample_rate(dir, ch).ok(),
            center_freq_hz,
            bandwidth_hz: self.device.bandwidth(dir, ch).ok(),
            gain_db,
            dial_db,
            lna,
            agc: self.device.gain_mode(dir, ch).ok(),
            antenna,
            gain_elements,
        }
    }

    /// Release the device. `hung` is the reader's verdict: a healthy
    /// device is unmade normally; a hung *local* driver is leaked (its
    /// unmake throws through FFI and would abort the daemon); a hung
    /// *remote* one is unmade (the socket client survives it). Idempotent.
    pub fn release(&mut self, hung: bool) {
        if self.released {
            return;
        }
        self.released = true;
        if hung && self.is_remote {
            // A SoapyRemote handle is a socket client: its destructor
            // catches its own RPC failures, so unmaking a dead one is
            // safe. Leaking it would be harmful — SoapySDR's factory
            // table would hand the same dead object back on the next
            // open with matching args, and a later module reload would
            // leave it pointing at unmapped code.
            tracing::info!(
                target: "driver",
                "radio released while unresponsive (remote); releasing the handle"
            );
            unsafe { ManuallyDrop::drop(&mut self.device) };
        } else if hung {
            // The driver stopped delivering — the service is wedged.
            // `SoapySDRDevice_unmake` (the `Device` Drop) calls the SDRplay
            // `ReleaseDevice()`, which throws `sdrplay_api_ServiceNotResponding`
            // as a C++ exception; it can't unwind through Rust and aborts the
            // whole daemon. Skip it: leak the (already-dead) handle. The next
            // `device reload` / `systemctl restart sdrplay` frees it cleanly,
            // and the leak is one dangling handle, not a recurring cost.
            tracing::warn!(
                target: "driver",
                "radio released with a hung driver; leaking the device \
                 handle to avoid an unmake() that would abort the process \
                 (restart the SDR service to reclaim it)"
            );
            // Do NOT run the device's Drop (the throwing unmake).
            soapy_retry::note_leaked_handle();
        } else {
            // Healthy: release normally.
            unsafe { ManuallyDrop::drop(&mut self.device) };
        }
        // Stamp the close anchor so the next open waits out the driver's
        // teardown settle window.
        soapy_retry::mark_device_closed();
    }
}

impl Drop for RadioHandle {
    /// Owners call [`RadioHandle::release`] with their reader's hung
    /// verdict first; this only covers a handle dropped before a reader
    /// ever ran (open succeeded, the block was discarded) — healthy.
    fn drop(&mut self) {
        self.release(false);
    }
}

/// The SDRplay LNA-state readback, from the live `RFGR` element and the
/// model/band ladder. `None` for non-SDRplay drivers or when the driver
/// reports no `RFGR`.
pub(crate) fn sdrplay_lna_readback(
    driver_key: &str,
    hardware_key: &str,
    freq_hz: f64,
    hdr: bool,
    antenna: Option<&str>,
    gain_elements: &BTreeMap<String, f64>,
) -> Option<LnaReadback> {
    if !driver_has_inverted_gain(driver_key) {
        return None;
    }
    let rfgr = gain_elements.get("RFGR").copied()?;
    if !rfgr.is_finite() || rfgr < 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let state = rfgr.round() as u32;
    // RSP2 / RSPduo expose the Hi-Z port as an antenna name.
    let hi_z = antenna.is_some_and(|a| a.to_ascii_lowercase().contains("hi-z"));
    let table = ferrite_sdr_tables::sdrplay_lna_table(hardware_key, freq_hz, hdr, hi_z);
    Some(LnaReadback {
        hardware: hardware_key.to_string(),
        state,
        states: table.and_then(|t| u32::try_from(t.atten_db.len()).ok()),
        atten_db: table
            .and_then(|t| t.atten_db.get(state as usize).copied())
            .map(f64::from),
        band_hz: table.map(|t| [t.band_hz.0, t.band_hz.1]),
    })
}

/// The dial counted the LNA state as `state` dB of reduction; replace
/// that with the ladder's attenuation. Falls back to the dial when the
/// ladder is unknown (visible as `lna.atten_db == null`).
pub(crate) fn gain_db_with_lna(dial_db: f64, lna: &LnaReadback) -> f64 {
    match lna.atten_db {
        Some(atten) => dial_db + f64::from(lna.state) - atten,
        None => dial_db,
    }
}

/// Open a SoapySDR device with a short retry loop for the
/// "device deletion in-progress" race — a previous handle still
/// tearing down inside the driver. Shared constants + classifier with
/// the raw-FFI probe path live in [`crate::soapy_retry`].
pub(crate) fn open_with_retry(args: &str) -> Result<Device> {
    // Proactive settle: if a device was closed recently (the common
    // stop→start case), wait out the remainder of the driver's
    // teardown window before the *first* attempt rather than letting
    // it fail and lean on the reactive backoff below. Cold opens and
    // opens long after the last close sleep zero.
    let slept = soapy_retry::wait_settle_after_close();
    if !slept.is_zero() {
        tracing::info!(
            target: "driver",
            settled_ms = slept.as_millis() as u64,
            "waited out post-close settle window before opening device"
        );
    }
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
