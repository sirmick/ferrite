//! Single source of truth for per-driver SDR lookup tables.
//!
//! The IF-filter ladders below were historically hand-duplicated: the
//! web side carried them in `web/src/lib/controls/sdr-presets/<driver>
//! .json` (`if_filter_ladder_hz`) while this CLI kept its own `match`
//! that only knew `sdrplay` — so a HackRF rate change via `ferrite-ctl`
//! silently skipped the bandwidth the web UI would have set. That drift
//! class is exactly what this module removes: define each table once,
//! here, in Rust; the CLI reads it directly and the web copy is
//! generated from it.
//!
//! Regenerate the web artifact (the documented one-liner):
//!
//! ```text
//! cargo run -p ferrite-ctl -- gen-tables
//! ```
//!
//! CI guard: `tests::generated_web_ladders_match` fails if the
//! committed `web/src/lib/controls/if-filter-ladders.generated.json`
//! drifts from this table — it runs under the workspace `cargo test`
//! the Rust CI job already executes, so editing the table without
//! regenerating breaks the build.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Per-driver IF-filter ladders, in Hz, ascending. Keyed by the
/// lowercased SoapySDR `driver` short name. A driver absent here has no
/// known ladder — the web side falls back to the device probe and the
/// CLI leaves bandwidth untouched so the driver keeps what it had.
///
/// Ordering of this slice is the on-disk key order of the generated
/// JSON; keep it alphabetical so the artifact diff stays stable.
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

/// Largest IF-filter ladder entry ≤ `rate_hz` for the given driver,
/// matching the rule the web UI's `pickFromLadder` uses. `None` when
/// the driver has no known ladder (leave bandwidth alone) or when the
/// rate is below the smallest ladder entry.
pub fn recommended_bandwidth_for(driver: &str, rate_hz: f64) -> Option<f64> {
    let ladder = IF_FILTER_LADDERS
        .iter()
        .find(|(k, _)| *k == driver)
        .map(|(_, v)| *v)?;
    #[allow(clippy::cast_precision_loss)]
    ladder
        .iter()
        .rev()
        .find(|&&x| x as f64 <= rate_hz)
        .map(|&x| x as f64)
}

/// The exact byte content of the web-consumed artifact: a JSON object
/// mapping driver key → ascending Hz array, one driver per line, keys
/// in [`IF_FILTER_LADDERS`] order, trailing newline. Hand-rolled rather
/// than via `serde_json` so the on-disk shape is fully pinned (and so
/// the CI equality check is byte-exact, not formatter-dependent).
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

/// Absolute path to the generated web artifact, resolved from this
/// crate's manifest dir so it works regardless of the caller's CWD.
fn web_ladders_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../web/src/lib/controls/if-filter-ladders.generated.json")
}

/// `gen-tables` subcommand body: (re)write the web artifact from the
/// Rust source of truth and print where it landed.
pub fn write_web_ladders() -> Result<()> {
    let path = web_ladders_path();
    let body = web_ladders_json();
    std::fs::write(&path, &body)
        .with_context(|| format!("writing generated ladders to {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed artifact, inlined at compile time. Path is
    /// relative to this source file (`tools/ferrite-ctl/src/`).
    const COMMITTED: &str =
        include_str!("../../../web/src/lib/controls/if-filter-ladders.generated.json");

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
        // Mirrors the web side's `pickFromLadder`. Anything below the
        // smallest ladder entry returns None (no entry ≤ rate).
        assert_eq!(
            recommended_bandwidth_for("sdrplay", 10_000_000.0),
            Some(8_000_000.0)
        );
        assert_eq!(
            recommended_bandwidth_for("sdrplay", 8_000_000.0),
            Some(8_000_000.0)
        );
        assert_eq!(
            recommended_bandwidth_for("sdrplay", 6_000_000.0),
            Some(6_000_000.0)
        );
        assert_eq!(
            recommended_bandwidth_for("sdrplay", 2_000_000.0),
            Some(1_536_000.0)
        );
        assert_eq!(recommended_bandwidth_for("sdrplay", 100_000.0), None);
        // HackRF: previously the CLI had no arm for this and silently
        // skipped bandwidth — the drift this module fixes.
        assert_eq!(
            recommended_bandwidth_for("hackrf", 20_000_000.0),
            Some(20_000_000.0)
        );
        assert_eq!(
            recommended_bandwidth_for("hackrf", 8_500_000.0),
            Some(8_000_000.0)
        );
        // Unknown driver → no ladder → leave bandwidth alone.
        assert_eq!(recommended_bandwidth_for("rtlsdr", 2_000_000.0), None);
    }
}
