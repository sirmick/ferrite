//! Spectrum-render helpers — pure functions, native + WASM.
//!
//! These are *not* [`Block`]s. They're the last-mile CPU math the
//! browser's `SpectrumRenderer` calls before drawing into the canvas:
//!
//!   * [`collapse_row_to_columns`] — max-per-pixel-column reduction when
//!     the FFT has more bins than the plot is wide. Draws a clean
//!     envelope instead of a lineTo-per-bin "comb".
//!   * [`update_max_hold`] — elementwise max into an accumulator, used
//!     by the spectrum's max-hold overlay.
//!   * [`compute_spectrum_stats`] — p10 / p99 of a row's byte values,
//!     emitted for the client-side auto-floor loop.
//!
//! Kept here rather than as blocks because the output isn't a stream
//! anyone else in the graph consumes — it feeds directly into a
//! `moveTo`/`lineTo` sequence on Canvas 2D. Pure functions are easier
//! to test and compose than scheduler-owned blocks when the output has
//! no downstream pluggability.
//!
//! [`Block`]: crate::Block

#[cfg(feature = "wasm")]
use wasm_bindgen::prelude::*;

/// Max-reduce `row` into `out`, one pixel column per output slot.
///
/// `out.len()` is the number of pixel columns the renderer has space
/// for. Every bin in `row` maps to exactly one column; the column's
/// value is the **max** of all bins that fall into it. When `row.len()
/// <= out.len()` the call is effectively a copy with the tail zero-
/// filled; callers in that regime should skip the collapse and draw
/// bins directly — exposing the branch here anyway keeps the TS
/// integration boring.
///
/// Partitioning matches the prior TS logic: bin `b` lands in column
/// `floor(b * out_px / n)`, where `n = row.len()` and `out_px =
/// out.len()`.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
pub fn collapse_row_to_columns(row: &[u8], out: &mut [u8]) {
    let n = row.len();
    let out_px = out.len();
    if out_px == 0 {
        return;
    }
    for slot in out.iter_mut() {
        *slot = 0;
    }
    if n == 0 {
        return;
    }
    if n <= out_px {
        // Not enough bins to saturate every column; copy what we have,
        // leave the tail at zero. The TS caller takes the non-collapse
        // branch for `n <= out_px`, but supporting it here means the
        // Rust fn is total.
        out[..n].copy_from_slice(row);
        return;
    }
    // n > out_px: max-reduce. Compute boundaries with u64 to keep the
    // index math honest at 16k bins × 2k-pixel plots on a 32-bit wasm
    // target.
    for (col, slot) in out.iter_mut().enumerate() {
        let b0 = (col as u64 * n as u64 / out_px as u64) as usize;
        let b1_raw = ((col as u64 + 1) * n as u64 / out_px as u64) as usize;
        let b1 = b1_raw.min(n).max(b0 + 1);
        let mut m = 0u8;
        for &v in &row[b0..b1] {
            if v > m {
                m = v;
            }
        }
        *slot = m;
    }
}

/// Elementwise `acc[i] = max(acc[i], new[i])`. `acc.len()` and
/// `new.len()` must match; callers ensure that (a TS mismatch just
/// results in the shorter of the two being processed).
#[cfg_attr(feature = "wasm", wasm_bindgen)]
pub fn update_max_hold(acc: &mut [u8], new: &[u8]) {
    let n = acc.len().min(new.len());
    for i in 0..n {
        if new[i] > acc[i] {
            acc[i] = new[i];
        }
    }
}

/// 10th / 99th percentile of a row's byte values, returned as a tuple-
/// like struct so wasm-bindgen can hand it back to JS without an
/// intermediate `Vec`.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpectrumStats {
    pub p10: u8,
    pub p99: u8,
}

/// Histogram-based p10/p99. Matches the TS `computeStats` exactly:
/// thresholds are `max(1, floor(n * 0.1))` and `max(1, floor(n * 0.99))`,
/// walked in ascending byte order; the first byte whose cumulative
/// count clears the threshold is the percentile. Empty rows return
/// `{ p10: 0, p99: 255 }` to keep the auto-floor loop from clamping
/// on a cold start.
#[cfg_attr(feature = "wasm", wasm_bindgen)]
pub fn compute_spectrum_stats(row: &[u8]) -> SpectrumStats {
    if row.is_empty() {
        return SpectrumStats { p10: 0, p99: 255 };
    }
    let mut hist = [0u32; 256];
    for &v in row {
        hist[v as usize] += 1;
    }
    let n = row.len() as u64;
    let t10 = ((n as f64 * 0.1) as u64).max(1);
    let t99 = ((n as f64 * 0.99) as u64).max(1);
    let mut cum = 0u64;
    let mut p10 = 0u8;
    let mut p99 = 255u8;
    let mut seen10 = false;
    for (v, &h) in hist.iter().enumerate() {
        cum += h as u64;
        if !seen10 && cum >= t10 {
            p10 = v as u8;
            seen10 = true;
        }
        if cum >= t99 {
            p99 = v as u8;
            break;
        }
    }
    SpectrumStats { p10, p99 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_copies_when_row_fits() {
        let row = [10u8, 20, 30, 40];
        let mut out = [0u8; 8];
        collapse_row_to_columns(&row, &mut out);
        assert_eq!(&out, &[10, 20, 30, 40, 0, 0, 0, 0]);
    }

    #[test]
    fn collapse_max_reduces_when_row_exceeds_columns() {
        // 16 bins → 4 columns, each column spans 4 bins. Max in each
        // 4-wide group:  [3,2,1,0] [7,6,5,4] [11,10,9,8] [15,14,13,12]
        let row: [u8; 16] = [3, 2, 1, 0, 7, 6, 5, 4, 11, 10, 9, 8, 15, 14, 13, 12];
        let mut out = [0u8; 4];
        collapse_row_to_columns(&row, &mut out);
        assert_eq!(&out, &[3, 7, 11, 15]);
    }

    #[test]
    fn collapse_single_peak_survives_wide_reduction() {
        // One sharp peak in the middle; ensure it's preserved in its
        // column even at a >>1 reduction factor. Prior TS bug shape
        // would lose peaks if the wrong rounding dropped a bin.
        let mut row = vec![0u8; 16384];
        row[8_000] = 250;
        let mut out = vec![0u8; 1024];
        collapse_row_to_columns(&row, &mut out);
        // 16384 bins / 1024 cols = 16 bins/col; bin 8000 lands in
        // col 500.
        assert_eq!(out[500], 250, "peak must land in exactly one column");
        // And nowhere else.
        assert!(out.iter().filter(|&&v| v == 250).count() == 1);
    }

    #[test]
    fn collapse_covers_every_bin() {
        // Regression guard against off-by-one in the boundary math:
        // every bin must be inspected for its column's max. Encode an
        // all-ones row and verify every output column is 1, including
        // the last.
        let row = vec![1u8; 777];
        let mut out = vec![0u8; 100];
        collapse_row_to_columns(&row, &mut out);
        assert!(out.iter().all(|&v| v == 1));
    }

    #[test]
    fn collapse_handles_empty_row() {
        let row: [u8; 0] = [];
        let mut out = [0u8; 8];
        collapse_row_to_columns(&row, &mut out);
        assert_eq!(&out, &[0; 8]);
    }

    #[test]
    fn collapse_handles_empty_out() {
        let row = [1u8, 2, 3];
        let mut out: [u8; 0] = [];
        collapse_row_to_columns(&row, &mut out);
        // No panic, no work done.
    }

    #[test]
    fn max_hold_monotone_nondecreasing() {
        let mut acc = [0u8; 4];
        update_max_hold(&mut acc, &[10, 20, 30, 40]);
        assert_eq!(&acc, &[10, 20, 30, 40]);
        update_max_hold(&mut acc, &[5, 25, 15, 50]);
        assert_eq!(&acc, &[10, 25, 30, 50]);
    }

    #[test]
    fn stats_empty_row_returns_safe_default() {
        let s = compute_spectrum_stats(&[]);
        assert_eq!(s, SpectrumStats { p10: 0, p99: 255 });
    }

    #[test]
    fn stats_uniform_row_pins_percentiles() {
        // All bytes == 128; both percentiles must be 128.
        let row = [128u8; 100];
        let s = compute_spectrum_stats(&row);
        assert_eq!(s.p10, 128);
        assert_eq!(s.p99, 128);
    }

    #[test]
    fn stats_ramp_matches_ts_thresholds() {
        // Ramp 0..100. With n=100, t10 = 10, t99 = 99.
        // Walking bytes ascending: cumulative hits 10 at byte value 9
        // (10 bytes 0..9), hits 99 at byte value 98.
        let row: Vec<u8> = (0..100).collect();
        let s = compute_spectrum_stats(&row);
        assert_eq!(s.p10, 9);
        assert_eq!(s.p99, 98);
    }
}
