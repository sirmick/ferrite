//! Heavyweight end-to-end audio-modes regression.
//!
//! For every analog voice mode Ferrite ships, synthesize a known
//! multi-tone "audio" message, modulate it to IQ with textbook math,
//! push it through the *real* demod block, and analyse the recovered
//! audio against the original:
//!
//! | mode  | modulation                              | demod      |
//! |-------|-----------------------------------------|------------|
//! | AM    | (1 + m·msg)·e^{jωc t}, sweep ωc         | `AmDemod`  |
//! | WBFM  | e^{j2π·75k·∫msg} @ 240 k                 | `FmDemod`  |
//! | NBFM  | e^{j2π·2.5k·∫msg} @ 24 k                 | `FmDemod`  |
//! | USB   | analytic msg  Σ Aₖ·e^{+jωₖt}            | `SsbDemod` |
//! | LSB   | analytic msg  Σ Aₖ·e^{-jωₖt}            | `SsbDemod` |
//!
//! NR (AudioNr) is intentionally out of scope here — it is a lossy
//! tradeoff with no clean reference contract; covered separately.
//!
//! Each assertion is scale-independent where the demod gain is
//! implementation-defined: we check (a) every message tone is recovered
//! well above the inter-tone noise floor (a SINAD proxy), (b) the
//! *relative* levels of the tones are preserved (a demod that distorts
//! tilts or intermods them), and (c) output is finite. The AM case
//! additionally sweeps the carrier offset — that is the guard that the
//! coherent-`ampmodem` regression would have failed (its PLL pulled in
//! only ~±500 Hz; envelope detection is offset-immune).

use core::f32::consts::TAU;

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{
    AmDemod, AmDemodParams, Block, FmDemod, FmDemodParams, Sideband, SsbDemod, SsbDemodParams,
};
use num_complex::Complex;

// ---------------------------------------------------------------------------
// Test signal + analysis
// ---------------------------------------------------------------------------

/// One message tone: frequency (Hz) and linear amplitude.
#[derive(Clone, Copy)]
struct Tone {
    hz: f32,
    amp: f32,
}

/// Sum-of-sines message, peak-normalised to `peak` so AM's
/// `(1 + m·msg)` stays positive and FM stays inside Nyquist.
fn message(tones: &[Tone], fs: f32, n: usize, peak: f32) -> Vec<f32> {
    let raw: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / fs;
            tones
                .iter()
                .map(|tn| tn.amp * (TAU * tn.hz * t).sin())
                .sum()
        })
        .collect();
    let max = raw.iter().fold(0.0_f32, |a, &x| a.max(x.abs())).max(1e-9);
    let g = peak / max;
    raw.iter().map(|x| x * g).collect()
}

/// Single-bin DFT magnitude at `target_hz`.
fn goertzel(samples: &[f32], target_hz: f32, fs: f32) -> f32 {
    let n = samples.len() as f32;
    let k = (n * target_hz / fs).round();
    let omega = TAU * k / n;
    let coeff = 2.0 * omega.cos();
    let (mut q1, mut q2) = (0.0_f32, 0.0_f32);
    for &s in samples {
        let q0 = coeff * q1 - q2 + s;
        q2 = q1;
        q1 = q0;
    }
    (q1 * q1 + q2 * q2 - coeff * q1 * q2).max(0.0).sqrt()
}

fn rms(xs: &[f32]) -> f32 {
    (xs.iter().map(|x| x * x).sum::<f32>() / xs.len() as f32).sqrt()
}

/// Assert: every tone recovered well above the local noise floor, and
/// the tones' relative levels match the source within `ratio_tol_db`.
/// `label` only colours the panic message.
fn assert_faithful(out: &[f32], fs: f32, tones: &[Tone], min_sinad_db: f32, ratio_tol_db: f32) {
    assert!(
        out.iter().all(|x| x.is_finite()),
        "{}: non-finite output sample",
        "audio",
    );
    assert!(
        rms(out) > 1e-4,
        "output effectively silent (rms {:.2e})",
        rms(out)
    );

    // Per-tone magnitude + an off-tone neighbour as the noise/distortion
    // proxy (250 Hz away, clear of every message tone).
    let mut mags = Vec::new();
    for tn in tones {
        let sig = goertzel(out, tn.hz, fs);
        let noise = goertzel(out, tn.hz + 250.0, fs).max(goertzel(out, tn.hz - 175.0, fs));
        let sinad = 20.0 * (sig / noise.max(1e-9)).log10();
        assert!(
            sinad > min_sinad_db,
            "tone {} Hz SINAD {:.1} dB < {:.1} dB (sig={:.4} noise={:.4})",
            tn.hz,
            sinad,
            min_sinad_db,
            sig,
            noise,
        );
        mags.push(sig);
    }

    // Relative-level fidelity: normalise both source and recovered tone
    // vectors by their first element and compare in dB. A faithful
    // linear demod preserves the ratios; distortion/intermod tilts them.
    for (idx, tn) in tones.iter().enumerate() {
        let src_db = 20.0 * (tn.amp / tones[0].amp).log10();
        let got_db = 20.0 * (mags[idx] / mags[0]).log10();
        assert!(
            (src_db - got_db).abs() < ratio_tol_db,
            "tone {} Hz relative level off by {:.1} dB (src {:.1} dB, got {:.1} dB) — \
             demod is distorting the spectrum",
            tn.hz,
            (src_db - got_db).abs(),
            src_db,
            got_db,
        );
    }
}

// ---------------------------------------------------------------------------
// Modulators (textbook, deterministic — match each demod's contract)
// ---------------------------------------------------------------------------

fn am_modulate(msg: &[f32], fs: f32, m: f32, carrier_off_hz: f32) -> Vec<Complex<f32>> {
    msg.iter()
        .enumerate()
        .map(|(i, &s)| {
            let t = i as f32 / fs;
            let env = 1.0 + m * s;
            let ph = TAU * carrier_off_hz * t;
            Complex::new(env * ph.cos(), env * ph.sin())
        })
        .collect()
}

fn fm_modulate(msg: &[f32], fs: f32, deviation_hz: f32) -> Vec<Complex<f32>> {
    let dt = 1.0 / fs;
    let mut phase = 0.0_f32;
    msg.iter()
        .map(|&s| {
            phase += TAU * deviation_hz * s * dt;
            Complex::new(phase.cos(), phase.sin())
        })
        .collect()
}

/// SSB at baseband = analytic message. Built directly from the known
/// tones so the Hilbert is exact: USB → Σ Aₖ·e^{+jωₖt}, LSB → conj.
fn ssb_modulate(tones: &[Tone], fs: f32, n: usize, peak: f32, sb: Sideband) -> Vec<Complex<f32>> {
    // Re-derive the same peak-normalisation gain the real message uses
    // so amplitudes line up with `tones`.
    let msg = message(tones, fs, n, peak);
    let raw_peak = {
        let raw: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                tones
                    .iter()
                    .map(|tn| tn.amp * (TAU * tn.hz * t).sin())
                    .sum()
            })
            .collect();
        raw.iter().fold(0.0_f32, |a, &x| a.max(x.abs())).max(1e-9)
    };
    let g = peak / raw_peak;
    let _ = msg; // message() kept for parity / future correlation checks
    (0..n)
        .map(|i| {
            let t = i as f32 / fs;
            let sign = match sb {
                Sideband::Usb => 1.0,
                Sideband::Lsb => -1.0,
            };
            let mut z = Complex::new(0.0, 0.0);
            for tn in tones {
                let ph = sign * TAU * tn.hz * t;
                z += Complex::from_polar(tn.amp * g, ph);
            }
            z
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Block drivers
// ---------------------------------------------------------------------------

fn drive_iq_real<B: Block>(block: &mut B, iq: &[Complex<f32>]) -> Vec<f32> {
    let mut out = vec![0.0_f32; iq.len()];
    let mut inputs = [InputPort {
        name: "in",
        meta: PortMeta::default(),
        buf: InBuf::IqF32(iq),
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
    block.process(&mut io).unwrap();
    out
}

// ---------------------------------------------------------------------------
// AM — multitone, carrier-offset sweep (the regression guard)
// ---------------------------------------------------------------------------

#[test]
fn am_envelope_recovers_multitone_at_every_offset() {
    let fs = 12_000.0_f32;
    let n = 24_000;
    let tones = [
        Tone {
            hz: 300.0,
            amp: 1.0,
        },
        Tone {
            hz: 1_200.0,
            amp: 0.6,
        },
        Tone {
            hz: 2_400.0,
            amp: 0.35,
        },
    ];
    let msg = message(&tones, fs, n, 0.9);

    for off in [0.0_f32, 50.0, 500.0, 1_500.0, 3_000.0] {
        let iq = am_modulate(&msg, fs, 0.8, off);
        let mut demod = AmDemod::new(AmDemodParams {
            sample_rate_hz: fs,
            audio_gain: 1.0,
        })
        .unwrap();
        let out = drive_iq_real(&mut demod, &iq);
        let tail = &out[n / 4..]; // skip DC-tracker settle
        assert_faithful(tail, fs, &tones, 22.0, 3.0);
        let _ = off;
    }
}

// ---------------------------------------------------------------------------
// WBFM + NBFM
// ---------------------------------------------------------------------------

#[test]
fn wbfm_recovers_multitone() {
    let fs = 240_000.0_f32;
    let n = (fs * 0.4) as usize;
    let tones = [
        Tone {
            hz: 400.0,
            amp: 1.0,
        },
        Tone {
            hz: 1_500.0,
            amp: 0.6,
        },
        Tone {
            hz: 4_000.0,
            amp: 0.3,
        },
    ];
    let msg = message(&tones, fs, n, 0.7);
    let iq = fm_modulate(&msg, fs, 75_000.0);
    let mut demod = FmDemod::new(FmDemodParams {
        sample_rate_hz: fs,
        max_deviation_hz: 75_000.0,
    })
    .unwrap();
    let out = drive_iq_real(&mut demod, &iq);
    assert_faithful(&out[1024..], fs, &tones, 20.0, 3.0);
}

#[test]
fn nbfm_recovers_voice_multitone() {
    let fs = 24_000.0_f32;
    let n = (fs * 0.8) as usize;
    let tones = [
        Tone {
            hz: 350.0,
            amp: 1.0,
        },
        Tone {
            hz: 1_000.0,
            amp: 0.7,
        },
        Tone {
            hz: 2_500.0,
            amp: 0.4,
        },
    ];
    let msg = message(&tones, fs, n, 0.7);
    let iq = fm_modulate(&msg, fs, 2_500.0);
    let mut demod = FmDemod::new(FmDemodParams {
        sample_rate_hz: fs,
        max_deviation_hz: 2_500.0,
    })
    .unwrap();
    let out = drive_iq_real(&mut demod, &iq);
    assert_faithful(&out[1024..], fs, &tones, 18.0, 3.5);
}

// ---------------------------------------------------------------------------
// SSB — USB + LSB, opposite-sideband rejection
// ---------------------------------------------------------------------------

fn ssb_case(sb: Sideband) {
    let fs = 48_000.0_f32;
    let n = 48_000;
    // Stay clear of liquid ampmodem's DC-block corner — no sub-~500 Hz
    // tones for SSB.
    let tones = [
        Tone {
            hz: 700.0,
            amp: 1.0,
        },
        Tone {
            hz: 1_600.0,
            amp: 0.6,
        },
        Tone {
            hz: 2_800.0,
            amp: 0.35,
        },
    ];
    let iq = ssb_modulate(&tones, fs, n, 0.9, sb);
    let mut demod = SsbDemod::new(SsbDemodParams {
        sample_rate_hz: fs,
        sideband: sb,
        audio_gain: 1.0,
    })
    .unwrap();
    let out = drive_iq_real(&mut demod, &iq);
    assert_faithful(&out[1024..], fs, &tones, 20.0, 3.0);

    // Wrong-sideband input must be heavily rejected.
    let other = match sb {
        Sideband::Usb => Sideband::Lsb,
        Sideband::Lsb => Sideband::Usb,
    };
    let wrong = ssb_modulate(&tones, fs, n, 0.9, other);
    let mut demod2 = SsbDemod::new(SsbDemodParams {
        sample_rate_hz: fs,
        sideband: sb,
        audio_gain: 1.0,
    })
    .unwrap();
    let rej = drive_iq_real(&mut demod2, &wrong);
    let on = rms(&out[1024..]);
    let off = rms(&rej[1024..]);
    let rej_db = 20.0 * (on / off.max(1e-9)).log10();
    assert!(
        rej_db > 30.0,
        "{sb:?}: opposite-sideband rejection only {rej_db:.1} dB",
    );
}

#[test]
fn usb_recovers_multitone_and_rejects_lsb() {
    ssb_case(Sideband::Usb);
}

#[test]
fn lsb_recovers_multitone_and_rejects_usb() {
    ssb_case(Sideband::Lsb);
}
