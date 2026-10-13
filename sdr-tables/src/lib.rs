//! Per-driver SDR policy tables — the single source of truth shared by the
//! `ferrited` daemon (`server/src/source_policy.rs`) and `ferrite-ctl`, so
//! the mouse, the CLI, and a headless AI all get identical hardware
//! behaviour.
//!
//! Deliberately a tiny leaf crate with no DSP/hardware dependencies: the
//! lightweight HTTP-only `ferrite-ctl` and the daemon can both depend on it
//! cheaply.
//!
//! What lives here (per-driver *policy numbers*, keyed on the lowercased
//! SoapySDR driver short name):
//! - **IF-filter ladders** — the discrete anti-alias filters the hardware
//!   actually has, so the daemon derives the right bandwidth for a rate even
//!   when the device only advertises a *continuous* range (HackRF).
//! - **Sample-rate ceilings** — drivers that advertise a rate the firmware
//!   can't actually stream (SDRplay claims 10.66 MS/s but `activateStream`
//!   fails above 10), so the daemon clamps.
//!
//! The browser's dropdown choices/limits are generated from these tables
//! (`gen-tables` → `if-filter-ladders.generated.json`) and guarded by
//! [`tests::generated_web_ladders_match`], so the UI is in sync by
//! construction. Presentation-only data (labels, tooltips, AI operator
//! notes) stays in `web/src/lib/controls/sdr-presets/<driver>.json`.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Per-driver IF-filter ladders, in Hz, ascending. A driver absent here has
/// no curated ladder — callers fall back to the device probe.
///
/// Ordering is the on-disk key order of the generated JSON; keep it
/// alphabetical so the artifact diff stays stable.
pub const IF_FILTER_LADDERS: &[(&str, &[i64])] = &[
    (
        "hackrf",
        &[
            1_750_000, 2_500_000, 3_500_000, 5_000_000, 5_500_000, 6_000_000, 7_000_000, 8_000_000,
            9_000_000, 10_000_000, 12_000_000, 14_000_000, 15_000_000, 20_000_000, 24_000_000,
        ],
    ),
    (
        "sdrplay",
        &[
            200_000, 300_000, 600_000, 1_536_000, 5_000_000, 6_000_000, 7_000_000, 8_000_000,
        ],
    ),
];

/// Largest IF-filter ladder entry ≤ `rate_hz` for the given driver — the
/// safe anti-alias choice. `None` when the driver has no curated ladder
/// (caller should fall back to the device's advertised ranges) or the rate
/// is below the smallest rung. Case-insensitive on the driver key.
#[must_use]
pub fn recommended_bandwidth_for(driver: &str, rate_hz: f64) -> Option<f64> {
    let key = driver.to_ascii_lowercase();
    let ladder = IF_FILTER_LADDERS
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| *v)?;
    #[allow(clippy::cast_precision_loss)]
    ladder
        .iter()
        .rev()
        .find(|&&x| x as f64 <= rate_hz)
        .map(|&x| x as f64)
}

/// Per-driver sample-rate policy, Hz. `default_hz` is the rate to open a
/// freshly-selected device at; `max_hz` is the highest rate the daemon will
/// actually stream — lower than the advertised max when the firmware can't
/// honour the top rung (SDRplay advertises 10.66 MS/s but `activateStream`
/// fails above 10). These mirror the values the browser preset JSON shows;
/// [`tests::rate_profiles_match_web_presets`] guards them against drift, so
/// the UI dropdown choices stay in sync with what the daemon enforces.
///
/// Keyed on the lowercased SoapySDR driver short name; ordering alphabetical.
pub const SDR_RATE_PROFILES: &[(&str, RateProfile)] = &[
    (
        "airspy",
        RateProfile {
            default_hz: 6_000_000.0,
            max_hz: None,
        },
    ),
    (
        "airspyhf",
        RateProfile {
            default_hz: 768_000.0,
            max_hz: None,
        },
    ),
    (
        "bladerf",
        RateProfile {
            default_hz: 2_000_000.0,
            max_hz: None,
        },
    ),
    (
        "hackrf",
        RateProfile {
            default_hz: 8_000_000.0,
            max_hz: Some(20_000_000.0),
        },
    ),
    (
        "lime",
        RateProfile {
            default_hz: 2_000_000.0,
            max_hz: None,
        },
    ),
    (
        "plutosdr",
        RateProfile {
            default_hz: 2_000_000.0,
            max_hz: None,
        },
    ),
    (
        "rtlsdr",
        RateProfile {
            default_hz: 2_048_000.0,
            max_hz: Some(3_200_000.0),
        },
    ),
    (
        "sdrplay",
        RateProfile {
            default_hz: 2_000_000.0,
            max_hz: Some(10_000_000.0),
        },
    ),
    (
        "uhd",
        RateProfile {
            default_hz: 2_000_000.0,
            max_hz: None,
        },
    ),
];

/// Per-driver sample-rate policy. See [`SDR_RATE_PROFILES`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateProfile {
    /// Rate to open a freshly-selected device at, Hz.
    pub default_hz: f64,
    /// Highest rate the daemon will stream, Hz; `None` = trust the device's
    /// advertised range (no firmware-level workaround needed).
    pub max_hz: Option<f64>,
}

fn rate_profile_for(driver: &str) -> Option<RateProfile> {
    let key = driver.to_ascii_lowercase();
    SDR_RATE_PROFILES
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, p)| *p)
}

/// Highest usable sample rate for the driver, Hz. `None` = no known ceiling
/// (trust the device's advertised range). Case-insensitive.
#[must_use]
pub fn max_sample_rate_for(driver: &str) -> Option<f64> {
    rate_profile_for(driver).and_then(|p| p.max_hz)
}

/// Default sample rate to open a freshly-selected device at, Hz. `None` for
/// an unknown driver (caller leaves the block default). Case-insensitive.
#[must_use]
pub fn default_sample_rate_for(driver: &str) -> Option<f64> {
    rate_profile_for(driver).map(|p| p.default_hz)
}

/// The exact byte content of the web-consumed artifact: a JSON object
/// mapping driver key → ascending Hz array, one driver per line, keys in
/// [`IF_FILTER_LADDERS`] order, trailing newline. Hand-rolled (not
/// `serde_json`) so the on-disk shape is fully pinned and the CI equality
/// check is byte-exact.
#[must_use]
pub fn web_ladders_json() -> String {
    use std::fmt::Write as _;
    let mut s = String::from("{\n");
    for (i, (driver, ladder)) in IF_FILTER_LADDERS.iter().enumerate() {
        let nums = ladder
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let comma = if i + 1 < IF_FILTER_LADDERS.len() {
            ","
        } else {
            ""
        };
        let _ = writeln!(s, "  \"{driver}\": [{nums}]{comma}");
    }
    s.push_str("}\n");
    s
}

/// Absolute path to the generated web artifact, resolved from this crate's
/// manifest dir so it works regardless of the caller's CWD.
fn web_ladders_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../web/src/lib/controls/if-filter-ladders.generated.json")
}

/// `ferrite-ctl gen-tables` body: (re)write the web artifact from the Rust
/// source of truth and print where it landed.
pub fn write_web_ladders() -> Result<()> {
    let path = web_ladders_path();
    let body = web_ladders_json();
    std::fs::write(&path, &body)
        .with_context(|| format!("writing generated ladders to {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

// ── Soapy driver-arg parsing ────────────────────────────────────────────
//
// The single home for turning a SoapySDR args string into a driver key,
// shared by the daemon (`source_policy`, `AppState::tune`) and the runtime
// (`inject_dc_block`) so every per-driver table below is keyed the same way.

/// The SoapySDR `driver=` value from a kv args string
/// (`"driver=hackrf,serial=abc"` → `Some("hackrf")`). `None` when no
/// non-empty `driver=` part is present (Soapy then auto-selects, and only a
/// device probe can name the driver). Not lowercased — every table here is
/// case-insensitive; use [`driver_key`] when you need the normalized name.
#[must_use]
pub fn driver_arg(args: &str) -> Option<&str> {
    let part = |key: &str| {
        args.split(',')
            .filter_map(|p| p.trim().strip_prefix(key))
            .map(str::trim)
            .find(|v| !v.is_empty())
    };
    let driver = part("driver=")?;
    // SoapyRemote: `driver=remote` is the transport. Its enumerate
    // results carry the radio's own key as `remote:driver=<name>` —
    // that is what every per-driver table here is about (dodge ratio,
    // DC block, bandwidth ladder), so prefer it when present. Without
    // the hint (hand-typed `driver=remote,remote=<host>`) callers must
    // probe the device, whose `driver_key` is proxied from the remote.
    if driver.eq_ignore_ascii_case("remote") {
        if let Some(inner) = part("remote:driver=") {
            return Some(inner);
        }
    }
    Some(driver)
}

/// For a SoapyRemote args string that lacks the `remote:driver=` hint
/// (hand-typed `driver=remote,remote=<host>`), return the args with the
/// probed radio's `driver_key` stamped in, so every args-keyed policy
/// (DC-blocker default at compose, dodge ratio, notch/BW ladders) sees
/// the real radio instead of the transport. `None` when nothing needs
/// stamping (not remote, or the hint is already present).
#[must_use]
pub fn with_remote_driver_hint(args: &str, probed_driver_key: &str) -> Option<String> {
    let literal = driver_arg(args)?;
    if !literal.eq_ignore_ascii_case("remote") {
        return None; // the hint was present (driver_arg unwrapped it) or not remote
    }
    let key = probed_driver_key.trim().to_ascii_lowercase();
    if key.is_empty() || key == "remote" {
        return None;
    }
    Some(format!(
        "{},remote:driver={key}",
        args.trim().trim_end_matches(',')
    ))
}

/// Normalized (lowercased) driver short name from a Soapy args string —
/// the `driver=<name>` value of a kv form, or a bare `"sdrplay"` when the
/// string has no `=`. Empty when neither shape yields a name. This is the
/// key every per-driver table below matches on.
#[must_use]
pub fn driver_key(args: &str) -> String {
    let a = args.trim();
    driver_arg(a)
        .unwrap_or(if a.contains('=') { "" } else { a })
        .to_ascii_lowercase()
}

// ── Per-driver policy tables ────────────────────────────────────────────

/// Per-driver DC-spike dodge ratio — the fraction of the channelizer's
/// output rate by which `AppState::tune` parks the source LO off the listen
/// target, so the zero-IF LO/DC spike falls *outside* the demodulated
/// channel and the channelizer recovers the carrier. The daemon-owned
/// default applied whenever the caller passes no explicit `offset_ratio`,
/// so the UI, ferrite-ctl, and a headless AI all dodge identically.
///
/// `0.7` clears a full-width channel (the spike at the LO sits a channel
/// half-width-plus outside the passband). `0` = "no dodge": DC-tracking or
/// low-IF drivers that don't produce an in-band spike, or correct it in
/// hardware. Keyed case-insensitively.
#[must_use]
pub fn tune_offset_ratio_for(driver_key: &str) -> f64 {
    match driver_key.to_ascii_lowercase().as_str() {
        // No hardware DC correction — the dodge is the only fix.
        "hackrf" => 0.7,
        // Zero-IF above ~30 MHz; the dodge complements `dc_offset_correction`
        // so a carrier never sits on the tracker's residual spike.
        "sdrplay" => 0.7,
        // RTL-SDR, Airspy, … : DC-tracking / low-IF, no dodge needed.
        _ => 0.0,
    }
}

/// Whether the auto-injected software complex-IQ DC blocker should default
/// **enabled** for this driver. Zero-IF radios (RTL-SDR, HackRF, …) leak the
/// LO straight to the ADC as a bright line at the tuned centre, so the
/// blocker is on. SDRplay is not zero-IF (real tuner) and its hardware
/// DC-offset tracker handles any residual, so the software blocker — which
/// would otherwise risk nulling a real carrier parked at centre — defaults
/// **off** there. Keyed on the lowercased Soapy driver short name.
#[must_use]
pub fn dc_block_default_enabled(driver_key: &str) -> bool {
    // Case-insensitive like every other table: a probed `driver_key`
    // arrives as the module spells it (`SDRplay` via SoapyRemote's
    // proxy, `sdrplay` from the local module).
    !driver_key.eq_ignore_ascii_case("sdrplay")
}

/// Driver settings (`writeSetting` keys) whose value *is* a realised gain
/// element's readback, so the daemon can tell a settings write that the
/// hardware already satisfies (skip it — a settings change costs a source
/// restart) and notice when a later `gain_db` write moved the element away
/// from a stored setting. SDRplay: `rfgain_sel` is the LNA state, read back
/// as the `RFGR` element (an index, not dB).
#[must_use]
pub fn setting_gain_element(driver_key: &str, setting: &str) -> Option<&'static str> {
    match (driver_key.to_ascii_lowercase().as_str(), setting) {
        ("sdrplay", "rfgain_sel") => Some("RFGR"),
        _ => None,
    }
}

// ── SDRplay LNA-state attenuation tables ────────────────────────────────
//
// SDRplay's `RFGR` gain element is the tuner's **LNA state** — an index
// into a per-band attenuator ladder, not a dB value. SoapySDR's overall
// gain sums every element's `(value − min)` as if it were dB, so a
// readback that trusts it counts LNA state 4 as 4 dB when the ladder says
// 12 dB at MW/HF (measured 8–14 dB on the station, 2026-10-12) or 16 dB at
// UHF. These tables are the SDRplay API Specification's "gain reduction
// tables" (§5 / appendix A in the v3.x documents), per model and RF band,
// as transcribed by gr-sdrplay3; the per-band state counts agree with the
// RSPdx limits other implementations carry (22/19/20/25/27/28/21/19) and
// with SoapySDRPlay3's `RFGR` ranges (RSP1 0–3, RSP2 0–8, RSP1A/1B/duo
// 0–9, RSPdx 0–27). The daemon's readback uses them to report the LNA
// state separately from the dB figure (`SoapyReadback.lna`).

/// One band of an RSP model's LNA attenuation ladder.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LnaTable {
    /// RF band the ladder applies to, Hz: `lo ≤ f < hi`.
    pub band_hz: (f64, f64),
    /// Attenuation per LNA state, dB, indexed by state. The number of
    /// entries is the number of valid states in this band.
    pub atten_db: &'static [u8],
}

const fn lna(lo_hz: f64, hi_hz: f64, atten_db: &'static [u8]) -> LnaTable {
    LnaTable {
        band_hz: (lo_hz, hi_hz),
        atten_db,
    }
}

const MHZ: f64 = 1.0e6;

/// RSPdx / RSPdx-R2 (identical front end). The HDR ladder applies only
/// with HDR mode on and below 2 MHz; otherwise the 0–12 MHz ladder covers
/// it. Bands: 0–12, 12–50, 50–60, 60–250, 250–420, 420–1000, 1000–2000 MHz.
const RSPDX_HDR: LnaTable = lna(
    0.0,
    2.0 * MHZ,
    &[
        0, 3, 6, 9, 12, 15, 18, 21, 24, 25, 27, 30, 33, 36, 39, 42, 45, 48, 51, 54, 57, 60,
    ],
);
const RSPDX: &[LnaTable] = &[
    lna(
        0.0,
        12.0 * MHZ,
        &[
            0, 3, 6, 9, 12, 15, 24, 27, 30, 33, 36, 39, 42, 45, 48, 51, 54, 57, 60,
        ],
    ),
    lna(
        12.0 * MHZ,
        50.0 * MHZ,
        &[
            0, 3, 6, 9, 12, 15, 18, 24, 27, 30, 33, 36, 39, 42, 45, 48, 51, 54, 57, 60,
        ],
    ),
    lna(
        50.0 * MHZ,
        60.0 * MHZ,
        &[
            0, 3, 6, 9, 12, 20, 23, 26, 29, 32, 35, 38, 44, 47, 50, 53, 56, 59, 62, 65, 68, 71, 74,
            77, 80,
        ],
    ),
    lna(
        60.0 * MHZ,
        250.0 * MHZ,
        &[
            0, 3, 6, 9, 12, 15, 24, 27, 30, 33, 36, 39, 42, 45, 48, 51, 54, 57, 60, 63, 66, 69, 72,
            75, 78, 81, 84,
        ],
    ),
    lna(
        250.0 * MHZ,
        420.0 * MHZ,
        &[
            0, 3, 6, 9, 12, 15, 18, 24, 27, 30, 33, 36, 39, 42, 45, 48, 51, 54, 57, 60, 63, 66, 69,
            72, 75, 78, 81, 84,
        ],
    ),
    lna(
        420.0 * MHZ,
        1000.0 * MHZ,
        &[
            0, 7, 10, 13, 16, 19, 22, 25, 31, 34, 37, 40, 43, 46, 49, 52, 55, 58, 61, 64, 67,
        ],
    ),
    lna(
        1000.0 * MHZ,
        2000.0 * MHZ,
        &[
            0, 5, 8, 11, 14, 17, 20, 32, 35, 38, 41, 44, 47, 50, 53, 56, 59, 62, 65,
        ],
    ),
];

/// RSP1A. Bands: 0–60, 60–420, 420–1000, 1000–2000 MHz.
const RSP1A: &[LnaTable] = &[
    lna(0.0, 60.0 * MHZ, &[0, 6, 12, 18, 37, 42, 61]),
    lna(
        60.0 * MHZ,
        420.0 * MHZ,
        &[0, 6, 12, 18, 20, 26, 32, 38, 57, 62],
    ),
    lna(
        420.0 * MHZ,
        1000.0 * MHZ,
        &[0, 7, 13, 19, 20, 27, 33, 39, 45, 64],
    ),
    lna(
        1000.0 * MHZ,
        2000.0 * MHZ,
        &[0, 6, 12, 20, 26, 32, 38, 43, 62],
    ),
];

/// RSP1B. Like the RSP1A, but the 7-state HF ladder stops at 50 MHz and
/// 50–60 MHz already uses the VHF ladder.
const RSP1B: &[LnaTable] = &[
    lna(0.0, 50.0 * MHZ, &[0, 6, 12, 18, 37, 42, 61]),
    lna(
        50.0 * MHZ,
        420.0 * MHZ,
        &[0, 6, 12, 18, 20, 26, 32, 38, 57, 62],
    ),
    lna(
        420.0 * MHZ,
        1000.0 * MHZ,
        &[0, 7, 13, 19, 20, 27, 33, 39, 45, 64],
    ),
    lna(
        1000.0 * MHZ,
        2000.0 * MHZ,
        &[0, 6, 12, 20, 26, 32, 38, 43, 62],
    ),
];

/// RSPduo on the 50 Ω ports. The Hi-Z port below 60 MHz has its own
/// 5-state ladder ([`RSPDUO_HIZ`]).
const RSPDUO: &[LnaTable] = RSP1A;
const RSPDUO_HIZ: LnaTable = lna(0.0, 60.0 * MHZ, &[0, 6, 12, 18, 37]);

/// RSP2 on the 50 Ω ports. Bands: 0–420, 420–1000, 1000–2000 MHz; the
/// Hi-Z port below 60 MHz uses [`RSP2_HIZ`]. (The 1–2 GHz ladder is
/// non-monotonic in the specification; reproduced as published.)
const RSP2: &[LnaTable] = &[
    lna(0.0, 420.0 * MHZ, &[0, 10, 15, 21, 24, 34, 39, 45, 64]),
    lna(420.0 * MHZ, 1000.0 * MHZ, &[0, 7, 10, 17, 22, 41]),
    lna(1000.0 * MHZ, 2000.0 * MHZ, &[0, 5, 21, 15, 15, 34]),
];
const RSP2_HIZ: LnaTable = lna(0.0, 60.0 * MHZ, &[0, 6, 12, 18, 37]);

/// RSP1 (four LNA states, non-monotonic below 420 MHz as published).
const RSP1: &[LnaTable] = &[
    lna(0.0, 420.0 * MHZ, &[0, 24, 19, 43]),
    lna(420.0 * MHZ, 1000.0 * MHZ, &[0, 7, 19, 26]),
    lna(1000.0 * MHZ, 2000.0 * MHZ, &[0, 5, 19, 24]),
];

/// The LNA attenuation ladder for an RSP model (SoapySDRPlay3's
/// `hardware_key`: `RSP1`, `RSP1A`, `RSP1B`, `RSP2`, `RSPduo`, `RSPdx`,
/// `RSPdx-R2`; case-insensitive) at RF frequency `freq_hz`. `hdr` is the
/// RSPdx HDR mode (`hdr_ctrl`), `hi_z` whether the RSP2/RSPduo Hi-Z port is
/// selected (both ignored on models without that variant). `None` for an
/// unknown model or a frequency outside every band (> 2 GHz).
#[must_use]
pub fn sdrplay_lna_table(
    hardware_key: &str,
    freq_hz: f64,
    hdr: bool,
    hi_z: bool,
) -> Option<LnaTable> {
    if !freq_hz.is_finite() || freq_hz < 0.0 {
        return None;
    }
    let in_band = |t: &LnaTable| freq_hz >= t.band_hz.0 && freq_hz < t.band_hz.1;
    let key = hardware_key.trim().to_ascii_lowercase().replace('-', "");
    let (special, ladder): (Option<LnaTable>, &[LnaTable]) = match key.as_str() {
        "rspdx" | "rspdxr2" => (hdr.then_some(RSPDX_HDR), RSPDX),
        "rsp1a" => (None, RSP1A),
        "rsp1b" => (None, RSP1B),
        "rspduo" => (hi_z.then_some(RSPDUO_HIZ), RSPDUO),
        "rsp2" => (hi_z.then_some(RSP2_HIZ), RSP2),
        "rsp1" => (None, RSP1),
        _ => return None,
    };
    special
        .filter(in_band)
        .or_else(|| ladder.iter().copied().find(in_band))
}

/// LNA attenuation, dB, for `state` on `hardware_key` at `freq_hz` — see
/// [`sdrplay_lna_table`]. `None` when the model/band is unknown or the
/// state is beyond the band's ladder (the API clamps such a write).
#[must_use]
pub fn sdrplay_lna_attenuation_db(
    hardware_key: &str,
    freq_hz: f64,
    state: usize,
    hdr: bool,
    hi_z: bool,
) -> Option<f64> {
    sdrplay_lna_table(hardware_key, freq_hz, hdr, hi_z)?
        .atten_db
        .get(state)
        .map(|&db| f64::from(db))
}

/// SDRplay hardware broadcast-notch passbands (Hz). The RF notch covers the
/// AM **and** FM broadcast bands (two disjoint ranges); the DAB notch covers
/// Band III. Fixed in hardware, locale-independent. Mirrors the rules in
/// `web/src/lib/controls/sdr-presets/sdrplay.json`.
const AM_BCAST_HZ: (f64, f64) = (540_000.0, 1_700_000.0);
const FM_BCAST_HZ: (f64, f64) = (88_000_000.0, 108_000_000.0);
const DAB_BAND3_HZ: (f64, f64) = (170_000_000.0, 240_000_000.0);

/// Half-open overlap test: `[span_lo, span_hi)` vs `[band_lo, band_hi)`.
fn overlaps(span_lo: f64, span_hi: f64, band: (f64, f64)) -> bool {
    span_lo < band.1 && span_hi > band.0
}

/// Desired SDRplay notch settings for a tune, as `(writeSetting key,
/// "true"/"false")` pairs. A notch is turned **off** (`"false"`) when the
/// covered span `center ± rate/2` overlaps the band it would attenuate — so
/// we can receive inside it — and left **on** (`"true"`) otherwise, where it
/// only helps reject broadcast overload. Decided on the covered span, not
/// just the centre. Values are strings to match the SoapySDR bool
/// convention.
#[must_use]
pub fn sdrplay_notch_settings(center_hz: f64, rate_hz: f64) -> [(&'static str, &'static str); 2] {
    let half = if rate_hz.is_finite() && rate_hz > 0.0 {
        rate_hz / 2.0
    } else {
        0.0
    };
    let lo = center_hz - half;
    let hi = center_hz + half;

    let in_rf = overlaps(lo, hi, AM_BCAST_HZ) || overlaps(lo, hi, FM_BCAST_HZ);
    let in_dab = overlaps(lo, hi, DAB_BAND3_HZ);

    [
        ("rfnotch_ctrl", if in_rf { "false" } else { "true" }),
        ("dabnotch_ctrl", if in_dab { "false" } else { "true" }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed artifact, inlined at compile time. Path is relative to
    /// this source file (`sdr-tables/src/`).
    const COMMITTED: &str =
        include_str!("../../web/src/lib/controls/if-filter-ladders.generated.json");

    #[test]
    fn generated_web_ladders_match() {
        assert_eq!(
            web_ladders_json(),
            COMMITTED,
            "if-filter-ladders.generated.json is stale — run \
             `cargo run -p ferrite-ctl -- gen-tables` and commit the result",
        );
    }

    #[test]
    fn ladders_are_sorted_ascending_unique_positive() {
        for (driver, ladder) in IF_FILTER_LADDERS {
            assert!(!ladder.is_empty(), "{driver} ladder is empty");
            for w in ladder.windows(2) {
                assert!(
                    w[0] > 0 && w[1] > w[0],
                    "{driver} ladder not strictly ascending / positive: {w:?}",
                );
            }
        }
    }

    #[test]
    fn recommended_bandwidth_picks_largest_le_rate() {
        assert_eq!(
            recommended_bandwidth_for("sdrplay", 10_000_000.0),
            Some(8_000_000.0)
        );
        assert_eq!(
            recommended_bandwidth_for("sdrplay", 2_000_000.0),
            Some(1_536_000.0)
        );
        assert_eq!(recommended_bandwidth_for("sdrplay", 100_000.0), None);
        assert_eq!(
            recommended_bandwidth_for("hackrf", 8_500_000.0),
            Some(8_000_000.0)
        );
        // Case-insensitive (device-reported "HackRF" vs args "hackrf").
        assert_eq!(
            recommended_bandwidth_for("HackRF", 8_500_000.0),
            Some(8_000_000.0)
        );
        // Unknown driver → no ladder → caller falls back to caps.
        assert_eq!(recommended_bandwidth_for("rtlsdr", 2_000_000.0), None);
    }

    /// The browser reads `sample_rate_hz` / `max_sample_rate_hz` from the
    /// per-driver preset JSON to build its rate dropdown; the daemon reads
    /// [`SDR_RATE_PROFILES`] to clamp and to seed device-open defaults. They
    /// MUST agree or the UI would offer a rate the daemon then clamps. This
    /// guard binds them — edit one without the other and the build breaks.
    #[test]
    fn rate_profiles_match_web_presets() {
        use std::fs;
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../web/src/lib/controls/sdr-presets"
        );
        let mut checked = 0;
        for entry in fs::read_dir(dir).expect("read sdr-presets dir") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let body = fs::read_to_string(&path).expect("read preset");
            let v: serde_json::Value = serde_json::from_str(&body)
                .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
            let Some(driver) = v.get("driver_key").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let profile = rate_profile_for(driver).unwrap_or_else(|| {
                panic!("web preset '{driver}' has no SDR_RATE_PROFILES entry — add it")
            });
            let web_default = v.get("sample_rate_hz").and_then(serde_json::Value::as_f64);
            assert_eq!(
                Some(profile.default_hz),
                web_default,
                "{driver}: default-rate drift (table vs preset.sample_rate_hz)",
            );
            let web_max = v
                .get("max_sample_rate_hz")
                .and_then(serde_json::Value::as_f64);
            assert_eq!(
                profile.max_hz, web_max,
                "{driver}: max-rate drift (table vs preset.max_sample_rate_hz)",
            );
            checked += 1;
        }
        assert!(checked >= 9, "expected ≥9 presets checked, got {checked}");
    }

    // ── SDRplay LNA attenuation ladders ────────────────────────────────

    #[test]
    fn rspdx_lna_state_is_not_db() {
        // The station measurement: LNA state 4 ≈ 12 dB at MW, 8–14 dB at
        // HF — not the 4 dB a naive element sum reports.
        let at = |f: f64, s: usize| sdrplay_lna_attenuation_db("RSPdx", f, s, false, false);
        assert_eq!(at(1.0e6, 4), Some(12.0));
        assert_eq!(at(7.1e6, 4), Some(12.0));
        assert_eq!(at(14.0e6, 4), Some(12.0));
        // The 88–150 MHz rule: dial 20 lands state 7, which is 27 dB here.
        assert_eq!(at(118.0e6, 7), Some(27.0));
        assert_eq!(at(118.0e6, 0), Some(0.0));
        // UHF ladder starts with a 7 dB step.
        assert_eq!(at(445.0e6, 1), Some(7.0));
        assert_eq!(at(1090.0e6, 7), Some(32.0));
        // Band edges are `lo ≤ f < hi`.
        assert_eq!(at(12.0e6 - 1.0, 6), Some(24.0));
        assert_eq!(at(12.0e6, 6), Some(18.0));
        // State beyond the band's ladder → None (19 states below 12 MHz).
        assert_eq!(at(1.0e6, 19), None);
        assert_eq!(at(118.0e6, 26), Some(84.0));
        assert_eq!(at(118.0e6, 27), None);
        // Above 2 GHz the specification has no ladder.
        assert_eq!(at(2.1e9, 0), None);
    }

    #[test]
    fn rspdx_hdr_ladder_only_below_2mhz_with_hdr_on() {
        assert_eq!(
            sdrplay_lna_attenuation_db("RSPdx", 1.0e6, 8, true, false),
            Some(24.0)
        );
        assert_eq!(
            sdrplay_lna_attenuation_db("RSPdx", 1.0e6, 8, false, false),
            Some(30.0)
        );
        // HDR on but above 2 MHz → the normal ladder.
        assert_eq!(
            sdrplay_lna_attenuation_db("RSPdx", 3.0e6, 8, true, false),
            Some(30.0)
        );
        let t = sdrplay_lna_table("RSPdx", 1.0e6, true, false).unwrap();
        assert_eq!(t.atten_db.len(), 22);
    }

    #[test]
    fn lna_ladders_match_soapy_rfgr_ranges_and_model_keys() {
        // Widest ladder per model == SoapySDRPlay3's `RFGR` range + 1.
        let widest = |hw: &str| {
            (0..2000)
                .map(|i| f64::from(i) * 1.0e6)
                .filter_map(|f| sdrplay_lna_table(hw, f, false, false))
                .map(|t| t.atten_db.len())
                .max()
                .unwrap()
        };
        assert_eq!(widest("RSP1"), 4);
        assert_eq!(widest("RSP2"), 9);
        assert_eq!(widest("RSPduo"), 10);
        assert_eq!(widest("RSP1A"), 10);
        assert_eq!(widest("RSP1B"), 10);
        assert_eq!(widest("RSPdx"), 28);
        assert_eq!(widest("RSPdx-R2"), 28);
        // Case-insensitive, and the R2 spelling variants.
        assert_eq!(
            sdrplay_lna_table("rspdx", 100.0e6, false, false),
            sdrplay_lna_table("RSPdxR2", 100.0e6, false, false)
        );
        assert_eq!(sdrplay_lna_table("hackrf", 100.0e6, false, false), None);
        // Hi-Z port ladders on the RSP2 / RSPduo below 60 MHz.
        assert_eq!(
            sdrplay_lna_attenuation_db("RSPduo", 7.0e6, 4, false, true),
            Some(37.0)
        );
        assert_eq!(
            sdrplay_lna_attenuation_db("RSPduo", 7.0e6, 5, false, true),
            None
        );
        assert_eq!(
            sdrplay_lna_attenuation_db("RSP2", 7.0e6, 1, false, false),
            Some(10.0)
        );
        assert_eq!(
            sdrplay_lna_attenuation_db("RSP2", 7.0e6, 1, false, true),
            Some(6.0)
        );
        // Every ladder starts at 0 dB (state 0 = full LNA gain).
        for hw in ["RSP1", "RSP1A", "RSP1B", "RSP2", "RSPduo", "RSPdx"] {
            for f in [0.5e6, 30.0e6, 55.0e6, 100.0e6, 300.0e6, 700.0e6, 1.5e9] {
                let t = sdrplay_lna_table(hw, f, true, true).unwrap_or_else(|| panic!("{hw} {f}"));
                assert_eq!(t.atten_db[0], 0, "{hw} at {f}");
            }
        }
    }

    // ── driver-arg parsing + per-driver policy ─────────────────────────

    #[test]
    fn driver_arg_extracts_soapy_driver() {
        assert_eq!(driver_arg("driver=hackrf"), Some("hackrf"));
        assert_eq!(driver_arg("driver=hackrf,serial=abc123"), Some("hackrf"));
        assert_eq!(driver_arg("serial=abc123,driver=sdrplay"), Some("sdrplay"));
        // SoapyRemote: the radio's key rides along as `remote:driver=`.
        assert_eq!(
            driver_arg("driver=remote,remote=tcp://bigear:55132,remote:driver=sdrplay"),
            Some("sdrplay")
        );
        assert_eq!(
            driver_key("driver=remote,remote=bigear,remote:driver=SDRplay"),
            "sdrplay"
        );
        // No hint → `remote` is all we know (callers must probe).
        assert_eq!(driver_arg("driver=remote,remote=bigear"), Some("remote"));
        // …and the probe result gets stamped in once, lowercased.
        let stamped = with_remote_driver_hint("driver=remote,remote=bigear", "SDRplay").unwrap();
        assert_eq!(stamped, "driver=remote,remote=bigear,remote:driver=sdrplay");
        assert_eq!(driver_key(&stamped), "sdrplay");
        assert_eq!(with_remote_driver_hint(&stamped, "SDRplay"), None);
        assert_eq!(with_remote_driver_hint("driver=sdrplay", "SDRplay"), None);
        assert_eq!(
            with_remote_driver_hint("driver=remote,remote=bigear", ""),
            None
        );
        assert_eq!(driver_arg(" driver=rtlsdr "), Some("rtlsdr"));
        assert_eq!(driver_arg("serial=abc123"), None);
        assert_eq!(driver_arg(""), None);
        assert_eq!(driver_arg("driver="), None);
    }

    #[test]
    fn driver_key_normalizes_kv_and_bare_forms() {
        assert_eq!(driver_key("driver=SDRplay,serial=x"), "sdrplay");
        assert_eq!(driver_key("SDRplay"), "sdrplay"); // bare form
        assert_eq!(driver_key("driver="), ""); // empty driver=
        assert_eq!(driver_key("serial=x"), ""); // kv but no driver=
        assert_eq!(driver_key(""), "");
    }

    #[test]
    fn tune_offset_ratio_is_per_driver_and_case_insensitive() {
        assert_eq!(tune_offset_ratio_for("hackrf"), 0.7);
        assert_eq!(tune_offset_ratio_for("HackRF"), 0.7);
        assert_eq!(tune_offset_ratio_for("sdrplay"), 0.7);
        assert_eq!(tune_offset_ratio_for("SDRplay"), 0.7);
        assert_eq!(tune_offset_ratio_for("rtlsdr"), 0.0);
        assert_eq!(tune_offset_ratio_for("airspy"), 0.0);
        assert_eq!(tune_offset_ratio_for(""), 0.0);
    }

    #[test]
    fn rfgain_sel_maps_to_rfgr_on_sdrplay_only() {
        assert_eq!(setting_gain_element("SDRplay", "rfgain_sel"), Some("RFGR"));
        assert_eq!(setting_gain_element("sdrplay", "agc_setpoint"), None);
        assert_eq!(setting_gain_element("hackrf", "rfgain_sel"), None);
    }

    #[test]
    fn dc_block_defaults_off_only_for_sdrplay() {
        assert!(dc_block_default_enabled("rtlsdr"));
        assert!(dc_block_default_enabled("hackrf"));
        assert!(!dc_block_default_enabled("sdrplay"));
        assert!(!dc_block_default_enabled("SDRplay")); // as proxied by SoapyRemote
    }

    #[test]
    fn notch_off_inside_am() {
        // 810 kHz AM, 2 MS/s span reaches into the AM band → RF notch off.
        let s = sdrplay_notch_settings(810_000.0, 2_000_000.0);
        assert_eq!(s[0], ("rfnotch_ctrl", "false"));
        assert_eq!(s[1], ("dabnotch_ctrl", "true"));
    }

    #[test]
    fn notch_off_inside_fm() {
        let s = sdrplay_notch_settings(98_500_000.0, 2_400_000.0);
        assert_eq!(s[0], ("rfnotch_ctrl", "false"));
    }

    #[test]
    fn notch_off_inside_dab() {
        let s = sdrplay_notch_settings(200_000_000.0, 2_400_000.0);
        assert_eq!(s[1], ("dabnotch_ctrl", "false"));
    }

    #[test]
    fn notch_all_on_in_clear_band() {
        // 462 MHz (GMRS) — clear of every broadcast band → both notches on.
        let s = sdrplay_notch_settings(462_000_000.0, 2_400_000.0);
        assert_eq!(s[0], ("rfnotch_ctrl", "true"));
        assert_eq!(s[1], ("dabnotch_ctrl", "true"));
    }

    #[test]
    fn notch_span_spill_into_fm_disables_rf() {
        // Centre just below the FM band, but a wide span spills in.
        let s = sdrplay_notch_settings(87_000_000.0, 4_000_000.0);
        assert_eq!(s[0], ("rfnotch_ctrl", "false"), "wide span spills into FM");
    }

    #[test]
    fn notch_exact_upper_edge_is_outside() {
        // Half-open: a span ending exactly at the band's lower edge is out.
        let s = sdrplay_notch_settings(108_000_000.0, 0.0);
        assert_eq!(
            s[0],
            ("rfnotch_ctrl", "true"),
            "exact upper edge is outside"
        );
    }
}
