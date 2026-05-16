//! End-to-end NBFM decode of a real off-air IQ capture, gated on the
//! CTCSS sub-audible tone.
//!
//! `samples/uhf/ctcss_482.768mhz_iq-s16.wav` (156 250 Hz stereo s16,
//! L=I R=Q, ~31 s) is a real narrowband-FM voice transmission carrying
//! a CTCSS (Continuous Tone-Coded Squelch System) sub-audible tone.
//! The chain — `FileIqSource → Channelizer → FmDemod` — is the exact
//! one a listen preset runs; the assertion is that a *standard EIA
//! CTCSS tone* (67.0–250.3 Hz) emerges and dominates the sub-audible
//! band. That is a sharp, deterministic gate: a broken tuner, wrong
//! decimation, or an FM-discriminator regression all destroy the
//! narrow tone long before they'd noticeably hurt voice intelligibility.
//!
//! This is the "Channelizer tune-back with a real capture" path the
//! `modulated_source_e2e` header flagged as the natural next step.
//!
//! **Fixture caveat:** `/samples/uhf/` is `.gitignore`d scratch (no
//! established licence — `samples/README.md` forbids committing such
//! captures). So this test *skips* (passes, with a notice) when the
//! WAV is absent — CI stays green; anyone with the local capture gets
//! the full real-RF gate. If the capture is ever curated + licensed,
//! drop the skip guard to make it mandatory.

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

mod common;

use common::{init_at, pump_iq_to_real, sample_path};
use ferrite_blocks::block::{Block, BlockIo, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{
    Channelizer, ChannelizerParams, FileIqSource, FileIqSourceParams, FmDemod, FmDemodParams,
};
use num_complex::Complex;

const CAPTURE: &str = "uhf/ctcss_482.768mhz_iq-s16.wav";
const IN_RATE: f64 = 156_250.0;
/// 156250 / round(156250/20000) = /8 = 19531.25 Hz NBFM channel.
const CH_RATE: f64 = 156_250.0 / 8.0;
const NBFM_DEV: f32 = 5_000.0;

/// EIA standard 38-tone CTCSS set (Hz).
const CTCSS: &[f32] = &[
    67.0, 71.9, 74.4, 77.0, 79.7, 82.5, 85.4, 88.5, 91.5, 94.8, 97.4, 100.0, 103.5, 107.2, 110.9,
    114.8, 118.8, 123.0, 127.3, 131.8, 136.5, 141.3, 146.2, 151.4, 156.7, 162.2, 167.9, 173.8,
    179.9, 186.2, 192.8, 203.5, 210.7, 218.1, 225.7, 233.6, 241.8, 250.3,
];

fn load_iq() -> Vec<Complex<f32>> {
    let mut src = FileIqSource::new(&FileIqSourceParams {
        path: sample_path(CAPTURE),
        rate_hz_hint: 0.0,
        center_freq_hz: 482_768_000.0,
        loop_playback: false,
    })
    .expect("open CTCSS IQ capture");
    assert!(
        (src.rate_hz() - IN_RATE).abs() < 1.0,
        "expected {IN_RATE} Hz, header says {}",
        src.rate_hz()
    );
    let mut all = Vec::new();
    let mut buf = vec![Complex::new(0.0_f32, 0.0); 1 << 16];
    loop {
        let mut outs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut buf),
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
    all
}

/// Goertzel single-bin magnitude at `f` Hz over `xs` sampled at `fs`.
fn goertzel(xs: &[f32], f: f32, fs: f32) -> f32 {
    let k = xs.len() as f32 * f / fs;
    let w = std::f32::consts::TAU * k / xs.len() as f32;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in xs {
        let s0 = coeff * s1 - s2 + x;
        s2 = s1;
        s1 = s0;
    }
    (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / xs.len() as f32
}

/// Channelize at `shift`, FM-demod, return recovered audio at `CH_RATE`.
fn demod_at(iq: &[Complex<f32>], shift_hz: f64) -> Vec<f32> {
    let mut ch = Channelizer::new(ChannelizerParams::new(IN_RATE, shift_hz, CH_RATE)).unwrap();
    init_at(&mut ch, IN_RATE);
    let chan = common::pump_iq_to_iq(&mut ch, iq);

    let mut fm = FmDemod::new(FmDemodParams {
        sample_rate_hz: CH_RATE as f32,
        max_deviation_hz: NBFM_DEV,
    })
    .unwrap();
    init_at(&mut fm, CH_RATE);
    pump_iq_to_real(&mut fm, &chan)
}

/// Best (tone, frequency, ratio-vs-band) over the EIA set, where the
/// "band" reference is the median CTCSS-bin magnitude (a robust noise
/// floor that a real tone sticks far above).
fn best_ctcss(audio: &[f32]) -> (f32, f32, f32) {
    let warm = audio.len() / 8;
    let body = &audio[warm..];
    let mut mags: Vec<(f32, f32)> = CTCSS
        .iter()
        .map(|&f| (f, goertzel(body, f, CH_RATE as f32)))
        .collect();
    let mut sorted: Vec<f32> = mags.iter().map(|&(_, m)| m).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2].max(1e-12);
    mags.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
    let (f, m) = mags[0];
    (f, m, m / median)
}

/// The capture is tuned on the signal — it sits at DC, so no shift.
/// (Established by the probe sweep: CTCSS 173.8 Hz dominates ×34 at
/// shift 0 and stays the top tone across the whole ±2.5 kHz centre.)
const SIGNAL_SHIFT_HZ: f64 = 0.0;
/// The standard EIA CTCSS tone this transmitter keys.
const EXPECTED_TONE_HZ: f32 = 173.8;

#[test]
fn real_nbfm_capture_carries_a_standard_ctcss_tone() {
    if !sample_path(CAPTURE).exists() {
        eprintln!(
            "SKIP: {CAPTURE} absent (gitignored scratch). \
             Place the capture there to run the real-RF NBFM/CTCSS gate."
        );
        return;
    }
    let iq = load_iq();
    assert!(iq.len() > 1_000_000, "capture unexpectedly short");

    let audio = demod_at(&iq, SIGNAL_SHIFT_HZ);
    assert!(audio.iter().all(|x| x.is_finite()), "non-finite audio");

    let warm = audio.len() / 8;
    let body = &audio[warm..];
    let rms = (body.iter().map(|x| x * x).sum::<f32>() / body.len() as f32).sqrt();

    let (tone, _mag, ratio) = best_ctcss(&audio);
    println!("CTCSS {tone:.1} Hz dominates the sub-audible band ×{ratio:.1}; audioRMS {rms:.4}");

    // 1. The dominant sub-audible tone is *the* standard EIA tone this
    //    transmitter uses — not merely "some tone". Wrong tuning or a
    //    discriminator regression shifts/destroys it.
    assert!(
        (tone - EXPECTED_TONE_HZ).abs() < 0.1,
        "dominant CTCSS tone {tone:.1} Hz ≠ expected {EXPECTED_TONE_HZ} Hz"
    );
    // 2. It dominates the sub-audible band decisively. Observed ×34 at
    //    correct tuning; off-tuned runs collapse below ×8. Gate ×15
    //    leaves margin while still catching a broken RF→audio chain.
    assert!(
        ratio > 15.0,
        "CTCSS {tone:.1} Hz only ×{ratio:.1} over the sub-audible floor — \
         tuner / decimation / FM-discriminator regression"
    );
    // 3. Voice-band audio is actually present (not just the tone on
    //    silence). Observed ~0.31; floor at 0.1.
    assert!(
        rms > 0.1,
        "recovered audio RMS {rms:.4} too low — chain produced no voice"
    );
}
