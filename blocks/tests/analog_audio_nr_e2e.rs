//! End-to-end audio chain tests for each analog mode that now runs
//! through `AudioNrMono` / `AudioNrStereo`.
//!
//! Follows the `wbfm_e2e.rs` pattern: synthesise a deterministic IQ
//! fixture, run it through the actual blocks the shipped preset
//! instantiates, and Goertzel-check the output audio for the expected
//! signal. Each test exercises one analog-mode preset's NR defaults:
//!
//! | test                                         | chain                                                  |
//! |----------------------------------------------|--------------------------------------------------------|
//! | `wbfm_mono_demod_through_audio_nr`           | FM-mod → FmDemod → AudioNrMono (deemph)                |
//! | `wbfm_stereo_decode_through_audio_nr`        | MPX → StereoDecoder → AudioNrStereo (deemph)           |
//! | `nbfm_audio_nr_blanker_kills_injected_spike` | NFM + spike → FmDemod → AudioNrMono (blanker)          |
//!
//! SSB (usb/lsb preset) is intentionally *not* covered by a Goertzel
//! fixture here. The ham NR chain (blanker + adaptive notch +
//! MMSE-LSA spectral) is designed for broadband speech + noise; a
//! synthetic tone + noise fixture couples those stages in ways that
//! are more about the fixture than the product — e.g. the adaptive
//! notch's job is literally "find and null narrowband tones." Each
//! stage is unit-tested at the algorithm level in `audio_nr::*`
//! (blanker zeros impulses, notch suppresses a tone in noise by
//! ≥15 dB, MMSE-LSA attenuates white noise ≥3 dB, RNNoise does ≥2 dB
//! on 48 k); end-to-end SSB behaviour is validated live by ear.
//!
//! Neural (RNNoise) is deliberately off in all tests below — it's
//! unit-tested in `audio_nr::neural`, requires exact 48 kHz, and has
//! warmup latency that would complicate golden-fixture assertions
//! without adding useful coverage.
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]

use core::f32::consts::TAU;

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{
    AudioNrMono, AudioNrParams, AudioNrStereo, Block, FmDemod, FmDemodParams, Sideband, SsbDemod,
    SsbDemodParams, StereoDecoder, StereoDecoderParams,
};
use num_complex::Complex;

// ---------------------------------------------------------------------------
// Helpers — shared across tests
// ---------------------------------------------------------------------------

/// Single-bin DFT magnitude at `target_hz`.
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

/// Apply FM modulation: given a baseband message `m(t)`, emit
/// `x(t) = exp(j · 2π · Δf · ∫m)`.
fn fm_modulate(message: &[f32], fs: f32, deviation_hz: f32) -> Vec<Complex<f32>> {
    let dt = 1.0 / fs;
    let mut phase = 0.0_f32;
    message
        .iter()
        .map(|m| {
            phase += TAU * deviation_hz * m * dt;
            Complex::new(phase.cos(), phase.sin())
        })
        .collect()
}

/// Drive an FmDemod over an IQ slice, return the real audio output.
fn run_fm_demod(demod: &mut FmDemod, iq: &[Complex<f32>]) -> Vec<f32> {
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
    demod.process(&mut io).unwrap();
    out
}

/// Drive an SsbDemod over an IQ slice, return the real audio output.
fn run_ssb_demod(demod: &mut SsbDemod, iq: &[Complex<f32>]) -> Vec<f32> {
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
    demod.process(&mut io).unwrap();
    out
}

/// Run an `AudioNrMono` over a real-float slice.
fn run_audio_nr_mono(nr: &mut AudioNrMono, input: &[f32]) -> Vec<f32> {
    let mut out = vec![0.0_f32; input.len()];
    let mut inputs = [InputPort {
        name: "in",
        meta: PortMeta::default(),
        buf: InBuf::RealF32(input),
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
    nr.process(&mut io).unwrap();
    out
}

/// Run a `StereoDecoder`: mono composite in, `(left, right)` out. The
/// `events` port is allocated but drained into a junk buffer — we're
/// not asserting on pilot-lock events here.
fn run_stereo_decoder(dec: &mut StereoDecoder, mpx: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let n = mpx.len();
    let mut left = vec![0.0_f32; n];
    let mut right = vec![0.0_f32; n];
    let mut events = vec![0_u8; 4096];
    let mut inputs = [InputPort {
        name: "in",
        meta: PortMeta::default(),
        buf: InBuf::RealF32(mpx),
    }];
    let mut outputs = [
        OutputPort {
            name: "left",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut left),
        },
        OutputPort {
            name: "right",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut right),
        },
        OutputPort {
            name: "events",
            meta: PortMeta::default(),
            buf: OutBuf::Events(&mut events),
        },
    ];
    let mut io = BlockIo {
        inputs: &mut inputs,
        outputs: &mut outputs,
    };
    dec.process(&mut io).unwrap();
    (left, right)
}

/// Run `AudioNrStereo` with separate L/R inputs and outputs.
fn run_audio_nr_stereo(
    nr: &mut AudioNrStereo,
    left_in: &[f32],
    right_in: &[f32],
) -> (Vec<f32>, Vec<f32>) {
    let n = left_in.len();
    let mut left_out = vec![0.0_f32; n];
    let mut right_out = vec![0.0_f32; n];
    let mut inputs = [
        InputPort {
            name: "left",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(left_in),
        },
        InputPort {
            name: "right",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(right_in),
        },
    ];
    let mut outputs = [
        OutputPort {
            name: "left",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut left_out),
        },
        OutputPort {
            name: "right",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut right_out),
        },
    ];
    let mut io = BlockIo {
        inputs: &mut inputs,
        outputs: &mut outputs,
    };
    nr.process(&mut io).unwrap();
    (left_out, right_out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// wbfm.json mono preset: FmDemod → AudioNrMono with deemph.
///
/// Drives a 1 kHz mono tone, FM-modulates at ±75 kHz deviation, runs
/// the same chain the preset builds. Asserts (a) the 1 kHz tone
/// dominates its Goertzel bin, (b) a high-frequency tone above the
/// 75 µs corner is attenuated, demonstrating deemph is engaged.
#[test]
fn wbfm_mono_demod_through_audio_nr() {
    const FS: f32 = 240_000.0;
    const DEVIATION: f32 = 75_000.0;
    const TONE_LO: f32 = 1_000.0;
    const TONE_HI: f32 = 15_000.0; // well above the 75µs corner (~2.1 kHz)
    const DURATION_S: f32 = 0.5;
    let n = (FS * DURATION_S) as usize;

    // Two-tone baseband: a loud 1 kHz mono + a quieter 15 kHz tone to
    // probe the deemph response.
    let message: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / FS;
            0.4 * (TAU * TONE_LO * t).sin() + 0.4 * (TAU * TONE_HI * t).sin()
        })
        .collect();

    let iq = fm_modulate(&message, FS, DEVIATION);

    let mut demod = FmDemod::new(FmDemodParams {
        sample_rate_hz: FS,
        max_deviation_hz: DEVIATION,
    })
    .unwrap();
    let demod_out = run_fm_demod(&mut demod, &iq);

    let mut nr = AudioNrMono::new(AudioNrParams {
        sample_rate_hz: FS,
        deemph_enable: true,
        deemph_tau_us: 75.0,
        ..Default::default()
    })
    .unwrap();
    let audio = run_audio_nr_mono(&mut nr, &demod_out);

    // Skip the first sample (FM discriminator) and give the deemph IIR
    // a few hundred samples to settle.
    let tail = &audio[512..];

    // (a) 1 kHz tone still dominates.
    let lo_mag = goertzel(tail, TONE_LO, FS);
    let lo_noise = goertzel(tail, TONE_LO + 500.0, FS);
    assert!(
        lo_mag > 20.0 * lo_noise,
        "1 kHz tone not dominant post-NR: mag={lo_mag:.3}, neighbour={lo_noise:.3}"
    );

    // (b) 15 kHz tone is measurably attenuated relative to the 1 kHz
    // tone. Pre-deemph the two tones had equal amplitude in the
    // message; at 15 kHz the 75 µs deemph LPF drops them by ~17 dB
    // relative to 1 kHz. Assert > 6 dB separation with margin.
    let hi_mag = goertzel(tail, TONE_HI, FS);
    let ratio_db = 20.0 * (lo_mag / hi_mag.max(1e-9)).log10();
    assert!(
        ratio_db > 6.0,
        "deemph not engaged: 1k/15k ratio only {ratio_db:.1} dB (lo={lo_mag:.3}, hi={hi_mag:.3})"
    );
}

/// wbfm_stereo.json audio back-half: StereoDecoder → AudioNrStereo.
///
/// FM modulation + demodulation is already covered by `wbfm_e2e.rs`
/// and the FmDemod unit tests. This test drives a broadcast MPX
/// composite *directly* into StereoDecoder so the assertions can
/// focus on what this e2e adds over the unit tests: the stereo NR
/// block preserving L/R separation after the decode. Matches the
/// MPX formula from `stereo_decoder` tests exactly.
#[test]
fn wbfm_stereo_decode_through_audio_nr() {
    const FS: f32 = 240_000.0;
    const PILOT_HZ: f32 = 19_000.0;
    const SUBCARRIER_HZ: f32 = 38_000.0;
    const L_TONE: f32 = 800.0;
    const R_TONE: f32 = 2_000.0;
    let n = 32_768_usize;

    // Textbook MPX: sum + diff·cos(2ω_p·t) + pilot·cos(ω_p·t). Unit
    // amplitude tones so the raw StereoDecoder output is in the ~1
    // range — comfortably above the deemph IIR's rounding floor.
    let mpx: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / FS;
            let l = (TAU * L_TONE * t).cos();
            let r = (TAU * R_TONE * t).cos();
            let sum = l + r;
            let diff = l - r;
            let carrier = (TAU * SUBCARRIER_HZ * t).cos();
            let pilot = 0.1 * (TAU * PILOT_HZ * t).cos();
            sum + diff * carrier + pilot
        })
        .collect();

    let mut stereo = StereoDecoder::new(StereoDecoderParams {
        sample_rate_hz: FS,
        lock_threshold: 0.02,
        lock_tau_ms: 10.0, // fast lock so blend-ramp settles inside
        // the fixture
        emit_interval_ms: 100.0,
    })
    .unwrap();
    let (l_raw, r_raw) = run_stereo_decoder(&mut stereo, &mpx);

    let mut nr = AudioNrStereo::new(AudioNrParams {
        sample_rate_hz: FS,
        deemph_enable: true,
        deemph_tau_us: 75.0,
        ..Default::default()
    })
    .unwrap();
    let (l_audio, r_audio) = run_audio_nr_stereo(&mut nr, &l_raw, &r_raw);

    // Skip the PLL acquisition + blend-ramp transient, then
    // dot-product each output against reference tones. A channel
    // that "carries its own tone" has a big own-dot and a small
    // cross-dot. 5× separation matches the existing stereo_decoder
    // test's bar.
    let skip = 8_192;
    let ref_l: Vec<f32> = (0..n)
        .map(|i| (TAU * L_TONE * i as f32 / FS).cos())
        .collect();
    let ref_r: Vec<f32> = (0..n)
        .map(|i| (TAU * R_TONE * i as f32 / FS).cos())
        .collect();
    let dot = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
    let l_own = dot(&l_audio[skip..], &ref_l[skip..]).abs();
    let l_cross = dot(&l_audio[skip..], &ref_r[skip..]).abs();
    let r_own = dot(&r_audio[skip..], &ref_r[skip..]).abs();
    let r_cross = dot(&r_audio[skip..], &ref_l[skip..]).abs();
    assert!(
        l_own > 5.0 * l_cross,
        "L channel not isolated after NR: L·refL={l_own:.1}, L·refR={l_cross:.1}"
    );
    assert!(
        r_own > 5.0 * r_cross,
        "R channel not isolated after NR: R·refR={r_own:.1}, R·refL={r_cross:.1}"
    );
}

/// nbfm.json preset: FmDemod → AudioNrMono with impulse blanker on.
///
/// Generates a narrow-FM tone, injects a single huge-amplitude impulse
/// directly into the demod's output stream (simulating what a
/// real-world click would look like post-demod), and asserts the
/// blanker wipes it without damaging the surrounding audio.
#[test]
fn nbfm_audio_nr_blanker_kills_injected_spike() {
    const FS: f32 = 48_000.0; // NBFM audio after the resampler
    const TONE: f32 = 1_000.0;
    let n = 4_096_usize;

    // Low-amplitude steady tone so the envelope follower has a stable
    // reference, then one outlier sample 30× above the envelope.
    let mut audio_in: Vec<f32> = (0..n)
        .map(|i| 0.1 * (TAU * TONE * i as f32 / FS).sin())
        .collect();
    let spike_idx = 2_000_usize;
    audio_in[spike_idx] = 3.0;

    let mut nr = AudioNrMono::new(AudioNrParams {
        sample_rate_hz: FS,
        blanker_enable: true,
        blanker_threshold_db: 20.0,
        blanker_hold_ms: 0.5,
        ..Default::default()
    })
    .unwrap();
    let out = run_audio_nr_mono(&mut nr, &audio_in);

    // Spike sample itself is zeroed (the blanker pulled it to 0 for
    // the hold window).
    assert!(
        out[spike_idx].abs() < 1e-6,
        "spike not blanked: out[{spike_idx}] = {}",
        out[spike_idx]
    );
    // Beyond the hold window (0.5 ms @ 48 k ≈ 24 samples), output
    // recovers and the 1 kHz tone is intact.
    let recovery_start = spike_idx + 200; // ample margin past the hold
    let recovery_end = recovery_start + 1_024;
    let recovered = &out[recovery_start..recovery_end];
    let mag = goertzel(recovered, TONE, FS);
    let noise = goertzel(recovered, TONE + 500.0, FS);
    assert!(
        mag > 10.0 * noise,
        "tone not recovered after blank: mag@1k={mag:.3}, neighbour={noise:.3}"
    );
}
