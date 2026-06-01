//! Server-side source policy: the "smart defaults" that make a bare tune
//! Do The Right Thing on the hardware, regardless of which client asked.
//!
//! Historically the two derived source settings — the anti-alias
//! **bandwidth** that should track the sample rate, and the SDRplay
//! **broadcast-notch** filters that should follow the tuned frequency —
//! were computed client-side: once in the browser (`bandwidthForRate` +
//! the `autoSettings` `$effect`) and once in ferrite-ctl (`tune_op`). A
//! headless MCP / AI driver with no browser got neither, so a span-driven
//! retune left an 8 MHz IF feeding a 2.4 MHz digitizer and the AM/FM notch
//! switched on while tuned into the broadcast band.
//!
//! These pure helpers are the single authority the daemon applies at the
//! `patch_source` choke point (see [`crate::app_state::AppState`]), so the
//! mouse, ferrite-ctl, and a fresh AI all get identical behaviour. They
//! make no device calls — bandwidth is derived from the already-probed
//! [`DeviceCapabilities`], notch bands from a tiny fixed table — so they
//! are trivially unit-testable.

use ferrite_runtime::SourceConfig;
use serde_json::Value;

use crate::device::DeviceCapabilities;

/// Fill the daemon-owned derived source settings into `next`, given the
/// already-resolved device `caps` and the `current` config (for override
/// detection). Pure — no device access, no async — so the policy decision
/// is exhaustively unit-testable; the async caps probe lives in the thin
/// `AppState::apply_source_policy` wrapper.
///
/// Rules:
/// - **Bandwidth**: when the sample rate is present and the caller didn't
///   explicitly change `bandwidth_hz` in this patch, set it to the largest
///   advertised filter ≤ rate — but only when the rate changed or the
///   existing BW is unset / `0` / wider-than-rate (so a center-only retune
///   isn't perturbed).
/// - **Notch** (SDRplay only): set `rfnotch_ctrl` / `dabnotch_ctrl` from
///   the tuned span, unless the caller changed that key in this patch.
///
/// Writing an unchanged value is a no-op the caller's `shallow_source_delta`
/// suppresses, so center-only retunes stay a hot apply and the notch only
/// forces a rebuild at a broadcast-band edge.
pub fn apply(current: &SourceConfig, next: &mut SourceConfig, caps: &DeviceCapabilities) {
    let cur = current.params.as_object();
    let cur_f64 = |k: &str| cur.and_then(|o| o.get(k)).and_then(Value::as_f64);
    let cur_setting = |k: &str| {
        cur.and_then(|o| o.get("settings"))
            .and_then(Value::as_object)
            .and_then(|s| s.get(k))
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    let Some(params) = next.params.as_object_mut() else {
        return;
    };

    // --- Anti-alias bandwidth: derive from the (possibly new) rate. ---
    if let Some(rate) = params.get("sample_rate_hz").and_then(Value::as_f64) {
        let new_bw = params.get("bandwidth_hz").and_then(Value::as_f64);
        let caller_set_bw = new_bw != cur_f64("bandwidth_hz"); // explicit this patch
        let rate_changed = cur_f64("sample_rate_hz") != Some(rate);
        // Stale-wide / unset: absent, the documented 0 "auto" sentinel, or
        // an IF wider than the digitizer (the 8 MHz-into-2.4 failure).
        let stale = new_bw.is_none_or(|b| b <= 0.0 || b > rate);
        if !caller_set_bw && (rate_changed || stale) {
            if let Some(bw) = bandwidth_from_caps(caps, rate) {
                if Some(bw) != new_bw {
                    params.insert("bandwidth_hz".into(), serde_json::json!(bw));
                }
            }
        }
    }

    // --- SDRplay broadcast notches: follow the tuned span. ---
    if caps.driver_key == "sdrplay" {
        if let Some(center) = params.get("center_freq_hz").and_then(Value::as_f64) {
            let rate = params
                .get("sample_rate_hz")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let desired = sdrplay_notch_settings(center, rate);
            let settings = params
                .entry("settings")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(smap) = settings.as_object_mut() {
                for (key, val) in desired {
                    let new_val = smap.get(key).and_then(Value::as_str).map(str::to_string);
                    // Respect a deliberate override: caller changed this key.
                    if new_val != cur_setting(key) {
                        continue;
                    }
                    smap.insert(key.to_string(), Value::String(val.to_string()));
                }
            }
        }
    }
}

/// Largest analog IF bandwidth the bound device advertises that does not
/// exceed `rate_hz` — the safe anti-alias choice (never wider than the
/// digitizer). Reads the device's own `bandwidth_ranges_hz` (channel 0),
/// so it's driver-agnostic: SDRplay advertises discrete filter rungs
/// (`min == max`), other drivers a continuous span. Returns `None` when
/// the device advertises no usable bandwidth ≤ rate (e.g. the smallest
/// filter is wider than the requested rate, or no ranges at all) — the
/// caller should then leave `bandwidth_hz` untouched rather than force a
/// value above Nyquist.
#[must_use]
pub fn bandwidth_from_caps(caps: &DeviceCapabilities, rate_hz: f64) -> Option<f64> {
    if !(rate_hz.is_finite() && rate_hz > 0.0) {
        return None;
    }
    let ch = caps.rx_channels.first()?;
    let mut best: Option<f64> = None;
    for r in &ch.bandwidth_ranges_hz {
        // Each range contributes its largest value ≤ rate: the range top
        // when it fits entirely, else `rate` itself when the range spans
        // it (continuous drivers). Discrete rungs have min == max so this
        // reduces to "the rung if it's ≤ rate".
        let candidate = if r.max <= rate_hz {
            Some(r.max)
        } else if r.min <= rate_hz {
            Some(rate_hz)
        } else {
            None
        };
        if let Some(c) = candidate {
            if best.is_none_or(|b| c > b) {
                best = Some(c);
            }
        }
    }
    best
}

/// SDRplay hardware broadcast-notch passbands (Hz). The RF notch covers
/// the AM **and** FM broadcast bands (two disjoint ranges); the DAB notch
/// covers Band III. Fixed in hardware, locale-independent — deliberately
/// not derived from the (US-only, name-fuzzy) band plan. Mirrors the rules
/// shipped in `web/src/lib/controls/sdr-presets/sdrplay.json`.
const AM_BCAST_HZ: (f64, f64) = (540_000.0, 1_700_000.0);
const FM_BCAST_HZ: (f64, f64) = (88_000_000.0, 108_000_000.0);
const DAB_BAND3_HZ: (f64, f64) = (170_000_000.0, 240_000_000.0);

/// Half-open overlap test between the covered span and a notch band.
/// `[span_lo, span_hi)` vs `[band_lo, band_hi)`.
fn overlaps(span_lo: f64, span_hi: f64, band: (f64, f64)) -> bool {
    span_lo < band.1 && span_hi > band.0
}

/// Desired SDRplay notch settings for a tune, as `(writeSetting key,
/// "true"/"false")` pairs. A notch is turned **off** (`"false"`) when the
/// covered span `center ± rate/2` overlaps the band it would attenuate —
/// so we can actually receive inside it — and left **on** (`"true"`)
/// otherwise, where the notch only helps reject broadcast overload.
///
/// Decided on the covered span, not just the centre, so a wide capture
/// that spills into a broadcast band still disables the notch. Values are
/// strings to match the driver `settings` map / SoapySDR bool convention.
#[must_use]
pub fn sdrplay_notch_settings(center_hz: f64, rate_hz: f64) -> [(&'static str, &'static str); 2] {
    // A non-finite/zero rate degenerates to a point at the centre.
    let half = if rate_hz.is_finite() && rate_hz > 0.0 {
        rate_hz / 2.0
    } else {
        0.0
    };
    let lo = center_hz - half;
    let hi = center_hz + half;

    let in_rf = overlaps(lo, hi, AM_BCAST_HZ) || overlaps(lo, hi, FM_BCAST_HZ);
    let in_dab = overlaps(lo, hi, DAB_BAND3_HZ);

    // value_in_band = "false" (notch off), value_out_of_band = "true".
    [
        ("rfnotch_ctrl", if in_rf { "false" } else { "true" }),
        ("dabnotch_ctrl", if in_dab { "false" } else { "true" }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{DeviceCapabilities, DeviceInfo, RangeSpec, RxChannelCapabilities};

    fn rung(hz: f64) -> RangeSpec {
        RangeSpec {
            min: hz,
            max: hz,
            step: None,
        }
    }

    fn caps_with_bw(driver: &str, bws: &[f64]) -> DeviceCapabilities {
        DeviceCapabilities {
            info: DeviceInfo {
                driver: driver.to_string(),
                label: String::new(),
                serial: None,
                args: Default::default(),
            },
            driver_key: driver.to_string(),
            hardware_key: String::new(),
            hardware_info: Default::default(),
            rx_channels: vec![RxChannelCapabilities {
                index: 0,
                antennas: vec![],
                sample_rate_ranges_hz: vec![],
                bandwidth_ranges_hz: bws.iter().copied().map(rung).collect(),
                frequency_ranges_hz: vec![],
                frequency_components: vec![],
                gains: vec![],
                overall_gain_range_db: None,
                has_agc: false,
                has_dc_offset_mode: false,
            }],
            settings: vec![],
        }
    }

    // SDRplay's discrete IF-filter ladder.
    const SDRPLAY_LADDER: &[f64] = &[
        200_000.0,
        300_000.0,
        600_000.0,
        1_536_000.0,
        5_000_000.0,
        6_000_000.0,
        7_000_000.0,
        8_000_000.0,
    ];

    #[test]
    fn bw_rounds_down_to_largest_rung_under_rate() {
        let caps = caps_with_bw("sdrplay", SDRPLAY_LADDER);
        // 2.4 MHz rate -> 1.536 MHz (the gap: nothing between 1.536 and 5).
        assert_eq!(bandwidth_from_caps(&caps, 2_400_000.0), Some(1_536_000.0));
        // 2.0 MHz -> still 1.536.
        assert_eq!(bandwidth_from_caps(&caps, 2_000_000.0), Some(1_536_000.0));
        // 6 MHz -> exact rung 6 MHz.
        assert_eq!(bandwidth_from_caps(&caps, 6_000_000.0), Some(6_000_000.0));
        // 250 kHz -> 200 kHz.
        assert_eq!(bandwidth_from_caps(&caps, 250_000.0), Some(200_000.0));
    }

    #[test]
    fn bw_none_when_smallest_filter_exceeds_rate() {
        let caps = caps_with_bw("sdrplay", SDRPLAY_LADDER);
        // 100 kHz rate, smallest filter is 200 kHz -> leave BW alone.
        assert_eq!(bandwidth_from_caps(&caps, 100_000.0), None);
        // No ranges advertised -> None.
        assert_eq!(
            bandwidth_from_caps(&caps_with_bw("x", &[]), 2_400_000.0),
            None
        );
    }

    #[test]
    fn bw_continuous_range_picks_rate_within_span() {
        // A driver with one continuous range [0, 20M].
        let mut caps = caps_with_bw("hackrf", &[]);
        caps.rx_channels[0].bandwidth_ranges_hz = vec![RangeSpec {
            min: 1_750_000.0,
            max: 28_000_000.0,
            step: None,
        }];
        // rate inside the range -> the rate itself (largest ≤ rate).
        assert_eq!(bandwidth_from_caps(&caps, 10_000_000.0), Some(10_000_000.0));
        // rate below the range min -> None (can't go narrower than 1.75M).
        assert_eq!(bandwidth_from_caps(&caps, 1_000_000.0), None);
    }

    #[test]
    fn notch_off_inside_am() {
        // 1.0 MHz any rate -> inside AM -> rfnotch off, dab on.
        let s = sdrplay_notch_settings(1_000_000.0, 2_400_000.0);
        assert_eq!(s[0], ("rfnotch_ctrl", "false"));
        assert_eq!(s[1], ("dabnotch_ctrl", "true"));
    }

    #[test]
    fn notch_off_inside_fm() {
        let s = sdrplay_notch_settings(98_000_000.0, 2_000_000.0);
        assert_eq!(s[0], ("rfnotch_ctrl", "false"));
        assert_eq!(s[1], ("dabnotch_ctrl", "true"));
    }

    #[test]
    fn notch_off_inside_dab() {
        let s = sdrplay_notch_settings(200_000_000.0, 8_000_000.0);
        assert_eq!(s[0], ("rfnotch_ctrl", "true"));
        assert_eq!(s[1], ("dabnotch_ctrl", "false"));
    }

    #[test]
    fn notch_all_on_in_clear_band() {
        // 145 MHz (2 m), 2 MHz span — clear of every broadcast notch.
        let s = sdrplay_notch_settings(145_000_000.0, 2_000_000.0);
        assert_eq!(s[0], ("rfnotch_ctrl", "true"));
        assert_eq!(s[1], ("dabnotch_ctrl", "true"));
    }

    #[test]
    fn notch_span_spill_into_fm_disables_rf() {
        // Centre 86 MHz (below FM start 88) but 8 MHz span reaches 90 MHz
        // -> spills into FM -> rfnotch off.
        let s = sdrplay_notch_settings(86_000_000.0, 8_000_000.0);
        assert_eq!(s[0], ("rfnotch_ctrl", "false"));
    }

    #[test]
    fn notch_exact_upper_edge_is_outside() {
        // Centre exactly at FM upper edge 108 MHz, negligible span ->
        // half-open: 108 is NOT inside [88,108) -> notch stays on.
        let s = sdrplay_notch_settings(108_000_000.0, 0.0);
        assert_eq!(s[0], ("rfnotch_ctrl", "true"));
    }

    // --- apply(): the full server decision logic, hardware-free ---

    fn sdrplay_caps() -> DeviceCapabilities {
        caps_with_bw("sdrplay", SDRPLAY_LADDER)
    }

    fn src(params: serde_json::Value) -> SourceConfig {
        SourceConfig {
            type_name: "SoapySource".into(),
            params,
        }
    }

    fn bw(cfg: &SourceConfig) -> Option<f64> {
        cfg.params.get("bandwidth_hz").and_then(|v| v.as_f64())
    }
    fn notch(cfg: &SourceConfig, key: &str) -> Option<String> {
        cfg.params
            .get("settings")
            .and_then(|s| s.get(key))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    }

    #[test]
    fn apply_fills_bw_from_rate_when_absent() {
        let current = src(serde_json::json!({ "sample_rate_hz": 240_000.0 }));
        let mut next =
            src(serde_json::json!({ "sample_rate_hz": 2_400_000.0, "center_freq_hz": 1.0e6 }));
        apply(&current, &mut next, &sdrplay_caps());
        assert_eq!(bw(&next), Some(1_536_000.0));
    }

    #[test]
    fn apply_rewrites_stale_wide_bw() {
        // Rate dropped to 2.4M but BW carried over at 8M (the failure mode).
        let current =
            src(serde_json::json!({ "sample_rate_hz": 8_000_000.0, "bandwidth_hz": 8_000_000.0 }));
        let mut next = src(
            serde_json::json!({ "sample_rate_hz": 2_400_000.0, "bandwidth_hz": 8_000_000.0, "center_freq_hz": 1.0e6 }),
        );
        apply(&current, &mut next, &sdrplay_caps());
        assert_eq!(bw(&next), Some(1_536_000.0));
    }

    #[test]
    fn apply_zero_bw_sentinel_is_derived() {
        let current =
            src(serde_json::json!({ "sample_rate_hz": 2_400_000.0, "bandwidth_hz": 0.0 }));
        let mut next = src(
            serde_json::json!({ "sample_rate_hz": 2_400_000.0, "bandwidth_hz": 0.0, "center_freq_hz": 1.0e6 }),
        );
        apply(&current, &mut next, &sdrplay_caps());
        assert_eq!(bw(&next), Some(1_536_000.0));
    }

    #[test]
    fn apply_preserves_explicit_caller_bw() {
        // Caller explicitly pins a valid BW <= rate this patch -> untouched.
        let current =
            src(serde_json::json!({ "sample_rate_hz": 2_400_000.0, "bandwidth_hz": 1_536_000.0 }));
        let mut next = src(
            serde_json::json!({ "sample_rate_hz": 2_400_000.0, "bandwidth_hz": 600_000.0, "center_freq_hz": 1.0e6 }),
        );
        apply(&current, &mut next, &sdrplay_caps());
        assert_eq!(bw(&next), Some(600_000.0), "explicit caller BW must win");
    }

    #[test]
    fn apply_leaves_bw_on_center_only_retune() {
        // Same rate + same (valid) BW, only center moved -> BW untouched
        // (so a hot center retune isn't turned into a rebuild).
        let current = src(
            serde_json::json!({ "sample_rate_hz": 2_400_000.0, "bandwidth_hz": 1_536_000.0, "center_freq_hz": 1.0e6 }),
        );
        let mut next = src(
            serde_json::json!({ "sample_rate_hz": 2_400_000.0, "bandwidth_hz": 1_536_000.0, "center_freq_hz": 1.1e6 }),
        );
        apply(&current, &mut next, &sdrplay_caps());
        assert_eq!(bw(&next), Some(1_536_000.0));
    }

    #[test]
    fn apply_sets_notch_off_in_am() {
        let current =
            src(serde_json::json!({ "sample_rate_hz": 2_400_000.0, "center_freq_hz": 5.0e6 }));
        let mut next =
            src(serde_json::json!({ "sample_rate_hz": 2_400_000.0, "center_freq_hz": 1.0e6 }));
        apply(&current, &mut next, &sdrplay_caps());
        assert_eq!(notch(&next, "rfnotch_ctrl"), Some("false".into()));
        assert_eq!(notch(&next, "dabnotch_ctrl"), Some("true".into()));
    }

    #[test]
    fn apply_respects_explicit_notch_override() {
        // Caller flips rfnotch on this patch while tuned in AM -> kept.
        let current = src(
            serde_json::json!({ "center_freq_hz": 1.0e6, "settings": { "rfnotch_ctrl": "false" } }),
        );
        let mut next = src(
            serde_json::json!({ "center_freq_hz": 1.0e6, "settings": { "rfnotch_ctrl": "true" } }),
        );
        apply(&current, &mut next, &sdrplay_caps());
        assert_eq!(
            notch(&next, "rfnotch_ctrl"),
            Some("true".into()),
            "explicit notch override must win"
        );
    }

    #[test]
    fn apply_skips_non_sdrplay_notch_but_still_does_bw() {
        // HackRF: continuous BW range, no notches touched.
        let mut caps = caps_with_bw("hackrf", &[]);
        caps.rx_channels[0].bandwidth_ranges_hz = vec![RangeSpec {
            min: 1_750_000.0,
            max: 28_000_000.0,
            step: None,
        }];
        let current = src(serde_json::json!({ "sample_rate_hz": 8_000_000.0 }));
        let mut next =
            src(serde_json::json!({ "sample_rate_hz": 10_000_000.0, "center_freq_hz": 1.0e6 }));
        apply(&current, &mut next, &caps);
        assert_eq!(bw(&next), Some(10_000_000.0));
        assert_eq!(
            notch(&next, "rfnotch_ctrl"),
            None,
            "non-sdrplay drivers get no notch keys"
        );
    }
}
