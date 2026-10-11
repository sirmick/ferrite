//! Long-FFT spectrum of a range: Hz-accurate peaks, level over the
//! floor, width, and keying (from a 20 ms envelope pass).
//!
//! Pass 1 runs large Hann FFTs (50 % overlap) and keeps only the bins
//! inside the range → average, max-hold and floor. Peaks are local
//! maxima with parabolic-interpolated frequency. Pass 2 mixes each peak
//! to DC and measures its power in 20 ms windows: long FFT frames can't
//! see CW keying, short windows can.

use std::io::Read;

use anyhow::{anyhow, Context, Result};
use num_complex::Complex;
use serde::Serialize;

use crate::db;

/// Largest FFT (resolution floor ≈ rate / 2^20).
const MAX_FFT: usize = 1 << 20;
/// A bin is "on" in a frame when it's this far above the floor (9 dB).
const ON_RATIO: f32 = 8.0;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Peak {
    /// Interpolated centre frequency, Hz.
    pub freq_hz: f64,
    /// Average power over the floor, dB.
    pub avg_db: f32,
    /// Max-hold over the floor, dB.
    pub peak_db: f32,
    /// Fraction of time the signal was on: from the 20 ms envelope when
    /// `keyed`, else the fraction of long frames ≥ 9 dB over the floor.
    pub duty: f32,
    /// −6 dB width of the average, Hz.
    pub width_hz: f64,
    /// On/off keyed (CW, beacon slots): intermittent and well above its
    /// own average when on.
    pub keyed: bool,
}

/// Settings for [`analyze`].
#[derive(Debug, Clone, Copy)]
pub struct SpectrumParams {
    pub rate: f64,
    pub center: f64,
    pub lo: f64,
    pub hi: f64,
    pub res_hz: f64,
    pub max_peaks: usize,
    pub profile_points: usize,
    /// Ignore peaks within this many Hz of `center` — on a raw source tap
    /// the LO's DC spur / leakage flickers there and reads as "keyed".
    pub dc_guard_hz: f64,
}

fn fft_size(rate: f64, res_hz: f64) -> usize {
    let want = (rate / res_hz.max(0.1)).max(64.0) as usize;
    want.next_power_of_two().min(MAX_FFT)
}

/// Run the analysis over interleaved cf32 IQ. `open` yields a fresh
/// reader each call: pass 1 is the long-FFT spectrum, pass 2 measures
/// each peak's envelope in short windows for keying.
pub fn analyze<R: Read>(open: impl Fn() -> Result<R>, a: SpectrumParams) -> Result<SpectrumResult> {
    let mut reader = open()?;
    let n = fft_size(a.rate, a.res_hz);
    let res = a.rate / n as f64;
    let half = a.rate / 2.0;
    if a.lo < a.center - half || a.hi > a.center + half {
        return Err(anyhow!(
            "range {:.0}–{:.0} Hz is outside the tapped span {:.0}–{:.0} Hz",
            a.lo,
            a.hi,
            a.center - half,
            a.center + half
        ));
    }
    // FFT bin indices covering [lo, hi], in frequency order.
    let to_bin = |f: f64| -> i64 { ((f - a.center) / res).round() as i64 };
    let (b_lo, b_hi) = (to_bin(a.lo), to_bin(a.hi));
    let bins: Vec<usize> = (b_lo..=b_hi)
        .map(|b| b.rem_euclid(n as i64) as usize)
        .collect();
    let freq_of = |i: usize| a.center + (b_lo + i as i64) as f64 * res;

    let win: Vec<f32> = (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos())
        .collect();
    let wsum: f32 = win.iter().sum();
    let norm = 1.0 / (wsum * wsum);
    let mut planner = rustfft::FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(n);

    // Stream with 50 % overlap.
    let hop = n / 2;
    let mut ring: Vec<Complex<f32>> = Vec::with_capacity(n);
    let mut frames: Vec<Vec<f32>> = Vec::new();
    let mut buf = vec![0u8; hop * 8];
    let mut scratch = vec![Complex::new(0.0f32, 0.0); n];
    loop {
        let mut filled = 0;
        while filled < buf.len() {
            let got = reader.read(&mut buf[filled..]).context("read IQ")?;
            if got == 0 {
                break;
            }
            filled += got;
        }
        let samples = filled / 8;
        for s in 0..samples {
            let o = s * 8;
            let re = f32::from_le_bytes(buf[o..o + 4].try_into().unwrap_or([0; 4]));
            let im = f32::from_le_bytes(buf[o + 4..o + 8].try_into().unwrap_or([0; 4]));
            ring.push(Complex::new(re, im));
        }
        while ring.len() >= n {
            for i in 0..n {
                scratch[i] = ring[i] * win[i];
            }
            fft.process(&mut scratch);
            frames.push(bins.iter().map(|&k| scratch[k].norm_sqr() * norm).collect());
            ring.drain(..hop);
        }
        if samples < hop {
            break;
        }
    }
    if frames.is_empty() {
        return Err(anyhow!(
            "not enough IQ for one {n}-point frame ({:.2} s at {:.0} Hz) — use a longer `secs` or coarser `res_hz`",
            n as f64 / a.rate,
            a.rate
        ));
    }
    let nf = frames.len();
    let mut core = reduce(&frames, res, &freq_of, a.max_peaks, a.profile_points);
    core.frames = nf;
    core.peaks
        .retain(|p| (p.freq_hz - a.center).abs() > a.dc_guard_hz);
    keying_pass(open()?, a, &mut core.peaks)?;
    Ok(core)
}

/// Envelope p90/p10 swing above which a peak counts as keyed.
const KEY_SWING_DB: f64 = 18.0;

/// Envelope window for the keying pass (20 ms: shorter than a CW dit at
/// 40 WPM's 30 ms would smear; longer blurs dits together).
const KEY_WINDOW_S: f64 = 0.02;

/// Pass 2: per peak, mix to DC and integrate power in 20 ms windows.
/// Keyed = the envelope spends real time both on and off: p90/p10 over
/// 10 dB; duty = windows above their geometric midpoint. (Pass 1's
/// frames are long — up to a second at fine resolution — so every frame
/// of a keyed signal holds some on-time; this is where keying shows.)
fn keying_pass<R: Read>(mut reader: R, a: SpectrumParams, peaks: &mut [Peak]) -> Result<()> {
    if peaks.is_empty() {
        return Ok(());
    }
    let w = ((a.rate * KEY_WINDOW_S) as usize).max(16);
    let k = peaks.len();
    let step: Vec<Complex<f64>> = peaks
        .iter()
        .map(|p| {
            let f = p.freq_hz - a.center;
            Complex::from_polar(1.0, -2.0 * std::f64::consts::PI * f / a.rate)
        })
        .collect();
    let mut osc = vec![Complex::new(1.0f64, 0.0); k];
    let mut acc = vec![Complex::new(0.0f64, 0.0); k];
    let mut env: Vec<Vec<f64>> = vec![Vec::new(); k];
    let mut in_win = 0usize;
    let mut buf = vec![0u8; 1 << 16];
    let mut have = 0usize; // bytes carried over (a partial sample)
    loop {
        let got = reader
            .read(&mut buf[have..])
            .context("read IQ (keying pass)")?;
        if got == 0 {
            break;
        }
        let total = have + got;
        let whole = total - total % 8;
        let (samples, _) = buf[..whole].as_chunks::<8>();
        for c in samples {
            let re = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            let im = f32::from_le_bytes([c[4], c[5], c[6], c[7]]);
            let z = Complex::new(f64::from(re), f64::from(im));
            for j in 0..k {
                acc[j] += z * osc[j];
                osc[j] *= step[j];
            }
            in_win += 1;
            if in_win == w {
                for j in 0..k {
                    env[j].push(acc[j].norm_sqr());
                    acc[j] = Complex::new(0.0, 0.0);
                    // Keep the oscillator on the unit circle.
                    let n = osc[j].norm();
                    osc[j] /= n;
                }
                in_win = 0;
            }
        }
        buf.copy_within(whole..total, 0);
        have = total - whole;
    }
    for (p, e) in peaks.iter_mut().zip(env.iter_mut()) {
        if e.len() < 10 {
            continue;
        }
        e.sort_by(f64::total_cmp);
        let p10 = e[e.len() / 10].max(1e-30);
        let p90 = e[e.len() * 9 / 10].max(1e-30);
        let mid = (p10 * p90).sqrt();
        let on = e.iter().filter(|&&v| v > mid).count() as f32 / e.len() as f32;
        let swing_db = 10.0 * (p90 / p10).log10();
        // Noise alone swings ~13 dB (p90/p10 of an exponential power
        // distribution = ln10/ln(10/9)); real keying is on/off by the
        // signal's SNR. 18 dB keeps noise out (live: a whole band of
        // noise bins read as "keyed" at the old 10 dB).
        p.keyed = swing_db > KEY_SWING_DB && on > 0.03 && on < 0.95;
        if p.keyed {
            p.duty = on;
        }
    }
    Ok(())
}

/// What [`analyze`] measured.
#[derive(Debug, Clone, Serialize)]
pub struct SpectrumResult {
    /// Actual resolution, Hz per bin.
    pub res_hz: f64,
    /// Long-FFT frames averaged.
    pub frames: usize,
    /// Median average power across the range, dBFS (the floor every
    /// `*_db` is relative to).
    pub floor_dbfs: f32,
    /// Strongest first.
    pub peaks: Vec<Peak>,
    /// `[freq_hz, maxhold_db_over_floor]`, evenly spaced over the range.
    pub profile: Vec<[f64; 2]>,
}

fn reduce(
    frames: &[Vec<f32>],
    res: f64,
    freq_of: &dyn Fn(usize) -> f64,
    max_peaks: usize,
    profile_points: usize,
) -> SpectrumResult {
    let nb = frames[0].len();
    let nf = frames.len() as f32;
    let mut avg = vec![0f32; nb];
    let mut mx = vec![0f32; nb];
    for fr in frames {
        for (i, &p) in fr.iter().enumerate() {
            avg[i] += p / nf;
            mx[i] = mx[i].max(p);
        }
    }
    let mut sorted = avg.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[nb / 2].max(1e-30);
    let floor_db = db(floor);
    let avg_db: Vec<f32> = avg.iter().map(|&p| db(p) - floor_db).collect();
    let mx_db: Vec<f32> = mx.iter().map(|&p| db(p) - floor_db).collect();
    let duty: Vec<f32> = (0..nb)
        .map(|i| frames.iter().filter(|fr| fr[i] > floor * ON_RATIO).count() as f32 / nf)
        .collect();

    // Candidates: steady (avg ≥ 6 dB) or intermittent (max-hold ≥ 12 dB),
    // local maxima of max-hold over ±2 bins.
    let mut peaks: Vec<Peak> = Vec::new();
    for i in 0..nb {
        if !(avg_db[i] >= 6.0 || mx_db[i] >= 12.0) {
            continue;
        }
        let lo = i.saturating_sub(2);
        let hi = (i + 2).min(nb - 1);
        if (lo..=hi).any(|j| j != i && (mx_db[j] > mx_db[i] || (mx_db[j] == mx_db[i] && j < i))) {
            continue;
        }
        // Parabolic interpolation on the max-hold dB around the bin.
        let delta = if i > 0 && i + 1 < nb {
            let (a, b, c) = (mx_db[i - 1], mx_db[i], mx_db[i + 1]);
            let den = a - 2.0 * b + c;
            if den.abs() > 1e-6 {
                (0.5 * (a - c) / den).clamp(-0.5, 0.5)
            } else {
                0.0
            }
        } else {
            0.0
        };
        // −6 dB width on the average — but never below 3 dB over the
        // floor, or a weak peak's "width" runs out across the whole range.
        let edge = (avg_db[i] - 6.0).max(3.0);
        let mut l = i;
        while l > 0 && avg_db[l - 1] > edge {
            l -= 1;
        }
        let mut r = i;
        while r + 1 < nb && avg_db[r + 1] > edge {
            r += 1;
        }
        let d = duty[i];
        peaks.push(Peak {
            freq_hz: freq_of(i) + f64::from(delta) * res,
            avg_db: avg_db[i],
            peak_db: mx_db[i],
            duty: d,
            width_hz: (r - l + 1) as f64 * res,
            keyed: false,
        });
    }
    peaks.sort_by(|a, b| b.peak_db.total_cmp(&a.peak_db));
    peaks.truncate(max_peaks);

    let profile = if profile_points == 0 {
        Vec::new()
    } else {
        let step = (nb as f64 / profile_points as f64).max(1.0);
        let mut out = Vec::new();
        let mut s = 0.0;
        while (s as usize) < nb {
            let a = s as usize;
            let b = ((s + step) as usize).min(nb).max(a + 1);
            let (k, v) = (a..b)
                .map(|j| (j, mx_db[j]))
                .max_by(|x, y| x.1.total_cmp(&y.1))
                .unwrap_or((a, 0.0));
            out.push([freq_of(k), f64::from((v * 10.0).round() / 10.0)]);
            s += step;
        }
        out
    };
    SpectrumResult {
        res_hz: res,
        frames: 0,
        floor_dbfs: floor_db,
        peaks,
        profile,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic IQ: noise + a steady carrier + a keyed carrier.
    fn synth(rate: f64, secs: f64) -> Vec<u8> {
        let n = (rate * secs) as usize;
        let mut out = Vec::with_capacity(n * 8);
        let mut seed: u32 = 12345;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32) - 0.5
        };
        let (f1, f2) = (1_234.5_f64, -2_000.25_f64);
        for i in 0..n {
            let t = i as f64 / rate;
            let mut z = Complex::new(rnd() * 0.02, rnd() * 0.02);
            z += Complex::from_polar(0.01, (2.0 * std::f64::consts::PI * f1 * t) as f32);
            // Keyed: on for 120 ms of every 400 ms.
            if (t * 1000.0) as u64 % 400 < 120 {
                z += Complex::from_polar(0.05, (2.0 * std::f64::consts::PI * f2 * t) as f32);
            }
            out.extend_from_slice(&z.re.to_le_bytes());
            out.extend_from_slice(&z.im.to_le_bytes());
        }
        out
    }

    #[test]
    fn finds_steady_and_keyed_carriers_to_a_few_hz() {
        let rate = 48_000.0;
        let iq = synth(rate, 6.0);
        let r = analyze(
            || Ok(std::io::Cursor::new(iq.clone())),
            SpectrumParams {
                rate,
                center: 14_000_000.0,
                lo: 13_995_000.0,
                hi: 14_005_000.0,
                res_hz: 5.0,
                max_peaks: 10,
                profile_points: 50,
                dc_guard_hz: 0.0,
            },
        )
        .unwrap();
        assert!(r.frames > 10);
        assert!(r.res_hz < 6.0);
        let near = |f: f64| {
            r.peaks
                .iter()
                .find(|p| (p.freq_hz - f).abs() < 6.0)
                .cloned()
        };
        let steady = near(14_001_234.5).expect("steady carrier found");
        let keyed = near(13_997_999.75).expect("keyed carrier found");
        assert!((steady.freq_hz - 14_001_234.5).abs() < 1.5, "{steady:?}");
        assert!((keyed.freq_hz - 13_997_999.75).abs() < 1.5, "{keyed:?}");
        assert!(!steady.keyed && steady.duty > 0.9, "{steady:?}");
        // On 120 ms of every 400 ms → duty ≈ 0.3.
        assert!(keyed.keyed && (keyed.duty - 0.3).abs() < 0.08, "{keyed:?}");
        assert_eq!(r.profile.len(), 50);
    }

    /// A reader that hands back at most 7 bytes per call — never a
    /// whole cf32 sample boundary — to pin the partial-read carry.
    struct Dribble(std::io::Cursor<Vec<u8>>);
    impl Read for Dribble {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = buf.len().min(7);
            self.0.read(&mut buf[..n])
        }
    }

    #[test]
    fn odd_sized_reads_give_the_same_answer() {
        let rate = 48_000.0;
        let iq = synth(rate, 3.0);
        let a = SpectrumParams {
            rate,
            center: 0.0,
            lo: -3_000.0,
            hi: 3_000.0,
            res_hz: 10.0,
            max_peaks: 5,
            profile_points: 0,
            dc_guard_hz: 0.0,
        };
        let whole = analyze(|| Ok(std::io::Cursor::new(iq.clone())), a).unwrap();
        let drib = analyze(|| Ok(Dribble(std::io::Cursor::new(iq.clone()))), a).unwrap();
        assert_eq!(whole.peaks, drib.peaks);
    }

    /// A weak *steady* carrier: clearly a peak on the long FFT, but in a
    /// 20 ms window it sits near the noise, so its envelope swings like
    /// noise (~13 dB p90/p10). It must not read as keyed (live, a band's
    /// worth of weak steady signals did at the old 10 dB swing bar).
    #[test]
    fn a_weak_steady_carrier_is_not_keyed() {
        let rate = 48_000.0;
        let n = (rate * 6.0) as usize;
        let mut seed: u32 = 99;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32) - 0.5
        };
        let mut iq = Vec::with_capacity(n * 8);
        for i in 0..n {
            let t = i as f64 / rate;
            let mut z = Complex::new(rnd() * 0.02, rnd() * 0.02);
            z += Complex::from_polar(0.0003, (2.0 * std::f64::consts::PI * 3_000.0 * t) as f32);
            iq.extend_from_slice(&z.re.to_le_bytes());
            iq.extend_from_slice(&z.im.to_le_bytes());
        }
        let a = SpectrumParams {
            rate,
            center: 0.0,
            lo: -10_000.0,
            hi: 10_000.0,
            res_hz: 5.0,
            max_peaks: 50,
            profile_points: 0,
            dc_guard_hz: 0.0,
        };
        let r = analyze(|| Ok(std::io::Cursor::new(iq.clone())), a).unwrap();
        let c = r
            .peaks
            .iter()
            .find(|p| (p.freq_hz - 3_000.0).abs() < 10.0)
            .expect("weak carrier found");
        assert!(c.avg_db > 6.0 && c.avg_db < 20.0, "{c:?}");
        assert!(!c.keyed, "{c:?}");
        assert!(r.peaks.iter().all(|p| p.width_hz < 1_000.0));
    }

    #[test]
    fn rejects_a_range_outside_the_span() {
        let iq = synth(48_000.0, 1.0);
        let err = analyze(
            || Ok(std::io::Cursor::new(iq.clone())),
            SpectrumParams {
                rate: 48_000.0,
                center: 1e6,
                lo: 1.1e6,
                hi: 1.2e6,
                res_hz: 10.0,
                max_peaks: 5,
                profile_points: 0,
                dc_guard_hz: 0.0,
            },
        )
        .unwrap_err();
        assert!(format!("{err}").contains("outside the tapped span"));
    }
}
