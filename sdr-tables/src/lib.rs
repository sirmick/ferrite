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
    args.split(',')
        .filter_map(|p| p.trim().strip_prefix("driver="))
        .map(str::trim)
        .find(|v| !v.is_empty())
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
    !matches!(driver_key, "sdrplay")
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

    // ── driver-arg parsing + per-driver policy ─────────────────────────

    #[test]
    fn driver_arg_extracts_soapy_driver() {
        assert_eq!(driver_arg("driver=hackrf"), Some("hackrf"));
        assert_eq!(driver_arg("driver=hackrf,serial=abc123"), Some("hackrf"));
        assert_eq!(driver_arg("serial=abc123,driver=sdrplay"), Some("sdrplay"));
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
    fn dc_block_defaults_off_only_for_sdrplay() {
        assert!(dc_block_default_enabled("rtlsdr"));
        assert!(dc_block_default_enabled("hackrf"));
        assert!(!dc_block_default_enabled("sdrplay"));
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
