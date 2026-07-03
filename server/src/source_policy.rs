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

    // --- Sample rate: seed a device-open default, then clamp to the
    // per-driver usable ceiling. The default fills ONLY when the caller left
    // the rate implicit — a fresh `select_device` sends just `args`, so a
    // headless driver lands on the same rate the UI's `defaultsFor` picks
    // instead of the block default. (Every other path merges into the full
    // config first, so the rate is already present and the fill is a no-op.)
    // The clamp keeps a requested rate under the firmware-usable max
    // (SDRplay advertises 10.66 MS/s but `activateStream` fails above 10) for
    // every caller. Both run before the BW/notch derivation so those key off
    // the resolved rate.
    if !params.contains_key("sample_rate_hz") {
        if let Some(def) = ferrite_sdr_tables::default_sample_rate_for(&caps.driver_key) {
            params.insert("sample_rate_hz".into(), serde_json::json!(def));
        }
    }
    if let Some(rate) = params.get("sample_rate_hz").and_then(Value::as_f64) {
        if let Some(max) = ferrite_sdr_tables::max_sample_rate_for(&caps.driver_key) {
            if rate > max {
                params.insert("sample_rate_hz".into(), serde_json::json!(max));
            }
        }
    }

    // --- Anti-alias bandwidth: derive from the (possibly new) rate. ---
    if let Some(rate) = params.get("sample_rate_hz").and_then(Value::as_f64) {
        let new_bw = params.get("bandwidth_hz").and_then(Value::as_f64);
        let caller_set_bw = new_bw != cur_f64("bandwidth_hz"); // explicit this patch
        let rate_changed = cur_f64("sample_rate_hz") != Some(rate);
        // Stale-wide / unset: absent, the documented 0 "auto" sentinel, or
        // an IF wider than the digitizer (the 8 MHz-into-2.4 failure).
        let stale = new_bw.is_none_or(|b| b <= 0.0 || b > rate);
        if !caller_set_bw && (rate_changed || stale) {
            // Prefer the curated per-driver IF-filter ladder (the real
            // discrete filters the hardware has) over the device's advertised
            // ranges: HackRF advertises a *continuous* range, so a caps-only
            // pick would land on `rate` itself rather than the nearest real
            // filter. Fall back to caps for drivers with no ladder.
            let bw = ferrite_sdr_tables::recommended_bandwidth_for(&caps.driver_key, rate)
                .or_else(|| bandwidth_from_caps(caps, rate));
            if let Some(bw) = bw {
                if Some(bw) != new_bw {
                    params.insert("bandwidth_hz".into(), serde_json::json!(bw));
                }
            }
        }
    }

    // --- SDRplay broadcast notches: follow the tuned span. ---
    // Case-insensitive: the opened device reports driver_key "SDRplay",
    // while the Soapy args string uses lowercase `driver=sdrplay` — match
    // either so the notch policy actually fires headless.
    if caps.driver_key.eq_ignore_ascii_case("sdrplay") {
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

/// Per-driver DC-spike dodge ratio — the fraction of the channelizer's
/// output rate by which [`AppState::tune`] parks the source LO off the
/// listen target, so the zero-IF LO/DC spike falls *outside* the
/// demodulated channel and the channelizer recovers the carrier. This is
/// the daemon-owned default applied whenever the caller doesn't pass an
/// explicit `offset_ratio`, so the UI, ferrite-ctl, and a headless AI all
/// dodge identically off one table — the per-SDR behaviour lives here, not
/// in each client.
///
/// `0.7` clears a full-width channel (the spike at the LO sits a channel
/// half-width-plus outside the passband). `0` means "no dodge": DC-tracking
/// or low-IF drivers that don't produce an in-band spike, or correct it in
/// hardware. Keyed case-insensitively on the device-reported `driver_key`.
///
/// [`AppState::tune`]: crate::app_state::AppState::tune
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

/// The device-side writes a tune resolves to. `AppState::tune` turns each
/// present field into a `patch_source` (LO/rate) or `apply_block_params`
/// (channelizer shift) call. All `None` = nothing to do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TunePlan {
    /// New source LO centre (Hz), or `None` to leave the LO where it is.
    /// Suppressed by a 0.5 Hz floor so readback jitter doesn't churn the
    /// graph — but always `Some` on the first tune (unknown centre).
    pub src_center_hz: Option<f64>,
    /// New source sample rate (Hz) when widening the span; `None` to keep
    /// the current rate.
    pub src_rate_hz: Option<f64>,
    /// Channelizer `freq_shift_hz` to apply; `None` when the graph has no
    /// channelizer (the LO alone carries the tune).
    pub chan_shift_hz: Option<f64>,
}

/// Pure DC-spike dodge / keep-out / DC-guard math behind [`AppState::tune`].
///
/// Given the target `freq_hz`, the per-driver `offset_ratio` (0 = no
/// dodge), the current source LO centre (`cur_src_center`, `None` = never
/// written) and rate (`cur_rate`), and the first channelizer's output rate
/// (`chan_rate`, `None` = no channelizer), decide what to write.
///
/// - **In-span fast path** (`keep_lo`): if the target already falls inside
///   the digitised span (half the rate, less half the channel width), move
///   the *channelizer only* and leave the LO + graph untouched — no source
///   restart. Ignores `span_hz`.
/// - **Sticky**: LO already parked such that the target sits within the
///   channel keep-out (`0.4·chan_rate`) and at least the DC-guard off it
///   (`0.1·chan_rate` when dodging, else 0) — reuse the LO, just re-shift.
/// - **Snap** (first tune / unknown centre / on-DC): park the LO low of
///   target by `offset_ratio·chan_rate` and let the channelizer recover
///   the carrier off the DC spike.
///
/// [`AppState::tune`]: crate::app_state::AppState::tune
#[must_use]
pub fn plan_tune(
    freq_hz: f64,
    span_hz: Option<f64>,
    offset_ratio: f64,
    keep_lo: bool,
    cur_src_center: Option<f64>,
    cur_rate: f64,
    chan_rate: Option<f64>,
) -> TunePlan {
    // In-span fast path: retune the channelizer, leave the LO + rate alone.
    if keep_lo {
        if let (Some(cur), Some(cr)) = (cur_src_center, chan_rate) {
            let usable_half = cur_rate / 2.0 - cr / 2.0;
            if cur_rate > 0.0 && usable_half > 0.0 && (freq_hz - cur).abs() <= usable_half {
                return TunePlan {
                    src_center_hz: None,
                    src_rate_hz: None,
                    chan_shift_hz: Some(freq_hz - cur),
                };
            }
        }
    }

    // Normal LO-moving path: dodge/keep-out/DC-guard.
    let (new_src_center, new_freq_shift) = match chan_rate {
        Some(cr) => {
            let keepout = 0.4 * cr;
            let dodge = offset_ratio * cr;
            let dc_guard = if offset_ratio > 0.0 { 0.1 * cr } else { 0.0 };
            match cur_src_center {
                Some(cur)
                    if {
                        let off = (freq_hz - cur).abs();
                        off <= keepout && off >= dc_guard
                    } =>
                {
                    (cur, freq_hz - cur)
                }
                _ => (freq_hz - dodge, dodge),
            }
        }
        None => (freq_hz, 0.0),
    };

    let center_changed = cur_src_center.is_none_or(|cur| (new_src_center - cur).abs() > 0.5);
    TunePlan {
        src_center_hz: center_changed.then_some(new_src_center),
        src_rate_hz: span_hz.filter(|s| *s > cur_rate),
        chan_shift_hz: chan_rate.map(|_| new_freq_shift),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{DeviceCapabilities, DeviceInfo, RangeSpec, RxChannelCapabilities};

    #[test]
    fn tune_offset_ratio_is_per_driver_and_case_insensitive() {
        assert_eq!(tune_offset_ratio_for("hackrf"), 0.7);
        assert_eq!(tune_offset_ratio_for("HackRF"), 0.7);
        assert_eq!(tune_offset_ratio_for("sdrplay"), 0.7);
        assert_eq!(tune_offset_ratio_for("SDRplay"), 0.7);
        // DC-tracking / unknown drivers don't dodge.
        assert_eq!(tune_offset_ratio_for("rtlsdr"), 0.0);
        assert_eq!(tune_offset_ratio_for("airspy"), 0.0);
        assert_eq!(tune_offset_ratio_for(""), 0.0);
    }

    // ── plan_tune ──────────────────────────────────────────────────────
    // chan_rate = 48 kHz, dodge ratio 0.7 (hackrf/sdrplay) → dodge = 33.6k,
    // keepout = 19.2k, dc_guard = 4.8k.
    const CR: f64 = 48_000.0;

    #[test]
    fn snap_on_first_tune_parks_lo_low_of_target_by_the_dodge() {
        // Unknown centre (fresh device) → always snap + always write.
        let p = plan_tune(100.0e6, None, 0.7, false, None, 2.4e6, Some(CR));
        let dodge = 0.7 * CR;
        assert_eq!(p.src_center_hz, Some(100.0e6 - dodge));
        assert_eq!(p.chan_shift_hz, Some(dodge));
        assert_eq!(p.src_rate_hz, None);
    }

    #[test]
    fn sticky_reuses_lo_when_target_inside_keepout_and_off_dc() {
        // LO parked at 100M-33.6k (the snap point); a new target 10 kHz up
        // sits within keepout (19.2k) and past the DC guard (4.8k) → reuse.
        let lo = 100.0e6 - 0.7 * CR;
        let target = lo + 10_000.0;
        let p = plan_tune(target, None, 0.7, false, Some(lo), 2.4e6, Some(CR));
        assert_eq!(p.src_center_hz, None, "LO must not move");
        assert_eq!(p.chan_shift_hz, Some(target - lo));
    }

    #[test]
    fn on_dc_target_re_snaps_instead_of_parking_on_the_spike() {
        // Target within 2 kHz of the LO — below the 4.8k DC guard. Sticky
        // would park it on the DC spike; must re-snap off it instead.
        let lo = 100.0e6;
        let target = lo + 2_000.0;
        let p = plan_tune(target, None, 0.7, false, Some(lo), 2.4e6, Some(CR));
        let dodge = 0.7 * CR;
        assert_eq!(p.src_center_hz, Some(target - dodge), "must re-snap off DC");
        assert_eq!(p.chan_shift_hz, Some(dodge));
    }

    #[test]
    fn dc_tracking_driver_may_sit_on_centre() {
        // offset_ratio 0 → dodge 0, dc_guard 0. Target exactly on the LO is
        // allowed (off=0 ≥ guard=0, ≤ keepout) → sticky, zero shift.
        let lo = 100.0e6;
        let p = plan_tune(lo, None, 0.0, false, Some(lo), 2.4e6, Some(CR));
        assert_eq!(p.src_center_hz, None);
        assert_eq!(p.chan_shift_hz, Some(0.0));
    }

    #[test]
    fn in_span_fast_path_moves_channelizer_only() {
        // keep_lo + target inside the usable half-span (1.2M - 24k) → LO and
        // rate untouched, channelizer takes the whole shift.
        let lo = 100.0e6;
        let target = lo + 500_000.0;
        let p = plan_tune(target, Some(4.0e6), 0.7, true, Some(lo), 2.4e6, Some(CR));
        assert_eq!(p.src_center_hz, None, "fast path leaves the LO put");
        assert_eq!(p.src_rate_hz, None, "fast path ignores span widening");
        assert_eq!(p.chan_shift_hz, Some(target - lo));
    }

    #[test]
    fn out_of_span_keep_lo_falls_through_to_normal_snap() {
        // keep_lo but target beyond the usable half-span → normal LO move.
        let lo = 100.0e6;
        let target = lo + 2_000_000.0; // > 1.2M - 24k
        let p = plan_tune(target, None, 0.7, true, Some(lo), 2.4e6, Some(CR));
        assert_eq!(p.src_center_hz, Some(target - 0.7 * CR), "LO must snap");
    }

    #[test]
    fn no_channelizer_writes_lo_directly() {
        let p = plan_tune(100.0e6, None, 0.7, false, None, 2.4e6, None);
        assert_eq!(
            p.src_center_hz,
            Some(100.0e6),
            "LO carries the tune, no dodge"
        );
        assert_eq!(p.chan_shift_hz, None);
    }

    #[test]
    fn span_widens_rate_only_when_larger() {
        // span > cur_rate → set rate.
        let p = plan_tune(100.0e6, Some(4.0e6), 0.7, false, None, 2.4e6, Some(CR));
        assert_eq!(p.src_rate_hz, Some(4.0e6));
        // span ≤ cur_rate → leave rate.
        let p = plan_tune(100.0e6, Some(1.0e6), 0.7, false, None, 2.4e6, Some(CR));
        assert_eq!(p.src_rate_hz, None);
    }

    #[test]
    fn sub_half_hz_lo_move_is_suppressed() {
        // A recomputed LO within 0.5 Hz of the current one must not churn
        // the graph — but only sticky can produce that (snap moves by dodge).
        let lo = 100.0e6;
        // Target on the LO with a DC-tracking driver → sticky, LO unchanged.
        let p = plan_tune(lo, None, 0.0, false, Some(lo), 2.4e6, Some(CR));
        assert_eq!(p.src_center_hz, None);
    }

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
        // Real casing the opened device reports — guards the case-insensitive
        // driver match (the Soapy args string is lowercase `driver=sdrplay`).
        caps_with_bw("SDRplay", SDRPLAY_LADDER)
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

    fn rate(cfg: &SourceConfig) -> Option<f64> {
        cfg.params.get("sample_rate_hz").and_then(|v| v.as_f64())
    }

    #[test]
    fn apply_clamps_rate_to_driver_ceiling() {
        // SDRplay advertises 10.66 MS/s but the firmware streams ≤ 10.
        let current = src(serde_json::json!({ "sample_rate_hz": 2_000_000.0 }));
        let mut next =
            src(serde_json::json!({ "sample_rate_hz": 10_660_000.0, "center_freq_hz": 100.0e6 }));
        apply(&current, &mut next, &sdrplay_caps());
        assert_eq!(rate(&next), Some(10_000_000.0));
    }

    #[test]
    fn apply_fills_default_rate_on_bare_device_select() {
        // A headless `select_device` sends just `args` — no rate. The daemon
        // seeds the per-driver default so it matches the UI's `defaultsFor`.
        let current = src(serde_json::json!({}));
        let mut next = src(serde_json::json!({ "args": "driver=sdrplay" }));
        apply(&current, &mut next, &sdrplay_caps());
        assert_eq!(rate(&next), Some(2_000_000.0));
    }

    #[test]
    fn apply_leaves_present_sub_ceiling_rate_untouched() {
        // A normal patch already carries the rate; the fill is a no-op and a
        // sub-ceiling rate isn't clamped.
        let current = src(serde_json::json!({ "sample_rate_hz": 2_000_000.0 }));
        let mut next =
            src(serde_json::json!({ "sample_rate_hz": 6_000_000.0, "center_freq_hz": 100.0e6 }));
        apply(&current, &mut next, &sdrplay_caps());
        assert_eq!(rate(&next), Some(6_000_000.0));
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
