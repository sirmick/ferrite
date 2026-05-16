//! Shared harness for the modulated-source end-to-end tests.
//!
//! Two jobs:
//!
//! 1. **Drive a block by hand** the way the runtime does — chunked
//!    `BlockIo` calls, advancing by `Work::consumed`, collecting
//!    `Work::produced` — for every type combination the analog chain
//!    needs (real→iq modulators, iq→real demods, iq→iq mixers,
//!    real→real resamplers). Rate-changing blocks (modulator,
//!    decimator, resampler) produce a different count than they
//!    consume; the pump loop handles that.
//!
//! 2. **Score recovered audio against the original.** Analog modes have
//!    no symbol stream to string-match, so fidelity is: find the
//!    chain's group delay by bounded-lag cross-correlation, normalise
//!    the gain by least squares, then report Pearson ρ and segmental
//!    SNR over the aligned overlap. ρ is mean-subtracted, so a constant
//!    DC pedestal (e.g. an FM carrier offset clamped by the demod) does
//!    not depress it.
//!
//! Not every test uses every helper; `#![allow(dead_code)]` keeps the
//! shared module warning-free per integration-test binary.

#![allow(dead_code)]
#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss, clippy::cast_possible_wrap)]
#![allow(clippy::similar_names, clippy::many_single_char_names)]

use ferrite_blocks::block::{
    Block, BlockIo, InBuf, InitCtx, InputPort, OutBuf, OutputPort, PortMeta,
};
use ferrite_blocks::{FileAudioSource, FileAudioSourceParams};
use num_complex::Complex;
use std::path::PathBuf;

/// Absolute path to a file under the repo's `samples/` tree.
pub fn sample_path(rel: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // blocks/ -> repo root
    p.push("samples");
    p.push(rel);
    p
}

/// Load a mono audio clip (dogfoods `FileAudioSource`) → `(samples,
/// rate_hz)`.
pub fn load_audio(rel: &str) -> (Vec<f32>, f64) {
    let mut src = FileAudioSource::new(&FileAudioSourceParams {
        path: sample_path(rel),
        rate_hz_hint: 0.0,
        gain: 1.0,
        loop_playback: false,
    })
    .expect("open audio sample");
    let rate = src.rate_hz();
    let mut all = Vec::new();
    let mut buf = vec![0.0_f32; 1 << 15];
    loop {
        let mut outs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut buf),
        }];
        let w = src
            .process(&mut BlockIo {
                inputs: &mut [],
                outputs: &mut outs,
            })
            .unwrap();
        all.extend_from_slice(&buf[..w.produced[0]]);
        if src.is_eof() {
            break;
        }
    }
    (all, rate)
}

fn one_in_meta(rate_hz: f64) -> [(&'static str, PortMeta); 1] {
    [(
        "in",
        PortMeta {
            sample_rate_hz: rate_hz,
            center_freq_hz: 0.0,
        },
    )]
}

/// Run a block's `init` with a single input port reporting `rate_hz`.
/// Needed for the rate-aware blocks (`*Demod`, `RealF32Resamp`) that
/// build their internals from the negotiated input rate.
pub fn init_at(block: &mut dyn Block, in_rate_hz: f64) {
    let meta = one_in_meta(in_rate_hz);
    let mut ctx = InitCtx {
        input_meta: &meta,
        output_meta: &[],
        frames_hint: 4096,
    };
    block.init(&mut ctx).expect("block init");
}

const IN_CHUNK: usize = 8192;
const OUT_CAP: usize = 1 << 17;

macro_rules! pump {
    ($name:ident, $in_ty:ty, $in_zero:expr, $in_buf:path, $out_ty:ty, $out_zero:expr, $out_buf:path) => {
        /// Feed `input` through `block` in chunks until it is fully
        /// consumed, collecting everything produced. Handles rate
        /// change (produced ≠ consumed) and breaks on stall.
        pub fn $name(block: &mut dyn Block, input: &[$in_ty]) -> Vec<$out_ty> {
            let mut out: Vec<$out_ty> = Vec::with_capacity(input.len());
            let mut obuf = vec![$out_zero; OUT_CAP];
            let mut idx = 0usize;
            loop {
                let take = (input.len() - idx).min(IN_CHUNK);
                if take == 0 {
                    break;
                }
                let w = {
                    let mut inp = [InputPort {
                        name: "in",
                        meta: PortMeta::default(),
                        buf: $in_buf(&input[idx..idx + take]),
                    }];
                    let mut outp = [OutputPort {
                        name: "out",
                        meta: PortMeta::default(),
                        buf: $out_buf(&mut obuf),
                    }];
                    block
                        .process(&mut BlockIo {
                            inputs: &mut inp,
                            outputs: &mut outp,
                        })
                        .unwrap()
                };
                out.extend_from_slice(&obuf[..w.produced[0]]);
                if w.consumed[0] == 0 && w.produced[0] == 0 {
                    break; // stalled — no progress possible
                }
                idx += w.consumed[0];
            }
            out
        }
    };
}

pump!(
    pump_real_to_iq,
    f32,
    0.0_f32,
    InBuf::RealF32,
    Complex<f32>,
    Complex::new(0.0_f32, 0.0),
    OutBuf::IqF32
);
pump!(
    pump_iq_to_real,
    Complex<f32>,
    Complex::new(0.0_f32, 0.0),
    InBuf::IqF32,
    f32,
    0.0_f32,
    OutBuf::RealF32
);
pump!(
    pump_iq_to_iq,
    Complex<f32>,
    Complex::new(0.0_f32, 0.0),
    InBuf::IqF32,
    Complex<f32>,
    Complex::new(0.0_f32, 0.0),
    OutBuf::IqF32
);
pump!(
    pump_real_to_real,
    f32,
    0.0_f32,
    InBuf::RealF32,
    f32,
    0.0_f32,
    OutBuf::RealF32
);

/// Result of aligning a recovered signal against its reference.
#[derive(Debug, Clone, Copy)]
pub struct Score {
    /// Integer lag (in samples) of `recovered` behind `reference` at
    /// the cross-correlation peak.
    pub lag: isize,
    /// Least-squares gain mapping reference → recovered.
    pub gain: f32,
    /// Pearson correlation over the aligned, mean-subtracted overlap.
    /// 1.0 = perfect, 0.0 = uncorrelated.
    pub rho: f32,
    /// Mean segmental SNR (dB) of reference vs gain-matched recovered.
    pub seg_snr_db: f32,
}

fn pearson(a: &[f32], b: &[f32]) -> (f32, f32) {
    let n = a.len().min(b.len());
    debug_assert!(n > 0);
    let nf = n as f64;
    let (mut sa, mut sb) = (0.0f64, 0.0f64);
    for i in 0..n {
        sa += f64::from(a[i]);
        sb += f64::from(b[i]);
    }
    let (ma, mb) = (sa / nf, sb / nf);
    let (mut cov, mut va, mut vb) = (0.0f64, 0.0f64, 0.0f64);
    for i in 0..n {
        let da = f64::from(a[i]) - ma;
        let db = f64::from(b[i]) - mb;
        cov += da * db;
        va += da * da;
        vb += db * db;
    }
    let rho = if va > 0.0 && vb > 0.0 {
        cov / (va.sqrt() * vb.sqrt())
    } else {
        0.0
    };
    // Least-squares gain a→b is cov / var(a).
    let gain = if va > 0.0 { cov / va } else { 0.0 };
    (rho as f32, gain as f32)
}

/// Cross-correlate `reference` against `recovered` over lags in
/// `[-max_lag, max_lag]`, pick the peak, then score the full aligned
/// overlap. A centred window of `reference` is used for the lag search
/// so it is O(window · lag), not O(N · lag).
pub fn align_and_score(reference: &[f32], recovered: &[f32], max_lag: usize) -> Score {
    assert!(!reference.is_empty() && !recovered.is_empty());

    // Lag search on a centred window, skipping the chain's startup
    // transient (first eighth) on both ends.
    let warm_r = reference.len() / 8;
    let win = 16_384
        .min(reference.len().saturating_sub(warm_r) / 2)
        .max(1);
    let r0 = warm_r;
    let ref_win = &reference[r0..(r0 + win).min(reference.len())];

    let mut best_lag = 0isize;
    let mut best_abs = -1.0f32;
    let lo = -(max_lag as isize);
    let hi = max_lag as isize;
    for lag in lo..=hi {
        // recovered index for reference index r0 is r0 + lag
        let start = r0 as isize + lag;
        if start < 0 {
            continue;
        }
        let start = start as usize;
        if start + ref_win.len() > recovered.len() {
            continue;
        }
        let (rho, _) = pearson(ref_win, &recovered[start..start + ref_win.len()]);
        if rho.abs() > best_abs {
            best_abs = rho.abs();
            best_lag = lag;
        }
    }

    // Score the full overlap at the chosen lag, skipping warm-up.
    let (ref_off, rec_off) = if best_lag >= 0 {
        (0usize, best_lag as usize)
    } else {
        ((-best_lag) as usize, 0usize)
    };
    let ref_off = ref_off + reference.len() / 8;
    let rec_off = rec_off + reference.len() / 8;
    let overlap =
        (reference.len().saturating_sub(ref_off)).min(recovered.len().saturating_sub(rec_off));
    assert!(overlap > 1024, "no usable overlap after alignment");
    let a = &reference[ref_off..ref_off + overlap];
    let b = &recovered[rec_off..rec_off + overlap];
    let (rho, gain) = pearson(a, b);

    // Segmental SNR of reference vs gain-matched recovered.
    let seg = 4_096usize;
    let mut acc = 0.0f64;
    let mut nseg = 0usize;
    let mut i = 0;
    while i + seg <= overlap {
        let (mut sig, mut err) = (0.0f64, 0.0f64);
        for k in 0..seg {
            let r = f64::from(a[i + k]);
            let e = r - f64::from(gain) * f64::from(b[i + k]);
            sig += r * r;
            err += e * e;
        }
        if sig > 1e-12 && err > 1e-12 {
            acc += 10.0 * (sig / err).log10();
            nseg += 1;
        }
        i += seg;
    }
    let seg_snr_db = if nseg > 0 {
        (acc / nseg as f64) as f32
    } else {
        0.0
    };

    Score {
        lag: best_lag,
        gain,
        rho,
        seg_snr_db,
    }
}

/// Generate a deterministic multitone + slow chirp "voice-like" message
/// at `rate_hz` for `secs`, peak-normalised to `peak`. Multitone =
/// well-defined spectrum; the chirp sweeps so a frequency-dependent
/// chain defect (group-delay ripple, aliasing) shows up as lost ρ.
pub fn synthetic_message(rate_hz: f64, secs: f64, peak: f32) -> Vec<f32> {
    let n = (rate_hz * secs) as usize;
    let tones = [220.0f64, 770.0, 1_650.0, 2_900.0];
    let mut v = Vec::with_capacity(n);
    let mut maxa = 0.0f32;
    for i in 0..n {
        let t = i as f64 / rate_hz;
        let mut s = 0.0f64;
        for (k, f) in tones.iter().enumerate() {
            s += (1.0 / (k as f64 + 1.0)) * (std::f64::consts::TAU * f * t).sin();
        }
        // Slow chirp 300→3000 Hz over the whole clip.
        let fch = 300.0 + 2_700.0 * (t / secs);
        s += 0.6 * (std::f64::consts::TAU * fch * t).sin();
        let x = s as f32;
        maxa = maxa.max(x.abs());
        v.push(x);
    }
    let g = if maxa > 0.0 { peak / maxa } else { 1.0 };
    for x in &mut v {
        *x *= g;
    }
    v
}
