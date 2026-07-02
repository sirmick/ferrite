//! Modulated-source end-to-end fidelity tests.
//!
//! The existing analog e2e tests (`am_e2e`, `wbfm_e2e`,
//! `audio_modes_e2e`) synthesise IQ inline with textbook math at a
//! single rate and assert spectral properties of the demod output.
//! This file closes the remaining gap: drive the *real* modulator
//! blocks from a real waveform (synthetic *and* a recorded
//! `samples/audio/*.wav` clip), push through the *real* demod + the
//! arbitrary resampler the listen presets use, and score the recovered
//! audio against the original with delay/gain-agnostic correlation.
//!
//! | path        | chain                                                   |
//! |-------------|---------------------------------------------------------|
//! | AM          | msg → `AmModulator` → `AmDemod` → `RealF32Resamp`       |
//! | NBFM/WBFM   | msg → `FmModulator` → `FmDemod` → `RealF32Resamp`       |
//! | IQ upmix    | IQ capture → `IqUpmix` → `IqUpmix`⁻¹ (NCO round-trip)   |
//!
//! Two tiers per analog mode: a **synthetic** multitone+chirp message
//! with a tight ρ gate (catches DSP regressions precisely) and a
//! **real recorded** clip with a looser gate (catches integration /
//! rate-mismatch bugs the clean tone won't). ρ is mean-subtracted so
//! an FM carrier-offset DC pedestal does not depress it.
//!
//! Scope note (mirrors `wbfm_e2e`'s): the `Channelizer` tune-back
//! variant — `…Modulator → IqUpmix → Channelizer → demod` — is the
//! natural next step; the NCO round-trip here proves the upmix stage
//! itself is lossless, which is the explicit bar for the IQ path.

mod common;

use common::{
    align_and_score, init_at, load_audio, pump_iq_to_iq, pump_iq_to_real, pump_real_to_iq,
    pump_real_to_real, sample_path, synthetic_message,
};
use ferrite_blocks::block::{Block, BlockIo, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{
    AmDemod, AmDemodParams, AmModulator, AmModulatorParams, FileIqSource, FileIqSourceParams,
    FmDemod, FmDemodParams, FmModulator, FmModulatorParams, IqUpmix, IqUpmixParams, RealF32Resamp,
    RealF32ResampParams,
};
use num_complex::Complex;

const AUDIO_RATE: f64 = 48_000.0;
const IF_RATE: f64 = 240_000.0;

/// msg(real @ AUDIO_RATE) → AmModulator → AmDemod → RealF32Resamp →
/// recovered(real @ AUDIO_RATE).
fn am_roundtrip(msg: &[f32], mod_depth: f32) -> Vec<f32> {
    let mut modu = AmModulator::new(AmModulatorParams {
        input_rate_hz: AUDIO_RATE as f32,
        output_rate_hz: IF_RATE as f32,
        offset_hz: 0.0,
        mod_depth,
    })
    .unwrap();
    let iq = pump_real_to_iq(&mut modu, msg);

    let mut demod = AmDemod::new(AmDemodParams {
        sample_rate_hz: IF_RATE as f32,
        audio_gain: 1.0,
    })
    .unwrap();
    init_at(&mut demod, IF_RATE);
    let if_audio = pump_iq_to_real(&mut demod, &iq);

    let mut resamp = RealF32Resamp::new(RealF32ResampParams {
        output_rate_hz: AUDIO_RATE,
        stopband_db: 60.0,
    })
    .unwrap();
    init_at(&mut resamp, IF_RATE);
    pump_real_to_real(&mut resamp, &if_audio)
}

/// msg → FmModulator → FmDemod → RealF32Resamp → recovered.
fn fm_roundtrip(msg: &[f32], deviation_hz: f32, if_rate: f64) -> Vec<f32> {
    let mut modu = FmModulator::new(FmModulatorParams {
        input_rate_hz: AUDIO_RATE as f32,
        output_rate_hz: if_rate as f32,
        offset_hz: 0.0,
        deviation_hz,
    })
    .unwrap();
    let iq = pump_real_to_iq(&mut modu, msg);

    let mut demod = FmDemod::new(FmDemodParams {
        sample_rate_hz: if_rate as f32,
        max_deviation_hz: deviation_hz,
    })
    .unwrap();
    init_at(&mut demod, if_rate);
    let if_audio = pump_iq_to_real(&mut demod, &iq);

    let mut resamp = RealF32Resamp::new(RealF32ResampParams {
        output_rate_hz: AUDIO_RATE,
        stopband_db: 60.0,
    })
    .unwrap();
    init_at(&mut resamp, if_rate);
    pump_real_to_real(&mut resamp, &if_audio)
}

// --------------------------------------------------------------------
// AM
// --------------------------------------------------------------------

#[test]
fn am_synthetic_roundtrip_is_high_fidelity() {
    let msg = synthetic_message(AUDIO_RATE, 2.0, 0.9);
    let rec = am_roundtrip(&msg, 0.7);
    let s = align_and_score(&msg, &rec, 4_096);
    println!(
        "AM synthetic: lag={} gain={:.4} rho={:.4} segSNR={:.1} dB",
        s.lag, s.gain, s.rho, s.seg_snr_db
    );
    assert!(
        s.rho > 0.999,
        "AM synthetic ρ={:.4} below tight gate (segSNR {:.1} dB)",
        s.rho,
        s.seg_snr_db
    );
    // segSNR floor closes the flat-gain blind spot ρ misses (ρ is
    // scale-invariant). Observed ≈5.8 dB; floor well below it.
    assert!(
        s.seg_snr_db > 3.0,
        "AM synthetic segSNR={:.1} dB collapsed (ρ={:.4})",
        s.seg_snr_db,
        s.rho
    );
}

#[test]
fn am_real_sample_roundtrip_survives_the_chain() {
    let (msg, rate) = load_audio("audio/am_0.810mhz_audio-s16.wav");
    assert!((rate - AUDIO_RATE).abs() < 0.5, "expected 48 kHz fixture");
    let rec = am_roundtrip(&msg, 0.7);
    let s = align_and_score(&msg, &rec, 4_096);
    println!(
        "AM real: lag={} gain={:.4} rho={:.4} segSNR={:.1} dB",
        s.lag, s.gain, s.rho, s.seg_snr_db
    );
    assert!(
        s.rho > 0.97,
        "AM real-sample ρ={:.4} below integration gate (segSNR {:.1} dB)",
        s.rho,
        s.seg_snr_db
    );
    // Observed ≈5.2 dB; floor catches a scale/flat-gain regression.
    assert!(
        s.seg_snr_db > 2.0,
        "AM real segSNR={:.1} dB collapsed (ρ={:.4})",
        s.seg_snr_db,
        s.rho
    );
}

// --------------------------------------------------------------------
// FM (NBFM synthetic, WBFM real)
// --------------------------------------------------------------------

#[test]
fn nbfm_synthetic_roundtrip_is_high_fidelity() {
    let msg = synthetic_message(AUDIO_RATE, 2.0, 0.9);
    let rec = fm_roundtrip(&msg, 5_000.0, IF_RATE);
    let s = align_and_score(&msg, &rec, 4_096);
    println!(
        "NBFM synthetic: lag={} gain={:.4} rho={:.4} segSNR={:.1} dB",
        s.lag, s.gain, s.rho, s.seg_snr_db
    );
    assert!(
        s.rho > 0.995,
        "NBFM synthetic ρ={:.4} below tight gate (segSNR {:.1} dB)",
        s.rho,
        s.seg_snr_db
    );
    // Observed ≈27 dB; floor ~6 dB below (round down).
    assert!(
        s.seg_snr_db > 20.0,
        "NBFM synthetic segSNR={:.1} dB collapsed (ρ={:.4})",
        s.seg_snr_db,
        s.rho
    );
}

#[test]
fn wbfm_real_sample_roundtrip_survives_the_chain() {
    let (msg, rate) = load_audio("audio/fm_98.500mhz_audio-s16.wav");
    assert!((rate - AUDIO_RATE).abs() < 0.5, "expected 48 kHz fixture");
    // WBFM ±75 kHz. 480 kHz IF so kf = 75k/480k ≈ 0.156 stays well
    // inside liquid's (0, 0.5) and |dev| < Nyquist (240 kHz).
    let rec = fm_roundtrip(&msg, 75_000.0, 480_000.0);
    let s = align_and_score(&msg, &rec, 8_192);
    println!(
        "WBFM real: lag={} gain={:.4} rho={:.4} segSNR={:.1} dB",
        s.lag, s.gain, s.rho, s.seg_snr_db
    );
    assert!(
        s.rho > 0.90,
        "WBFM real-sample ρ={:.4} below integration gate (segSNR {:.1} dB)",
        s.rho,
        s.seg_snr_db
    );
    // Observed ≈9.2 dB; floor ~6 dB below (round down).
    assert!(
        s.seg_snr_db > 3.0,
        "WBFM real segSNR={:.1} dB collapsed (ρ={:.4})",
        s.seg_snr_db,
        s.rho
    );
}

// --------------------------------------------------------------------
// IQ upmix — the explicit "at least upmix for an IQ sample" path.
// --------------------------------------------------------------------

/// Read the full APRS IQ capture (39 062 Hz stereo s16, L=I R=Q) via
/// the production `FileIqSource`.
fn load_iq_capture() -> (Vec<Complex<f32>>, f64) {
    let mut src = FileIqSource::new(&FileIqSourceParams {
        path: sample_path("vhf/aprs_145.070mhz_iq-s16.wav"),
        rate_hz_hint: 0.0,
        center_freq_hz: 145_070_000.0,
        loop_playback: false,
    })
    .expect("open APRS IQ capture");
    let rate = src.rate_hz();
    let mut all = Vec::new();
    let mut buf = vec![Complex::new(0.0_f32, 0.0); 1 << 15];
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
    (all, rate)
}

#[test]
fn iq_capture_upmix_is_lossless_round_trip() {
    let (iq, rate) = load_iq_capture();
    assert!(iq.len() > 10_000, "APRS capture unexpectedly short");

    // Slide the whole capture +8 kHz up the band…
    let mut up = IqUpmix::new(IqUpmixParams {
        shift_hz: 8_000.0,
        sample_rate_hz: rate as f32,
    })
    .unwrap();
    let shifted = pump_iq_to_iq(&mut up, &iq);

    // …and exactly back down. The composed NCO must reconstruct the
    // original IQ to floating-point precision.
    let mut down = IqUpmix::new(IqUpmixParams {
        shift_hz: -8_000.0,
        sample_rate_hz: rate as f32,
    })
    .unwrap();
    let back = pump_iq_to_iq(&mut down, &shifted);

    assert_eq!(back.len(), iq.len());
    let n = iq.len();
    // Score the recovered magnitude envelope (a real signal the harness
    // can correlate) and bound the worst-case complex error directly.
    let orig_mag: Vec<f32> = iq.iter().map(|c| c.norm()).collect();
    let back_mag: Vec<f32> = back.iter().map(|c| c.norm()).collect();
    let s = align_and_score(&orig_mag, &back_mag, 16);
    println!(
        "IQ upmix round-trip: rho={:.6} segSNR={:.1} dB",
        s.rho, s.seg_snr_db
    );

    let mut max_err = 0.0f32;
    for i in 0..n {
        max_err = max_err.max((iq[i] - back[i]).norm());
    }
    println!("IQ upmix round-trip: max |Δ| = {max_err:.3e}");
    assert!(s.rho > 0.999, "upmix envelope ρ={:.6} not ~1", s.rho);
    assert!(
        max_err < 1e-3,
        "upmix/downmix NCO not lossless: max complex error {max_err:.3e}"
    );
}
