//! Power at one frequency over time → on/off segments. The Morse-by-eye
//! plot, beacon slot timing, burst detection.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::db;
use crate::slice::Slice;

/// Below this p90−p10 swing the envelope is "steady", not on/off.
const STEADY_SWING_DB: f32 = 6.0;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct EnvelopeParams {
    /// Integration window, ms (default 10 — a 40 WPM dit is 30 ms).
    pub window_ms: f64,
}

impl Default for EnvelopeParams {
    fn default() -> Self {
        Self { window_ms: 10.0 }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq)]
pub struct Segment {
    /// Seconds from the snapshot start.
    pub start_s: f64,
    pub end_s: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    pub freq_hz: f64,
    pub window_s: f64,
    /// Power per window, dBFS.
    pub power_db: Vec<f32>,
    /// On/off decision level (geometric midpoint of p10 and p90), dBFS.
    pub threshold_db: f32,
    /// p90 − p10, dB. Noise alone swings ~13 dB.
    pub swing_db: f32,
    pub keyed: bool,
    /// Fraction of windows on.
    pub duty: f32,
    pub segments: Vec<Segment>,
}

/// Envelope of a slice centred on the signal (`sl.center_hz`): the
/// slice's own band-limit sets the bandwidth.
pub fn envelope(sl: &Slice, p: EnvelopeParams) -> Result<Envelope> {
    let w = ((p.window_ms / 1000.0 * sl.rate_hz).round() as usize).max(1);
    if sl.samples.len() < w * 10 {
        return Err(anyhow!(
            "slice too short for an envelope at {} ms windows",
            p.window_ms
        ));
    }
    let power_db: Vec<f32> = sl
        .samples
        .chunks_exact(w)
        .map(|c| db(c.iter().map(num_complex::Complex::norm_sqr).sum::<f32>() / w as f32))
        .collect();
    let mut sorted = power_db.clone();
    sorted.sort_by(f32::total_cmp);
    let p10 = sorted[sorted.len() / 10];
    let p90 = sorted[sorted.len() * 9 / 10];
    let swing = p90 - p10;
    // A steady signal (or steady noise) barely swings: a midpoint split
    // would chop it into random on/off. Call it continuously on.
    let threshold = if swing < STEADY_SWING_DB {
        p10 - 3.0
    } else {
        (p10 + p90) / 2.0
    };
    let window_s = w as f64 / sl.rate_hz;
    let mut segments = Vec::new();
    let mut start: Option<usize> = None;
    for (i, &v) in power_db.iter().enumerate() {
        match (v > threshold, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                segments.push(Segment {
                    start_s: s as f64 * window_s,
                    end_s: i as f64 * window_s,
                });
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        segments.push(Segment {
            start_s: s as f64 * window_s,
            end_s: power_db.len() as f64 * window_s,
        });
    }
    let on = power_db.iter().filter(|&&v| v > threshold).count() as f32 / power_db.len() as f32;
    Ok(Envelope {
        freq_hz: sl.center_hz,
        window_s,
        threshold_db: threshold,
        swing_db: swing,
        keyed: swing > 18.0 && on > 0.03 && on < 0.95,
        duty: on,
        segments,
        power_db,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_complex::Complex;

    #[test]
    fn a_steady_tone_is_one_segment_not_random_chops() {
        let rate = 1_000.0;
        let samples: Vec<Complex<f32>> = (0..2_000).map(|_| Complex::new(1.0, 0.0)).collect();
        let sl = Slice {
            center_hz: 0.0,
            rate_hz: rate,
            samples,
        };
        let e = envelope(&sl, EnvelopeParams::default()).unwrap();
        assert!(!e.keyed);
        assert_eq!(e.segments.len(), 1);
        assert!((e.duty - 1.0).abs() < 1e-6);
    }

    #[test]
    fn keyed_tone_gives_segments_at_the_right_times() {
        let rate = 1_000.0;
        // On 0.2–0.5 s and 1.0–1.1 s, else weak noise.
        let n = (rate * 2.0) as usize;
        let mut seed: u32 = 7;
        let samples: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let nz = (seed as f32 / u32::MAX as f32 - 0.5) * 0.01;
                let t = i as f64 / rate;
                let on = (0.2..0.5).contains(&t) || (1.0..1.1).contains(&t);
                Complex::new(if on { 1.0 } else { 0.0 } + nz, nz)
            })
            .collect();
        let sl = Slice {
            center_hz: 14.1e6,
            rate_hz: rate,
            samples,
        };
        let e = envelope(&sl, EnvelopeParams::default()).unwrap();
        assert!(e.keyed);
        assert_eq!(e.segments.len(), 2, "{:?}", e.segments);
        assert!((e.segments[0].start_s - 0.2).abs() < 0.011);
        assert!((e.segments[0].end_s - 0.5).abs() < 0.011);
        assert!((e.segments[1].start_s - 1.0).abs() < 0.011);
        assert!((e.duty - 0.2).abs() < 0.02);
    }
}
