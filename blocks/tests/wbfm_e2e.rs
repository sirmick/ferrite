//! Golden-fixture WBFM end-to-end smoke test.
//!
//! Synthesises a wideband-FM IQ signal at 240 kHz containing a mono 1 kHz
//! tone plus a 19 kHz stereo pilot, runs it through `FmDemod`, and asserts
//! that the demodulated output reproduces both tones at the expected
//! amplitudes — Goertzel bins at the mono and pilot frequencies must
//! dominate adjacent off-signal bins, and overall RMS must track the
//! original baseband message within 10 %.
//!
//! Why a synthetic fixture instead of a checked-in broadcast capture:
//! real WBFM recordings are large, licence-encumbered, and time-variable.
//! A parametric fixture exercises the same demodulator with deterministic
//! assertions and no binary dependency.
//!
//! Scope note: this covers the IQ-to-audio slice that exists today. Once
//! a Real-typed decimator lands and the headless package (#91) can host a
//! full graph, this test will extend to the full WBFM chain. The "end to
//! end" framing still applies — fixture-in, audio-out, with no mocks of
//! the block trait.
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]

use core::f32::consts::TAU;

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{Block, FmDemod, FmDemodParams};
use num_complex::Complex;

fn run_fm_demod(demod: &mut FmDemod, input: &[Complex<f32>]) -> Vec<f32> {
    let mut out = vec![0.0_f32; input.len()];
    let mut inputs = [InputPort {
        name: "in",
        meta: PortMeta::default(),
        buf: InBuf::IqF32(input),
    }];
    let mut outputs = [OutputPort {
        name: "out",
        meta: PortMeta::default(),
        buf: OutBuf::RealF32(&mut out),
    }];
    let mut io = BlockIo {
        inputs: &mut inputs,
        outputs: &mut outputs,
    };
    demod.process(&mut io).unwrap();
    out
}

/// Single-bin DFT magnitude via Goertzel at `target_hz`. Returns
/// `|X(k)|` where `k = round(N · target / fs)`.
fn goertzel(samples: &[f32], target_hz: f32, sample_rate_hz: f32) -> f32 {
    let n = samples.len() as f32;
    let k = (n * target_hz / sample_rate_hz).round();
    let omega = TAU * k / n;
    let coeff = 2.0 * omega.cos();
    let mut q1 = 0.0_f32;
    let mut q2 = 0.0_f32;
    for &s in samples {
        let q0 = coeff * q1 - q2 + s;
        q2 = q1;
        q1 = q0;
    }
    (q1 * q1 + q2 * q2 - coeff * q1 * q2).max(0.0).sqrt()
}

fn rms(xs: &[f32]) -> f32 {
    let sum_sq: f32 = xs.iter().map(|x| x * x).sum();
    (sum_sq / xs.len() as f32).sqrt()
}

#[test]
fn wbfm_demod_reproduces_mono_plus_pilot() {
    const FS: f32 = 240_000.0;
    const DEVIATION: f32 = 75_000.0;
    const MONO_HZ: f32 = 1_000.0;
    const PILOT_HZ: f32 = 19_000.0;
    // Amplitudes in baseband units — post-demod we expect the same values
    // back out, since the demodulator gain is `fs / (2π · deviation)` and
    // the modulator integrates `2π · deviation · m(t)`.
    const MONO_AMP: f32 = 0.5;
    const PILOT_AMP: f32 = 0.1;
    const DURATION_S: f32 = 0.25;
    let n = (FS * DURATION_S) as usize;

    // 1. Build the baseband message m(t) = mono + pilot.
    let message: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / FS;
            let mono = MONO_AMP * (TAU * MONO_HZ * t).sin();
            let pilot = PILOT_AMP * (TAU * PILOT_HZ * t).sin();
            mono + pilot
        })
        .collect();

    // 2. Integrate m to instantaneous phase φ(t), then emit x(t)=e^{jφ}.
    let mut phase = 0.0_f32;
    let dt = 1.0 / FS;
    let iq: Vec<Complex<f32>> = message
        .iter()
        .map(|m| {
            phase += TAU * DEVIATION * m * dt;
            Complex::new(phase.cos(), phase.sin())
        })
        .collect();

    // 3. Demodulate.
    let params = FmDemodParams {
        sample_rate_hz: FS,
        max_deviation_hz: DEVIATION,
    };
    let mut demod = FmDemod::new(params).unwrap();
    let audio = run_fm_demod(&mut demod, &iq);

    // Skip the first sample — prev is (0,0) at start, so the discriminator
    // can't compute a valid phase derivative there.
    let tail = &audio[1..];
    let msg_tail = &message[1..];

    // Assertion 1: overall RMS tracks the input message within 10 %.
    let expected_rms = rms(msg_tail);
    let actual_rms = rms(tail);
    assert!(
        (actual_rms - expected_rms).abs() / expected_rms < 0.1,
        "audio RMS mismatch: expected ≈{expected_rms:.4}, got {actual_rms:.4}",
    );

    // Assertion 2: pilot tone dominates the 19 kHz bin. Compare against a
    // clearly-off-signal neighbour 500 Hz away; the true bin should be at
    // least 20× stronger than any empty bin nearby.
    let pilot_mag = goertzel(tail, PILOT_HZ, FS);
    let pilot_noise = goertzel(tail, PILOT_HZ - 500.0, FS);
    assert!(
        pilot_mag > 20.0 * pilot_noise,
        "pilot not dominant: mag@19k={pilot_mag:.3}, neighbour@18.5k={pilot_noise:.3}",
    );

    // Assertion 3: mono tone dominates the 1 kHz bin with the expected
    // amplitude relationship — mono is 5× the pilot amplitude, so its
    // Goertzel magnitude should be roughly 5× too.
    let mono_mag = goertzel(tail, MONO_HZ, FS);
    let mono_noise = goertzel(tail, MONO_HZ + 500.0, FS);
    assert!(
        mono_mag > 20.0 * mono_noise,
        "mono tone not dominant: mag@1k={mono_mag:.3}, neighbour@1.5k={mono_noise:.3}",
    );
    let ratio = mono_mag / pilot_mag;
    let expected_ratio = MONO_AMP / PILOT_AMP;
    assert!(
        (ratio - expected_ratio).abs() / expected_ratio < 0.1,
        "mono/pilot ratio off: expected {expected_ratio:.2}, got {ratio:.2}",
    );
}
