//! Frequency-allocation lookup.
//!
//! Embeds `web/src/lib/presets/bandplan-usa.json` (Arrin Clark
//! [KN1E]'s SDR-Band-Plans, CC0) at compile time so the AI's
//! `band_at(freq_hz)` MCP tool can answer "what's this frequency
//! allocated to?" without a side-channel data fetch. Same source of
//! truth the UI's band ribbon uses.
//!
//! Lookup is linear over ~1.2 k entries; one `band_at` call costs a
//! handful of microseconds. A sorted-+-binary-search variant would
//! be tidier but the wall-clock case is invisible; not optimising
//! until the call site is in a hot loop.
//!
//! The JSON's `start` / `end` are nominal Hz integers (e.g. 88_000_000
//! for the bottom of FM broadcast). `type` is the upstream ribbon's
//! RGBA-hex colour code; we surface it verbatim so callers that want
//! to render a ribbon strip can.

use serde::{Deserialize, Serialize};

/// Verbatim copy of the upstream JSON. `bands` is the flat list of
/// allocations; an FFT bin can land in multiple overlapping entries
/// (broadcast inside a primary allocation, then a guard sub-band,
/// etc.) so `band_at` returns every match rather than picking one.
#[derive(Debug, Deserialize)]
struct Doc {
    bands: Vec<BandEntry>,
}

/// One row of the band plan. `start` / `end` are inclusive of `start`,
/// exclusive of `end` (matches the JSON's open-right convention —
/// adjacent rows have `prev.end == next.start`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandEntry {
    pub name: String,
    /// Upstream colour code (RGBA hex without `#` prefix). Surfaced
    /// for ribbon-rendering callers; AI clients can ignore.
    #[serde(rename = "type")]
    pub kind: String,
    pub start: f64,
    pub end: f64,
}

const BAND_PLAN_JSON: &str = include_str!("../../web/src/lib/presets/bandplan-usa.json");

fn band_plan() -> &'static [BandEntry] {
    static CELL: std::sync::OnceLock<Vec<BandEntry>> = std::sync::OnceLock::new();
    CELL.get_or_init(|| {
        let doc: Doc =
            serde_json::from_str(BAND_PLAN_JSON).expect("compile-time bandplan-usa.json is valid");
        doc.bands
    })
    .as_slice()
}

/// Every band whose `[start, end)` window contains `hz`. Multiple
/// matches are common — a single FFT bin sits inside a primary
/// allocation AND a sub-band carving inside it. Caller chooses;
/// usually you want the *narrowest* match for naming and the *whole*
/// list for context.
#[must_use]
pub fn at(hz: f64) -> Vec<BandEntry> {
    band_plan()
        .iter()
        .filter(|b| hz >= b.start && hz < b.end)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sanity-check: 162.4 MHz (NOAA WX) hits at least one named US
    /// allocation. Plan is large (~1.2k rows) so we can't pin the
    /// exact name without rewriting the test every time upstream
    /// reshuffles — `is_empty()` is enough to catch a parse failure
    /// or a coord-system regression (e.g. someone reading Hz as MHz).
    #[test]
    fn nwr_freq_lands_in_a_band() {
        let bands = at(162_400_000.0);
        assert!(
            !bands.is_empty(),
            "162.4 MHz should match at least one band entry"
        );
    }

    #[test]
    fn out_of_range_returns_empty() {
        // 100 THz is well past anyone's allocation chart.
        let bands = at(1.0e14);
        assert!(bands.is_empty());
    }
}
