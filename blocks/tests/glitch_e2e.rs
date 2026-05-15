//! End-to-end glitch (pip/blip) detector.
//!
//! Drives a clean modulated tone through the real audio demod chain
//! (demod → AudioShaper → RealF32Resamp), feeding every stage in a
//! jittered stream of varying chunk sizes (the scheduler never hands a
//! block a fixed block size). The recovered audio is a smooth
//! band-limited tone; a click/pip is an isolated discontinuity, which
//! shows up as a first-difference spike far above the tone's natural
//! per-sample slew. Assert there are none past the filter warmup.
//!
//! This is the regression net for the whole class of "audio blips":
//! cold-start transients that ring, inter-stage boundary
//! discontinuities, resampler tail seams. (Driving the *actual*
//! scheduler + WsBridge — test "F" — is the follow-up; this hand-chains
//! the same blocks, which already covers the DSP-side glitch sources.)

use core::f32::consts::TAU;

use ferrite_blocks::block::{BlockIo, InBuf, InitCtx, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{
    AmDemod, AmDemodParams, AudioShaper, AudioShaperParams, Block, FmDemod, FmDemodParams,
    RealF32Resamp, RealF32ResampParams,
};
use num_complex::Complex;

const JITTER: &[usize] = &[97, 1, 256, 33, 512, 8, 3, 128, 1, 1000, 64];

fn iq_to_real(b: &mut dyn Block, input: &[Complex<f32>]) -> Vec<f32> {
    let mut out = Vec::new();
    let (mut cur, mut pi) = (0usize, 0usize);
    while cur < input.len() {
        let want = JITTER[pi % JITTER.len()].max(1);
        pi += 1;
        let inb = &input[cur..(cur + want).min(input.len())];
        let mut ob = vec![0.0_f32; inb.len() * 5 + 256];
        let mut ins = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(inb),
        }];
        let mut outs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut ob),
        }];
        let w = b
            .process(&mut BlockIo {
                inputs: &mut ins,
                outputs: &mut outs,
            })
            .unwrap();
        out.extend_from_slice(&ob[..w.produced[0]]);
        if w.consumed[0] == 0 {
            break;
        }
        cur += w.consumed[0];
    }
    out
}

fn real_to_real(b: &mut dyn Block, input: &[f32]) -> Vec<f32> {
    let mut out = Vec::new();
    let (mut cur, mut pi) = (0usize, 0usize);
    while cur < input.len() {
        let want = JITTER[pi % JITTER.len()].max(1);
        pi += 1;
        let inb = &input[cur..(cur + want).min(input.len())];
        let mut ob = vec![0.0_f32; inb.len() * 5 + 256];
        let mut ins = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(inb),
        }];
        let mut outs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut ob),
        }];
        let w = b
            .process(&mut BlockIo {
                inputs: &mut ins,
                outputs: &mut outs,
            })
            .unwrap();
        out.extend_from_slice(&ob[..w.produced[0]]);
        if w.consumed[0] == 0 {
            break;
        }
        cur += w.consumed[0];
    }
    out
}

fn rms(xs: &[f32]) -> f32 {
    (xs.iter().map(|x| x * x).sum::<f32>() / xs.len() as f32).sqrt()
}

/// Flag isolated discontinuities. A clean recovered tone of amplitude
/// `A` at `f` Hz has a bounded per-sample slew `2π·f/fs·A`; a click is
/// a step of order `A`, i.e. many × the slew. Returns (count, worst Δ,
/// worst index) for samples whose first difference exceeds 8× the
/// expected slew, past `warmup`.
fn glitches(y: &[f32], fs: f32, tone_hz: f32, warmup: usize) -> (usize, f32, usize) {
    let body = &y[warmup.min(y.len())..];
    let amp = rms(body) * std::f32::consts::SQRT_2;
    let slew = TAU * tone_hz / fs * amp;
    let thr = (8.0 * slew).max(1e-4);
    let mut count = 0;
    let mut worst = 0.0_f32;
    let mut worst_i = 0;
    for i in 1..body.len() {
        let d = (body[i] - body[i - 1]).abs();
        if d > thr {
            count += 1;
            if d > worst {
                worst = d;
                worst_i = warmup + i;
            }
        }
    }
    (count, worst, worst_i)
}

#[test]
fn fm_audio_chain_has_no_glitches() {
    // NBFM-ish: demod 24 kHz, ±5 kHz dev, 1 kHz message → AudioShaper
    // (9 kHz LPF + DC block) → resample 24 k → 48 k. ~1 s of audio.
    let fs_in = 24_000.0_f32;
    let n = 24_000;
    let dt = 1.0 / fs_in;
    let mut ph = 0.0_f32;
    let iq: Vec<Complex<f32>> = (0..n)
        .map(|i| {
            let m = (TAU * 1_000.0 * i as f32 / fs_in).sin();
            ph += TAU * 5_000.0 * m * dt;
            Complex::new(ph.cos(), ph.sin())
        })
        .collect();

    let mut demod = FmDemod::new(FmDemodParams {
        sample_rate_hz: fs_in,
        max_deviation_hz: 5_000.0,
    })
    .unwrap();
    let mut shaper = AudioShaper::new(AudioShaperParams {
        sample_rate_hz: fs_in,
        lpf_hz: 9_000.0,
        dc_block: true,
    })
    .unwrap();
    let mut resamp = RealF32Resamp::new(RealF32ResampParams {
        output_rate_hz: 48_000.0,
        stopband_db: 60.0,
    })
    .unwrap();
    let meta = [(
        "in",
        PortMeta {
            sample_rate_hz: f64::from(fs_in),
            center_freq_hz: 0.0,
        },
    )];
    resamp
        .init(&mut InitCtx {
            input_meta: &meta[..],
            output_meta: &[],
            frames_hint: 1024,
        })
        .unwrap();

    let d = iq_to_real(&mut demod, &iq);
    let s = real_to_real(&mut shaper, &d);
    let audio = real_to_real(&mut resamp, &s);

    assert!(audio.iter().all(|x| x.is_finite()), "non-finite audio");
    assert!(
        rms(&audio) > 1e-3,
        "no audio recovered (rms {})",
        rms(&audio)
    );

    // Skip the FIR/resampler group-delay warmup at the 48 kHz rate.
    let (count, worst, at) = glitches(&audio, 48_000.0, 1_000.0, 8_192);
    assert_eq!(
        count, 0,
        "FM chain produced {count} discontinuities (worst Δ={worst} at sample {at}) \
         — a pip/blip in the audio path",
    );
}

#[test]
fn am_audio_chain_has_no_glitches() {
    // AM broadcast-ish: envelope demod 12 kHz, 800 Hz tone → AudioShaper
    // (5 kHz LPF; AmDemod already DC-blocks) → resample 12 k → 48 k.
    let fs_in = 12_000.0_f32;
    let n = 24_000;
    let iq: Vec<Complex<f32>> = (0..n)
        .map(|i| {
            let env = 1.0 + 0.6 * (TAU * 800.0 * i as f32 / fs_in).cos();
            Complex::new(env, 0.0)
        })
        .collect();

    let mut demod = AmDemod::new(AmDemodParams {
        sample_rate_hz: fs_in,
        audio_gain: 1.0,
    })
    .unwrap();
    let mut shaper = AudioShaper::new(AudioShaperParams {
        sample_rate_hz: fs_in,
        lpf_hz: 5_000.0,
        dc_block: false,
    })
    .unwrap();
    let mut resamp = RealF32Resamp::new(RealF32ResampParams {
        output_rate_hz: 48_000.0,
        stopband_db: 60.0,
    })
    .unwrap();
    let meta = [(
        "in",
        PortMeta {
            sample_rate_hz: f64::from(fs_in),
            center_freq_hz: 0.0,
        },
    )];
    resamp
        .init(&mut InitCtx {
            input_meta: &meta[..],
            output_meta: &[],
            frames_hint: 1024,
        })
        .unwrap();

    let d = iq_to_real(&mut demod, &iq);
    let s = real_to_real(&mut shaper, &d);
    let audio = real_to_real(&mut resamp, &s);

    assert!(audio.iter().all(|x| x.is_finite()), "non-finite audio");
    assert!(rms(&audio) > 1e-3, "no audio recovered");

    let (count, worst, at) = glitches(&audio, 48_000.0, 800.0, 8_192);
    assert_eq!(
        count, 0,
        "AM chain produced {count} discontinuities (worst Δ={worst} at sample {at})",
    );
}
