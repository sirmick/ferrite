//! One signal, characterised: where exactly it is, how wide, how strong,
//! how it's keyed and what its spectrum looks like over time — the
//! measurements an operator reads off a waterfall before naming a mode.
//!
//! Pass 1 slices the analysis span and runs a short-time FFT: the floor,
//! the signal's occupied region (99 % of its power over the floor, on the
//! max-hold so bursts count), a power-weighted centre, and the distinct
//! spectral lines inside it. Which line dominates each frame tells FSK
//! (two lines taking turns) and MFSK from a carrier with sidebands. Pass 2
//! slices just the occupied region and measures its envelope: keying,
//! duty, burst timing, periodicity, and how constant the amplitude is
//! (FM / FSK / PSK stay flat; AM, SSB, OFDM and noise don't).

use std::io::Read;

use anyhow::{anyhow, Result};
use num_complex::Complex;
use serde::Serialize;

use crate::db;
use crate::envelope::{envelope, Envelope, EnvelopeParams};
use crate::iq::IqMeta;
use crate::slice::{slice, Slice};
use crate::spectrogram::{spectrogram, Spectrogram, SpectrogramParams};

/// Bins this far over the floor (6 dB) belong to the signal's region.
const REGION_RATIO: f32 = 4.0;
/// A spectral line must stand this far over the floor (10 dB)…
const TONE_FLOOR_RATIO: f32 = 10.0;
/// …and this far over the region's typical level (6 dB) to be a tone
/// rather than a lump of a wide signal's plateau.
const TONE_PLATEAU_RATIO: f32 = 4.0;
/// Most tones reported.
const MAX_TONES: usize = 8;
/// Below this SNR there's nothing to characterise.
const MIN_SNR_DB: f32 = 6.0;

#[derive(Debug, Clone, Copy)]
pub struct InspectParams {
    /// The analysis span (the slice; ~3× the signal's width).
    pub lo_hz: f64,
    pub hi_hz: f64,
    /// Where the signal is expected.
    pub freq_hz: f64,
    /// Its expected width, Hz (0 = unknown).
    pub width_hz: f64,
    /// The LO, when it falls inside the span: its DC spur is blanked.
    pub dc_hz: Option<f64>,
    pub dc_guard_hz: f64,
}

/// A distinct spectral line inside the signal.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Tone {
    pub freq_hz: f64,
    /// Max-hold level relative to the strongest tone, dB (≤ 0).
    pub rel_db: f32,
    /// Fraction of on-frames in which this tone was the strongest.
    pub share: f32,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Bursts {
    /// On-segments in the envelope.
    pub count: usize,
    pub on_ms_median: f64,
    pub gap_ms_median: Option<f64>,
    /// Start-to-start interval when it repeats regularly.
    pub period_s: Option<f64>,
    pub regular: bool,
}

/// The feature bundle.
#[derive(Debug, Clone, Serialize)]
pub struct Features {
    /// Power-weighted centre of the occupied region, Hz.
    pub freq_hz: f64,
    /// `freq_hz` − the asked frequency.
    pub offset_hz: f64,
    /// 99 %-power occupied bandwidth (max-hold), Hz.
    pub occupied_hz: f64,
    /// Strongest bin (max-hold), dBFS at `res_hz`.
    pub peak_dbfs: f32,
    /// Average power over the floor, summed across the region, dBFS.
    pub power_dbfs: f32,
    /// Per-bin noise floor at `res_hz`, dBFS.
    pub floor_dbfs: f32,
    /// Max-hold peak over the floor, dB.
    pub snr_db: f32,
    /// FFT resolution, Hz per bin.
    pub res_hz: f64,
    pub tones: Vec<Tone>,
    /// Two lines taking turns: their spacing, Hz.
    pub fsk_shift_hz: Option<f64>,
    pub keyed: bool,
    pub duty: f32,
    /// Envelope p90 − p10, dB.
    pub swing_db: f32,
    pub bursts: Bursts,
    /// Strongest envelope periodicity (autocorrelation), seconds.
    pub envelope_period_s: Option<f64>,
    /// Its autocorrelation peak, 0–1.
    pub envelope_period_strength: f32,
    /// std/mean of |x| while on — ~0 for constant envelope, ~0.52 for
    /// noise. `None` when too little was on.
    pub envelope_cv: Option<f32>,
    pub constant_envelope: bool,
    /// One-line reading of the above (heuristic — a lead, not a verdict).
    pub hint: String,
    pub secs: f64,
}

#[derive(Debug, Clone)]
pub struct InspectResult {
    pub features: Features,
    /// The span over time, for the picture.
    pub spectrogram: Spectrogram,
    /// The occupied region's envelope.
    pub envelope: Envelope,
}

/// Short-time power spectra of a slice: frames × bins, bins low → high
/// frequency (bin `i` ↔ `center + (i − n/2)·res`), linear, 0 dBFS = a
/// full-scale tone.
fn stft(sl: &Slice, n: usize) -> Vec<Vec<f32>> {
    let win: Vec<f32> = (0..n)
        .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos())
        .collect();
    let wsum: f32 = win.iter().sum();
    let norm = 1.0 / (wsum * wsum);
    let fft = rustfft::FftPlanner::<f32>::new().plan_fft_forward(n);
    let mut buf = vec![Complex::new(0.0f32, 0.0); n];
    let mut out = Vec::new();
    let mut s = 0;
    while s + n <= sl.samples.len() {
        for i in 0..n {
            buf[i] = sl.samples[s + i] * win[i];
        }
        fft.process(&mut buf);
        out.push(
            (0..n)
                .map(|i| buf[(i + n / 2) % n].norm_sqr() * norm)
                .collect(),
        );
        s += n / 2;
    }
    out
}

fn percentile(v: &[f32], q: f32) -> f32 {
    let mut s = v.to_vec();
    s.sort_by(f32::total_cmp);
    s[((s.len() - 1) as f32 * q).round() as usize]
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

/// Characterise the signal at `p.freq_hz`. `open` yields a fresh reader
/// over the snapshot each call (it's read twice).
pub fn inspect<R: Read>(
    open: impl Fn() -> Result<R>,
    meta: IqMeta,
    p: InspectParams,
) -> Result<InspectResult> {
    let sl = slice(open()?, meta, p.lo_hz, p.hi_hz)?;
    let secs = sl.samples.len() as f64 / sl.rate_hz;
    let span = p.hi_hz - p.lo_hz;

    // ── pass 1: spectrum over time ─────────────────────────────────────
    let mut n = ((sl.rate_hz / (span / 512.0)) as usize)
        .next_power_of_two()
        .clamp(64, 1 << 16);
    while n > 64 && n * 8 > sl.samples.len() {
        n /= 2;
    }
    if sl.samples.len() < n {
        return Err(anyhow!("snapshot too short to inspect ({secs:.2} s)"));
    }
    let res = sl.rate_hz / n as f64;
    let mut frames = stft(&sl, n);
    let freq_of = |i: usize| sl.center_hz + (i as f64 - (n / 2) as f64) * res;
    let bin_of = |f: f64| ((f - sl.center_hz) / res + (n / 2) as f64).round();
    // Only the asked span (the slice keeps a guard band either side).
    let b0 = bin_of(p.lo_hz).max(0.0) as usize;
    let b1 = (bin_of(p.hi_hz) as usize).min(n - 1);
    for fr in &mut frames {
        fr.truncate(b1 + 1);
        fr.drain(..b0);
    }
    let nb = b1 + 1 - b0;
    let f_of = |i: usize| freq_of(i + b0);
    let nf = frames.len() as f32;
    let mut avg = vec![0f32; nb];
    let mut mx = vec![0f32; nb];
    for fr in &frames {
        for (i, &v) in fr.iter().enumerate() {
            avg[i] += v / nf;
            mx[i] = mx[i].max(v);
        }
    }
    // Blank the LO's DC spur: it reads as a perfect tone.
    let dc_bins: Vec<usize> = match p.dc_hz {
        Some(dc) => (0..nb)
            .filter(|&i| (f_of(i) - dc).abs() <= p.dc_guard_hz.max(res))
            .collect(),
        None => Vec::new(),
    };
    let keep: Vec<usize> = (0..nb).filter(|i| !dc_bins.contains(i)).collect();
    let pick = |v: &[f32]| keep.iter().map(|&i| v[i]).collect::<Vec<_>>();
    let floor_avg = percentile(&pick(&avg), 0.2).max(1e-20);
    let floor_mx = percentile(&pick(&mx), 0.2).max(1e-20);
    for &i in &dc_bins {
        avg[i] = floor_avg;
        mx[i] = floor_mx;
        for fr in &mut frames {
            fr[i] = floor_avg;
        }
    }

    // The peak near where we were told to look.
    let w = p.width_hz.max(4.0 * res);
    let margin = (w / 2.0).max(10.0 * res);
    let s0 = (bin_of(p.freq_hz - w / 2.0 - margin) - b0 as f64).max(0.0) as usize;
    let s1 = ((bin_of(p.freq_hz + w / 2.0 + margin) - b0 as f64).max(0.0) as usize).min(nb - 1);
    let pk = (s0..=s1.max(s0))
        .max_by(|&a, &b| mx[a].total_cmp(&mx[b]))
        .unwrap_or(nb / 2);
    let snr_db = db(mx[pk]) - db(floor_mx);

    // Its region: grow while over the floor, bridging small gaps.
    let thr = floor_mx * REGION_RATIO;
    let gap_bins = ((w * 0.5 / res) as usize).max(2);
    let grow = |step: isize| {
        let mut edge = pk;
        let mut gap = 0;
        let mut i = pk as isize;
        loop {
            i += step;
            if i < 0 || i >= nb as isize {
                break;
            }
            if mx[i as usize] > thr {
                edge = i as usize;
                gap = 0;
            } else {
                gap += 1;
                if gap > gap_bins {
                    break;
                }
            }
        }
        edge
    };
    let (r0, r1) = if snr_db >= MIN_SNR_DB {
        (grow(-1), grow(1))
    } else {
        (s0, s1.max(s0))
    };
    let excess: Vec<f32> = (r0..=r1).map(|i| (mx[i] - floor_mx).max(0.0)).collect();
    let total: f32 = excess.iter().sum();
    let (mut o0, mut o1) = (r0, r1);
    if total > 0.0 {
        let mut acc = 0.0;
        let mut lo_set = false;
        for (k, &e) in excess.iter().enumerate() {
            acc += e;
            if !lo_set && acc >= total * 0.005 {
                o0 = r0 + k;
                lo_set = true;
            }
            if acc >= total * 0.995 {
                o1 = r0 + k;
                break;
            }
        }
    }
    let occupied_hz = (o1 + 1 - o0) as f64 * res;
    let centre = if total > 0.0 {
        excess
            .iter()
            .enumerate()
            .map(|(k, &e)| f_of(r0 + k) * f64::from(e))
            .sum::<f64>()
            / f64::from(total)
    } else {
        p.freq_hz
    };
    let power: f32 = (r0..=r1).map(|i| (avg[i] - floor_avg).max(0.0)).sum();

    // Frames the signal is on (band power over its own midpoint).
    let band: Vec<f32> = frames.iter().map(|fr| fr[r0..=r1].iter().sum()).collect();
    let band_db: Vec<f32> = band.iter().map(|&v| db(v)).collect();
    let (p10, p90) = (percentile(&band_db, 0.1), percentile(&band_db, 0.9));
    let on_thr = if p90 - p10 < 6.0 {
        f32::MIN
    } else {
        (p10 + p90) / 2.0
    };
    let on: Vec<usize> = (0..frames.len()).filter(|&f| band_db[f] > on_thr).collect();

    // Distinct lines: local maxima standing clear of floor and plateau.
    let plateau = percentile(&mx[r0..=r1], 0.5);
    let sep = ((occupied_hz / 40.0 / res) as usize).max(2);
    let mut lines: Vec<usize> = (r0..=r1)
        .filter(|&i| {
            mx[i] > floor_mx * TONE_FLOOR_RATIO
                && mx[i] > plateau * TONE_PLATEAU_RATIO
                && (i.saturating_sub(sep)..=(i + sep).min(nb - 1)).all(|j| mx[j] <= mx[i])
        })
        .collect();
    lines.sort_by(|&a, &b| mx[b].total_cmp(&mx[a]));
    if let Some(&top) = lines.first() {
        lines.retain(|&i| mx[i] >= mx[top] / 100.0);
    }
    lines.truncate(MAX_TONES);
    // Who dominates each moment, on frames just long enough to tell the
    // two closest strong lines apart (the long frames above span many
    // symbols and see every tone at once).
    let mut wins = vec![0usize; lines.len()];
    let mut exclusive = 0usize;
    let mut n_on = 0usize;
    // FSK / MFSK tones each reach full level in the max-hold; sidebands
    // and keying splatter don't.
    let top_mx = lines.first().map_or(0.0, |&i| mx[i]);
    let full: Vec<usize> = lines
        .iter()
        .copied()
        .filter(|&i| mx[i] >= top_mx / 2.0)
        .collect();
    if full.len() >= 2 {
        let d = full
            .iter()
            .flat_map(|&a| full.iter().map(move |&b| a.abs_diff(b)))
            .filter(|&x| x > 0)
            .min()
            .unwrap_or(1) as f64
            * res;
        let ns = (((3.0 * sl.rate_hz / d).ceil() as usize).next_power_of_two()).clamp(16, n);
        let rs = sl.rate_hz / ns as f64;
        let sbin = |f: f64| ((f - sl.center_hz) / rs + (ns / 2) as f64).round() as usize;
        let short = stft(&sl, ns);
        let (q0, q1) = (sbin(f_of(r0)).min(ns - 1), sbin(f_of(r1)).min(ns - 1));
        let sband: Vec<f32> = short
            .iter()
            .map(|fr| db(fr[q0..=q1].iter().sum()))
            .collect();
        let (s10, s90) = (percentile(&sband, 0.1), percentile(&sband, 0.9));
        let s_thr = if s90 - s10 < 6.0 {
            f32::MIN
        } else {
            (s10 + s90) / 2.0
        };
        let at: Vec<(usize, usize)> = lines
            .iter()
            .enumerate()
            .filter(|(_, i)| full.contains(i))
            .map(|(k, &i)| (k, sbin(f_of(i)).min(ns - 1)))
            .collect();
        for (fr, _) in short.iter().zip(&sband).filter(|(_, &b)| b > s_thr) {
            n_on += 1;
            let mut lvls: Vec<(usize, f32)> = at.iter().map(|&(k, b)| (k, fr[b])).collect();
            lvls.sort_by(|a, b| b.1.total_cmp(&a.1));
            wins[lvls[0].0] += 1;
            if lvls[0].1 > lvls[1].1 * 4.0 {
                exclusive += 1;
            }
        }
    } else if !lines.is_empty() {
        wins[0] = on.len();
        n_on = on.len();
        exclusive = on.len();
    }
    let n_on = n_on.max(1) as f32;
    let top_lvl = lines.first().map_or(1.0, |&i| mx[i]);
    let tones: Vec<Tone> = lines
        .iter()
        .zip(&wins)
        .map(|(&i, &w)| Tone {
            freq_hz: f_of(i),
            rel_db: db(mx[i]) - db(top_lvl),
            share: w as f32 / n_on,
        })
        .collect();
    let taking_turns: Vec<&Tone> = tones.iter().filter(|t| t.share >= 0.12).collect();
    let exclusive_frac = exclusive as f32 / n_on;
    let fsk_shift_hz = (taking_turns.len() == 2 && exclusive_frac >= 0.45)
        .then(|| (taking_turns[0].freq_hz - taking_turns[1].freq_hz).abs());
    let mfsk = taking_turns.len() >= 3 && exclusive_frac >= 0.45;

    // ── pass 2: the occupied region's envelope ─────────────────────────
    let half = meta.rate_hz / 2.0;
    let pad = (occupied_hz * 0.1).max(2.0 * res).max(25.0);
    let e_lo = (f_of(o0) - res / 2.0 - pad).max(meta.center_hz - half);
    let e_hi = (f_of(o1) + res / 2.0 + pad).min(meta.center_hz + half);
    let narrow = slice(open()?, meta, e_lo, e_hi)?;
    // ~20 samples a window keeps noise from reading as keying; a narrow
    // signal gets ≤ 10 ms (a 40 WPM dit is 30 ms).
    let window_ms = (20_000.0 / occupied_hz.max(1.0)).clamp(1.0, 10.0);
    let env = envelope(&narrow, EnvelopeParams { window_ms })?;

    let mut on_ms: Vec<f64> = env
        .segments
        .iter()
        .map(|s| (s.end_s - s.start_s) * 1e3)
        .collect();
    let mut gaps: Vec<f64> = env
        .segments
        .windows(2)
        .map(|w| (w[1].start_s - w[0].end_s) * 1e3)
        .collect();
    let mut starts: Vec<f64> = env
        .segments
        .windows(2)
        .map(|w| w[1].start_s - w[0].start_s)
        .collect();
    let interval = median(&mut starts.clone());
    let regular = starts.len() >= 3
        && interval.is_some_and(|m| {
            let mut dev: Vec<f64> = starts.iter().map(|s| (s - m).abs()).collect();
            median(&mut dev).is_some_and(|d| d / m < 0.15)
        });
    let bursts = Bursts {
        count: env.segments.len(),
        on_ms_median: median(&mut on_ms).unwrap_or(0.0),
        gap_ms_median: median(&mut gaps),
        period_s: if regular { interval } else { None },
        regular,
    };
    starts.clear();

    // Periodicity the segmenter may miss (fast pulse trains).
    let lin: Vec<f64> = env
        .power_db
        .iter()
        .map(|&d| 10f64.powf(f64::from(d) / 10.0))
        .collect();
    let mean = lin.iter().sum::<f64>() / lin.len() as f64;
    let x: Vec<f64> = lin.iter().map(|v| v - mean).collect();
    let r0v: f64 = x.iter().map(|v| v * v).sum();
    let (mut period, mut strength) = (None, 0f32);
    if r0v > 0.0 && env.swing_db >= 3.0 {
        let ac = |lag: usize| x.iter().zip(&x[lag..]).map(|(a, b)| a * b).sum::<f64>() / r0v;
        let max_lag = x.len() / 2;
        let mut lag = 1;
        while lag < max_lag && ac(lag) > 0.0 {
            lag += 1;
        }
        let best = (lag..max_lag)
            .map(|l| (l, ac(l)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        if let Some((l, r)) = best {
            if r >= 0.3 {
                period = Some(l as f64 * env.window_s);
                strength = r as f32;
            }
        }
    }

    // Amplitude spread while on.
    let per = (env.window_s * narrow.rate_hz).round().max(1.0) as usize;
    let mags: Vec<f32> = narrow
        .samples
        .iter()
        .enumerate()
        .filter(|(k, _)| {
            env.power_db
                .get(k / per)
                .is_some_and(|&d| d > env.threshold_db)
        })
        .map(|(_, s)| s.norm())
        .collect();
    let envelope_cv = (mags.len() >= 200).then(|| {
        let m = mags.iter().sum::<f32>() / mags.len() as f32;
        let var = mags.iter().map(|v| (v - m) * (v - m)).sum::<f32>() / mags.len() as f32;
        var.sqrt() / m.max(1e-20)
    });
    let constant_envelope = snr_db >= 15.0 && envelope_cv.is_some_and(|c| c < 0.25);

    // ── the picture ────────────────────────────────────────────────────
    let mut sp = SpectrogramParams::new(p.lo_hz, p.hi_hz);
    sp.res_hz = (span / 300.0).max(sl.rate_hz / 65_536.0);
    sp.step_ms = (secs * 1_000.0 / 300.0).max(2.0);
    sp.max_rows = 300;
    let sg = spectrogram(&sl, sp)?;

    let mut f = Features {
        freq_hz: centre,
        offset_hz: centre - p.freq_hz,
        occupied_hz,
        peak_dbfs: db(mx[pk]),
        power_dbfs: db(power.max(1e-20)),
        floor_dbfs: db(floor_mx),
        snr_db,
        res_hz: res,
        tones,
        fsk_shift_hz,
        keyed: env.keyed,
        duty: env.duty,
        swing_db: env.swing_db,
        bursts,
        envelope_period_s: period,
        envelope_period_strength: strength,
        envelope_cv,
        constant_envelope,
        hint: String::new(),
        secs,
    };
    f.hint = hint(&f, mfsk);
    Ok(InspectResult {
        features: f,
        spectrogram: sg,
        envelope: env,
    })
}

fn hz(v: f64) -> String {
    if v >= 1e6 {
        format!("{:.2} MHz", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.1} kHz", v / 1e3)
    } else {
        format!("{v:.0} Hz")
    }
}

fn hint(f: &Features, mfsk: bool) -> String {
    if f.snr_db < MIN_SNR_DB {
        return "nothing clearly above the floor here".into();
    }
    let mut h = Vec::new();
    if f.offset_hz.abs() > (f.occupied_hz / 2.0).max(2_000.0) {
        h.push(format!(
            "nothing at the asked frequency — the strongest signal nearby is {:+.1} kHz off",
            f.offset_hz / 1e3
        ));
    }
    if f.snr_db < 15.0 {
        h.push(format!(
            "weak (SNR {:.0} dB) — features are rough",
            f.snr_db
        ));
    }
    h.push(format!("{} wide", hz(f.occupied_hz)));
    if let Some(s) = f.fsk_shift_hz {
        h.push(format!("2-FSK, shift {}", hz(s)));
    } else if mfsk {
        h.push(format!("MFSK ({} tones taking turns)", f.tones.len()));
    } else if f.tones.len() == 1 && f.occupied_hz <= 4.0 * f.res_hz {
        h.push("a single unmodulated line".into());
    } else if !f.constant_envelope && f.tones.iter().filter(|t| t.rel_db >= -3.0).count() >= 3 {
        // Lines present at once can't make a constant envelope (an FM
        // sweep's max-hold is lumpy too — that's not a comb).
        h.push(format!(
            "{} near-equal spectral lines (a comb, or a multi-carrier signal)",
            f.tones.iter().filter(|t| t.rel_db >= -3.0).count()
        ));
    }
    if f.bursts.regular {
        if let Some(p) = f.bursts.period_s {
            h.push(format!(
                "bursts {:.0} ms every {:.2} s",
                f.bursts.on_ms_median, p
            ));
        }
    } else if f.keyed {
        h.push(format!("on/off keyed (duty {:.0} %)", f.duty * 100.0));
    } else if f.duty < 0.95 && f.swing_db >= 6.0 {
        h.push("intermittent".into());
    } else {
        h.push("continuous".into());
    }
    if let Some(p) = f.envelope_period_s {
        if !f.bursts.regular {
            h.push(format!("envelope repeats at {:.1} Hz", 1.0 / p));
        }
    }
    if f.constant_envelope {
        h.push("constant envelope (FM / FSK / PSK-like)".into());
    } else if f.envelope_cv.is_some_and(|c| c > 0.4) && f.snr_db >= 15.0 {
        h.push("amplitude varies (AM / SSB / OFDM / noise-like)".into());
    }
    h.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;
    const CENTER: f64 = 10.0e6;

    struct Noise(u32);
    impl Noise {
        fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0 as f32 / u32::MAX as f32 - 0.5
        }
    }

    /// `secs` of IQ at `RATE`, `sig(t)` → baseband sample, plus noise.
    fn iq(secs: f64, noise: f32, sig: impl Fn(f64) -> Complex<f32>) -> Vec<u8> {
        let mut nz = Noise(12_345);
        let n = (RATE * secs) as usize;
        let s: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f64 / RATE;
                sig(t) + Complex::new(nz.next() * noise, nz.next() * noise)
            })
            .collect();
        crate::iq::to_cf32(&s)
    }

    fn tone(f: f64, t: f64, a: f32) -> Complex<f32> {
        let ph = 2.0 * std::f64::consts::PI * f * t;
        Complex::new(ph.cos() as f32 * a, ph.sin() as f32 * a)
    }

    fn run(bytes: &[u8], f: f64, w: f64) -> InspectResult {
        let meta = IqMeta {
            rate_hz: RATE,
            center_hz: CENTER,
        };
        let span = (w * 3.0).max(2_000.0);
        inspect(
            || Ok(std::io::Cursor::new(bytes.to_vec())),
            meta,
            InspectParams {
                lo_hz: f - span / 2.0,
                hi_hz: f + span / 2.0,
                freq_hz: f,
                width_hz: w,
                dc_hz: Some(CENTER),
                dc_guard_hz: 50.0,
            },
        )
        .unwrap()
    }

    #[test]
    fn fsk_gives_its_shift_and_constant_envelope() {
        // 170 Hz shift, 45.45 baud, pseudo-random bits, at +3 kHz.
        let bytes = iq(3.0, 0.02, |t| {
            let sym = (t * 45.45) as u64;
            let bit = (sym.wrapping_mul(2_654_435_761) >> 7) & 1;
            let f = 3_000.0 + if bit == 1 { 85.0 } else { -85.0 };
            tone(f, t, 0.5)
        });
        let r = run(&bytes, CENTER + 3_000.0, 300.0);
        let f = &r.features;
        let shift = f.fsk_shift_hz.unwrap_or_else(|| panic!("no fsk: {f:#?}"));
        assert!(
            (shift - 170.0).abs() < 2.0 * f.res_hz + 5.0,
            "{shift} {:?}",
            f.tones
        );
        assert!(
            (f.freq_hz - (CENTER + 3_000.0)).abs() < 20.0,
            "{}",
            f.freq_hz
        );
        assert!(f.snr_db > 30.0);
        assert!(f.constant_envelope, "{:?}", f.envelope_cv);
        assert!(f.hint.contains("2-FSK"), "{}", f.hint);
    }

    #[test]
    fn periodic_bursts_are_timed() {
        // 40 ms bursts every 250 ms of a carrier at −2 kHz.
        let bytes = iq(3.0, 0.01, |t| {
            if (t % 0.25) < 0.04 {
                tone(-2_000.0, t, 0.5)
            } else {
                Complex::new(0.0, 0.0)
            }
        });
        let r = run(&bytes, CENTER - 2_000.0, 100.0);
        let b = &r.features.bursts;
        assert!(b.regular, "{b:?}");
        assert!((b.period_s.unwrap() - 0.25).abs() < 0.01, "{b:?}");
        // ± one 10 ms envelope window + filter smear.
        assert!((b.on_ms_median - 40.0).abs() <= 12.0, "{b:?}");
        assert!(
            r.features.hint.contains("every 0.25 s"),
            "{}",
            r.features.hint
        );
    }

    #[test]
    fn am_is_not_constant_envelope_and_width_is_measured() {
        // 3 kHz-wide AM (tones at ±1.5 kHz sidebands of a carrier) at +6 kHz,
        // driven by a noisy message so the envelope wanders.
        let bytes = iq(2.0, 0.005, |t| {
            let m = 0.8
                * (2.0 * std::f64::consts::PI * 1_000.0 * t).sin()
                * (2.0 * std::f64::consts::PI * 3.0 * t).sin();
            tone(6_000.0, t, (0.4 * (1.0 + m)) as f32)
        });
        let r = run(&bytes, CENTER + 6_000.0, 2_500.0);
        let f = &r.features;
        assert!(!f.constant_envelope, "{:?}", f.envelope_cv);
        assert!(
            (1_500.0..3_500.0).contains(&f.occupied_hz),
            "{}",
            f.occupied_hz
        );
    }

    #[test]
    fn empty_spot_says_so() {
        let bytes = iq(1.0, 0.02, |_| Complex::new(0.0, 0.0));
        let r = run(&bytes, CENTER + 5_000.0, 1_000.0);
        assert!(r.features.snr_db < MIN_SNR_DB + 6.0);
        assert!(r.features.hint.contains("nothing") || r.features.snr_db >= MIN_SNR_DB);
    }
}
