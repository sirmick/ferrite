//! Time × frequency of a slice — the picture that tells modes apart by
//! eye (RTTY: two alternating stripes; Olivia: hopping tones; CW: dashes
//! on one line; PSK: one thin steady line).

use anyhow::{anyhow, Result};
use num_complex::Complex;
use serde::{Deserialize, Serialize};

use crate::db;
use crate::slice::Slice;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct SpectrogramParams {
    pub lo_hz: f64,
    pub hi_hz: f64,
    /// Target frequency resolution, Hz per column (default 10).
    pub res_hz: f64,
    /// Time step between rows, ms (default 20).
    pub step_ms: f64,
    /// Pool down to at most this many rows / columns.
    pub max_rows: usize,
    pub max_cols: usize,
}

impl SpectrogramParams {
    #[must_use]
    pub fn new(lo_hz: f64, hi_hz: f64) -> Self {
        Self {
            lo_hz,
            hi_hz,
            res_hz: 10.0,
            step_ms: 20.0,
            max_rows: 400,
            max_cols: 400,
        }
    }
}

/// Rows are time (oldest first), columns frequency (lowest first).
/// `cells` are dB quantised to 0..=255 between `db_lo` and `db_hi`.
#[derive(Debug, Clone, Serialize)]
pub struct Spectrogram {
    /// RF frequency of column 0's centre.
    pub f0_hz: f64,
    /// Hz per column (after pooling).
    pub df_hz: f64,
    /// Seconds per row (after pooling).
    pub dt_s: f64,
    pub rows: usize,
    pub cols: usize,
    pub db_lo: f32,
    pub db_hi: f32,
    /// Median cell power, dBFS.
    pub floor_dbfs: f32,
    /// Resolution actually used by the FFT, Hz.
    pub fft_res_hz: f64,
    #[serde(skip)]
    pub cells: Vec<u8>,
}

pub fn spectrogram(sl: &Slice, p: SpectrogramParams) -> Result<Spectrogram> {
    let half = sl.rate_hz / 2.0;
    if p.lo_hz < sl.center_hz - half || p.hi_hz > sl.center_hz + half || !(p.hi_hz > p.lo_hz) {
        return Err(anyhow!(
            "range {:.0}–{:.0} Hz is outside the slice {:.0}–{:.0} Hz",
            p.lo_hz,
            p.hi_hz,
            sl.center_hz - half,
            sl.center_hz + half
        ));
    }
    let n = ((sl.rate_hz / p.res_hz.max(0.5)) as usize)
        .next_power_of_two()
        .clamp(16, 1 << 16);
    let res = sl.rate_hz / n as f64;
    let hop = ((p.step_ms / 1000.0 * sl.rate_hz) as usize).max(1);
    if sl.samples.len() < n {
        return Err(anyhow!(
            "slice too short for a {n}-point FFT ({:.2} s) — coarser res_hz or a longer snapshot",
            n as f64 / sl.rate_hz
        ));
    }
    let to_bin = |f: f64| ((f - sl.center_hz) / res).round() as i64;
    let (b_lo, b_hi) = (to_bin(p.lo_hz), to_bin(p.hi_hz));
    let bins: Vec<usize> = (b_lo..=b_hi)
        .map(|b| b.rem_euclid(n as i64) as usize)
        .collect();

    let win: Vec<f32> = (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos())
        .collect();
    let wsum: f32 = win.iter().sum();
    let norm = 1.0 / (wsum * wsum);
    let fft = rustfft::FftPlanner::<f32>::new().plan_fft_forward(n);
    let mut scratch = vec![Complex::new(0.0f32, 0.0); n];
    let mut raw: Vec<Vec<f32>> = Vec::new();
    let mut s = 0;
    while s + n <= sl.samples.len() {
        for i in 0..n {
            scratch[i] = sl.samples[s + i] * win[i];
        }
        fft.process(&mut scratch);
        raw.push(
            bins.iter()
                .map(|&k| db(scratch[k].norm_sqr() * norm))
                .collect(),
        );
        s += hop;
    }

    // Pool (max) to the size limits.
    let (r0, c0) = (raw.len(), bins.len());
    let rp = r0.div_ceil(p.max_rows.max(1));
    let cp = c0.div_ceil(p.max_cols.max(1));
    let (rows, cols) = (r0.div_ceil(rp), c0.div_ceil(cp));
    let mut pooled = vec![f32::MIN; rows * cols];
    for (ri, row) in raw.iter().enumerate() {
        for (ci, &v) in row.iter().enumerate() {
            let cell = &mut pooled[(ri / rp) * cols + ci / cp];
            *cell = cell.max(v);
        }
    }
    let mut sorted = pooled.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[sorted.len() / 2];
    let top = sorted[(sorted.len() * 999 / 1000).min(sorted.len() - 1)];
    let (db_lo, db_hi) = (floor - 3.0, top.max(floor + 20.0));
    let cells = pooled
        .iter()
        .map(|&v| (((v - db_lo) / (db_hi - db_lo)).clamp(0.0, 1.0) * 255.0).round() as u8)
        .collect();
    Ok(Spectrogram {
        f0_hz: sl.center_hz + b_lo as f64 * res + (cp as f64 - 1.0) * res / 2.0,
        df_hz: res * cp as f64,
        dt_s: hop as f64 / sl.rate_hz * rp as f64,
        rows,
        cols,
        db_lo,
        db_hi,
        floor_dbfs: floor,
        fft_res_hz: res,
        cells,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two alternating tones (FSK) → two columns that take turns.
    #[test]
    fn fsk_shows_two_alternating_stripes() {
        let rate = 2_000.0;
        let n = (rate * 4.0) as usize;
        let samples: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f64 / rate;
                let f = if ((t / 0.022) as u64).is_multiple_of(2) {
                    -85.0
                } else {
                    85.0
                };
                let ph = 2.0 * std::f64::consts::PI * f * t;
                Complex::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let sl = Slice {
            center_hz: 7.0e6,
            rate_hz: rate,
            samples,
        };
        let sg = spectrogram(&sl, SpectrogramParams::new(7.0e6 - 300.0, 7.0e6 + 300.0)).unwrap();
        let col = |f: f64| ((f - sg.f0_hz) / sg.df_hz).round() as usize;
        let (a, b, mid) = (col(7.0e6 - 85.0), col(7.0e6 + 85.0), col(7.0e6));
        let mean = |c: usize| {
            (0..sg.rows)
                .map(|r| f64::from(sg.cells[r * sg.cols + c]))
                .sum::<f64>()
                / sg.rows as f64
        };
        assert!(
            mean(a) > mean(mid) + 40.0 && mean(b) > mean(mid) + 40.0,
            "{} {} {}",
            mean(a),
            mean(b),
            mean(mid)
        );
    }
}
