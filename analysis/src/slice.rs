//! Mix one frequency range to baseband and decimate it — the narrow
//! working signal for the spectrogram, envelope and (later) classify.

use std::io::Read;

use anyhow::{anyhow, Result};
use num_complex::Complex;
use serde::Serialize;

use crate::iq::{for_each_block, IqMeta};

/// A decimated baseband slice of a snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct Slice {
    /// RF frequency at the slice's DC.
    pub center_hz: f64,
    pub rate_hz: f64,
    #[serde(skip)]
    pub samples: Vec<Complex<f32>>,
}

/// Mix `[lo_hz, hi_hz]` down to DC and decimate by the largest integer
/// that keeps ~25 % guard band, through a Blackman-windowed sinc FIR
/// (only the kept outputs are computed). Errors if the range isn't
/// inside the snapshot's span.
pub fn slice<R: Read>(reader: R, meta: IqMeta, lo_hz: f64, hi_hz: f64) -> Result<Slice> {
    let half = meta.rate_hz / 2.0;
    if !(hi_hz > lo_hz) || lo_hz < meta.center_hz - half || hi_hz > meta.center_hz + half {
        return Err(anyhow!(
            "range {lo_hz:.0}–{hi_hz:.0} Hz is outside the snapshot span {:.0}–{:.0} Hz",
            meta.center_hz - half,
            meta.center_hz + half
        ));
    }
    let bw = hi_hz - lo_hz;
    let center = (lo_hz + hi_hz) / 2.0;
    let decim = ((meta.rate_hz / (bw * 1.25)).floor() as usize).max(1);
    let rate_out = meta.rate_hz / decim as f64;

    // Low-pass at the slice half-width (+10 %), normalized to the input rate.
    let cutoff = ((bw / 2.0) * 1.1 / meta.rate_hz).min(0.49);
    let ntaps = (8 * decim + 1).min(16_001) | 1;
    let taps = lowpass(ntaps, cutoff);

    let step = Complex::from_polar(
        1.0f64,
        -2.0 * std::f64::consts::PI * (center - meta.center_hz) / meta.rate_hz,
    );
    let mut osc = Complex::new(1.0f64, 0.0);
    let mut hist = vec![Complex::new(0.0f32, 0.0); ntaps];
    let mut pos = 0usize; // next write index in `hist` (circular)
    let mut since_out = 0usize;
    let mut renorm = 0usize;
    let mut out = Vec::new();
    for_each_block(reader, |block| {
        for &z in block {
            let m = Complex::new(osc.re as f32, osc.im as f32);
            hist[pos] = z * m;
            pos = (pos + 1) % ntaps;
            osc *= step;
            renorm += 1;
            if renorm == 4096 {
                osc /= osc.norm();
                renorm = 0;
            }
            since_out += 1;
            if since_out == decim {
                since_out = 0;
                // Dot product, oldest sample first.
                let mut acc = Complex::new(0.0f32, 0.0);
                for (k, t) in taps.iter().enumerate() {
                    acc += hist[(pos + k) % ntaps] * *t;
                }
                out.push(acc);
            }
        }
    })?;
    // Drop the filter's warm-up.
    let warm = (ntaps / decim).min(out.len());
    out.drain(..warm);
    Ok(Slice {
        center_hz: center,
        rate_hz: rate_out,
        samples: out,
    })
}

/// Blackman-windowed sinc low-pass, unity DC gain.
fn lowpass(n: usize, cutoff: f64) -> Vec<f32> {
    let m = (n - 1) as f64;
    let mut t: Vec<f64> = (0..n)
        .map(|i| {
            let x = i as f64 - m / 2.0;
            let sinc = if x == 0.0 {
                2.0 * cutoff
            } else {
                (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x)
            };
            let w = 0.42 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / m).cos()
                + 0.08 * (4.0 * std::f64::consts::PI * i as f64 / m).cos();
            sinc * w
        })
        .collect();
    let sum: f64 = t.iter().sum();
    for v in &mut t {
        *v /= sum;
    }
    t.into_iter().map(|v| v as f32).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iq::to_cf32;

    #[test]
    fn a_tone_lands_at_its_offset_from_the_slice_centre() {
        let rate = 240_000.0;
        let f_tone = 10_000.0; // relative to the snapshot centre
        let n = (rate * 0.5) as usize;
        let s: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let ph = 2.0 * std::f64::consts::PI * f_tone * i as f64 / rate;
                Complex::new(ph.cos() as f32, ph.sin() as f32)
            })
            .collect();
        let meta = IqMeta {
            rate_hz: rate,
            center_hz: 1e6,
        };
        let sl = slice(
            std::io::Cursor::new(to_cf32(&s)),
            meta,
            1e6 + 9_000.0,
            1e6 + 12_000.0,
        )
        .unwrap();
        assert!(
            sl.rate_hz >= 3_000.0 * 1.2 && sl.rate_hz < 10_000.0,
            "{}",
            sl.rate_hz
        );
        // Expected baseband frequency: 10 kHz − 10.5 kHz = −500 Hz.
        let ph: Vec<f64> = sl
            .samples
            .iter()
            .map(|z| f64::from(z.im).atan2(f64::from(z.re)))
            .collect();
        let mut dsum = 0.0;
        for w in ph.windows(2) {
            let mut d = w[1] - w[0];
            while d > std::f64::consts::PI {
                d -= 2.0 * std::f64::consts::PI;
            }
            while d < -std::f64::consts::PI {
                d += 2.0 * std::f64::consts::PI;
            }
            dsum += d;
        }
        let f_est = dsum / (ph.len() - 1) as f64 * sl.rate_hz / (2.0 * std::f64::consts::PI);
        assert!((f_est + 500.0).abs() < 2.0, "{f_est}");
        // Amplitude ~1 after unity-gain filtering.
        let mid = sl.samples[sl.samples.len() / 2].norm();
        assert!((mid - 1.0).abs() < 0.05, "{mid}");
    }

    #[test]
    fn out_of_span_is_an_error() {
        let meta = IqMeta {
            rate_hz: 48_000.0,
            center_hz: 0.0,
        };
        assert!(slice(
            std::io::Cursor::new(Vec::<u8>::new()),
            meta,
            30_000.0,
            31_000.0
        )
        .is_err());
    }
}
