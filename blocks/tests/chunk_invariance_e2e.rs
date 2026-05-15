//! Chunk-boundary invariance — the guard against per-tick clicks.
//!
//! The scheduler hands each block a *variable* number of samples per
//! `process()` call (driver USB cadence, jitter, reconfigure). A
//! stateful DSP block that doesn't carry its filter/NCO/resampler
//! state across calls produces a discontinuity at every call boundary
//! — an audible pip at the tick rate.
//!
//! This feeds each block the SAME total input two ways: one giant
//! call, and a jittered stream of tiny/large chunks (including
//! single-sample calls). If state is preserved the produced streams
//! are sample-identical; any divergence is a glitch bug.

use core::f32::consts::TAU;

use ferrite_blocks::block::{BlockIo, InBuf, InitCtx, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{
    AmDemod, AmDemodParams, AudioShaper, AudioShaperParams, Block, Channelizer, ChannelizerParams,
    FmDemod, FmDemodParams, RealF32Resamp, RealF32ResampParams,
};
use num_complex::Complex;

/// Jittered chunk pattern — tiny (1, 3), odd, and large sizes so any
/// per-call state reset shows up. Cycled to cover the whole input.
const JITTER: &[usize] = &[97, 1, 256, 33, 512, 8, 3, 128, 1, 1000, 64];

// --- generic drivers (one process() call per fed chunk) -------------

fn drive_iq_real(b: &mut dyn Block, input: &[Complex<f32>], pattern: &[usize]) -> Vec<f32> {
    let mut out_acc = Vec::new();
    let (mut cur, mut pi) = (0usize, 0usize);
    while cur < input.len() {
        let want = pattern[pi % pattern.len()].max(1);
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
        out_acc.extend_from_slice(&ob[..w.produced[0]]);
        if w.consumed[0] == 0 {
            break;
        }
        cur += w.consumed[0];
    }
    out_acc
}

fn drive_iq_iq(b: &mut dyn Block, input: &[Complex<f32>], pattern: &[usize]) -> Vec<Complex<f32>> {
    let mut out_acc = Vec::new();
    let (mut cur, mut pi) = (0usize, 0usize);
    while cur < input.len() {
        let want = pattern[pi % pattern.len()].max(1);
        pi += 1;
        let inb = &input[cur..(cur + want).min(input.len())];
        let mut ob = vec![Complex::new(0.0_f32, 0.0); inb.len() + 256];
        let mut ins = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(inb),
        }];
        let mut outs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut ob),
        }];
        let w = b
            .process(&mut BlockIo {
                inputs: &mut ins,
                outputs: &mut outs,
            })
            .unwrap();
        out_acc.extend_from_slice(&ob[..w.produced[0]]);
        if w.consumed[0] == 0 {
            break;
        }
        cur += w.consumed[0];
    }
    out_acc
}

fn drive_real_real(b: &mut dyn Block, input: &[f32], pattern: &[usize]) -> Vec<f32> {
    let mut out_acc = Vec::new();
    let (mut cur, mut pi) = (0usize, 0usize);
    while cur < input.len() {
        let want = pattern[pi % pattern.len()].max(1);
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
        out_acc.extend_from_slice(&ob[..w.produced[0]]);
        if w.consumed[0] == 0 {
            break;
        }
        cur += w.consumed[0];
    }
    out_acc
}

// --- comparison -----------------------------------------------------

/// Assert the one-call and jittered streams agree. liquid DSP is
/// deterministic, so preserved state ⇒ identical output; a divergence
/// is a per-call state bug (the click). Tail slack absorbs the few
/// samples a resampler may have buffered when the last chunk lands on
/// a different boundary.
fn assert_same(label: &str, a: &[f32], b: &[f32]) {
    let n = a.len().min(b.len());
    assert!(
        n > 256,
        "{label}: produced too little ({} / {})",
        a.len(),
        b.len()
    );
    let cmp = n.saturating_sub(64);
    for i in 0..cmp {
        let d = (a[i] - b[i]).abs();
        assert!(
            d < 1e-3,
            "{label}: chunking changed the output at sample {i} \
             (one-call={} jittered={} Δ={d}) — block doesn't preserve \
             state across process() calls (per-tick click)",
            a[i],
            b[i],
        );
    }
}

fn assert_same_cx(label: &str, a: &[Complex<f32>], b: &[Complex<f32>]) {
    let n = a.len().min(b.len());
    assert!(
        n > 256,
        "{label}: produced too little ({} / {})",
        a.len(),
        b.len()
    );
    let cmp = n.saturating_sub(64);
    for i in 0..cmp {
        let d = ((a[i].re - b[i].re).powi(2) + (a[i].im - b[i].im).powi(2)).sqrt();
        assert!(
            d < 1e-3,
            "{label}: chunking changed the output at sample {i} (Δ={d}) — \
             block doesn't preserve state across process() calls",
        );
    }
}

// --- signal helpers -------------------------------------------------

fn cx_tone(freq: f32, fs: f32, n: usize) -> Vec<Complex<f32>> {
    (0..n)
        .map(|i| {
            let p = TAU * freq * i as f32 / fs;
            Complex::new(p.cos(), p.sin())
        })
        .collect()
}

fn fm_iq(msg_hz: f32, fs: f32, dev: f32, n: usize) -> Vec<Complex<f32>> {
    let dt = 1.0 / fs;
    let mut ph = 0.0_f32;
    (0..n)
        .map(|i| {
            let m = (TAU * msg_hz * i as f32 / fs).sin();
            ph += TAU * dev * m * dt;
            Complex::new(ph.cos(), ph.sin())
        })
        .collect()
}

fn am_iq(msg_hz: f32, fs: f32, n: usize) -> Vec<Complex<f32>> {
    (0..n)
        .map(|i| {
            let env = 1.0 + 0.5 * (TAU * msg_hz * i as f32 / fs).cos();
            Complex::new(env, 0.0)
        })
        .collect()
}

fn real_multitone(fs: f32, n: usize) -> Vec<f32> {
    (0..n)
        .map(|i| {
            let t = i as f32 / fs;
            0.5 * (TAU * 600.0 * t).sin()
                + 0.3 * (TAU * 2400.0 * t).sin()
                + 0.2 * (TAU * 7000.0 * t).sin()
        })
        .collect()
}

// --- tests ----------------------------------------------------------

#[test]
fn channelizer_is_chunk_invariant() {
    let iq = cx_tone(5_000.0, 240_000.0, 48_000);
    let mk = || Channelizer::new(ChannelizerParams::new(240_000.0, 0.0, 24_000.0)).unwrap();
    let mut a = mk();
    let mut b = mk();
    let one = drive_iq_iq(&mut a, &iq, &[iq.len()]);
    let jit = drive_iq_iq(&mut b, &iq, JITTER);
    assert_same_cx("Channelizer", &one, &jit);
}

#[test]
fn fm_demod_is_chunk_invariant() {
    let iq = fm_iq(1_000.0, 48_000.0, 5_000.0, 48_000);
    let mk = || {
        FmDemod::new(FmDemodParams {
            sample_rate_hz: 48_000.0,
            max_deviation_hz: 5_000.0,
        })
        .unwrap()
    };
    let mut a = mk();
    let mut b = mk();
    let one = drive_iq_real(&mut a, &iq, &[iq.len()]);
    let jit = drive_iq_real(&mut b, &iq, JITTER);
    assert_same("FmDemod", &one, &jit);
}

#[test]
fn am_demod_is_chunk_invariant() {
    let iq = am_iq(800.0, 12_000.0, 24_000);
    let mk = || {
        AmDemod::new(AmDemodParams {
            sample_rate_hz: 12_000.0,
            audio_gain: 1.0,
        })
        .unwrap()
    };
    let mut a = mk();
    let mut b = mk();
    let one = drive_iq_real(&mut a, &iq, &[iq.len()]);
    let jit = drive_iq_real(&mut b, &iq, JITTER);
    assert_same("AmDemod", &one, &jit);
}

#[test]
fn audio_shaper_is_chunk_invariant() {
    let sig = real_multitone(48_000.0, 48_000);
    let mk = || {
        AudioShaper::new(AudioShaperParams {
            sample_rate_hz: 48_000.0,
            lpf_hz: 15_000.0,
            dc_block: true,
        })
        .unwrap()
    };
    let mut a = mk();
    let mut b = mk();
    let one = drive_real_real(&mut a, &sig, &[sig.len()]);
    let jit = drive_real_real(&mut b, &sig, JITTER);
    assert_same("AudioShaper", &one, &jit);
}

#[test]
fn real_resamp_is_chunk_invariant() {
    let sig = real_multitone(12_000.0, 24_000);
    let mk = || {
        let mut r = RealF32Resamp::new(RealF32ResampParams {
            output_rate_hz: 48_000.0,
            stopband_db: 60.0,
        })
        .unwrap();
        // Resampler realises its msresamp instance at init from the
        // scheduler-reported input rate.
        let meta = [(
            "in",
            PortMeta {
                sample_rate_hz: 12_000.0,
                center_freq_hz: 0.0,
            },
        )];
        let mut ctx = InitCtx {
            input_meta: &meta[..],
            output_meta: &[],
            frames_hint: 1024,
        };
        r.init(&mut ctx).unwrap();
        r
    };
    let mut a = mk();
    let mut b = mk();
    let one = drive_real_real(&mut a, &sig, &[sig.len()]);
    let jit = drive_real_real(&mut b, &sig, JITTER);
    assert_same("RealF32Resamp", &one, &jit);
}
