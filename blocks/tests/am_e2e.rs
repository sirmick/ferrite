//! End-to-end AM round-trip against the sigidwiki AM_IQ recording.
//!
//! Loads `samples/sigidwiki/AM_IQ_5s.wav` — a 5-second slice of the
//! upstream `AM_IQ.zip` (sigidwiki/Amplitude_Modulation_(AM)). Format
//! is 64 kHz stereo u8 (left = I, right = Q), offset binary
//! (128 = zero). Runs the IQ through `AmDemod` (liquid-dsp `ampmodem`
//! coherent DSB-with-carrier mode) and asserts the output audio is
//! finite and has substantive RMS once the PLL has locked.
//!
//! What this test does NOT assert: a specific tone or recovered
//! audio content. The recording is real off-air AM (the wiki's
//! sample) and we don't know what's in it; the bar is "the demod
//! produced *something* reasonable", which is enough to catch
//! regressions like all-zero output, NaN-poisoned chains, or a PLL
//! that never locks.

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{AmDemod, AmDemodParams, Block};
use num_complex::Complex;

const SAMPLE_RATE_HZ: u32 = 64_000;
const DURATION_SAMPLES: usize = 5 * SAMPLE_RATE_HZ as usize;

/// Read `AM_IQ_5s.wav`. Stereo u8 PCM, left/right interleaved, offset
/// binary so 128 maps to zero. Returns I + jQ as `Complex<f32>` in
/// the [-1, 1] range.
fn read_am_iq_wav() -> Vec<Complex<f32>> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "samples",
        "sigidwiki",
        "AM_IQ_5s.wav",
    ]
    .iter()
    .collect();
    let mut bytes = Vec::new();
    File::open(&path)
        .expect("open AM_IQ_5s.wav — pruned?")
        .read_to_end(&mut bytes)
        .unwrap();
    let mut pos = 0;
    while pos + 8 < bytes.len() {
        if &bytes[pos..pos + 4] == b"data" {
            pos += 8;
            break;
        }
        pos += 1;
    }
    assert!(pos < bytes.len(), "no data chunk in AM_IQ_5s.wav");
    let pcm = &bytes[pos..];
    let n = pcm.len() / 2;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let i_byte = pcm[i * 2];
        let q_byte = pcm[i * 2 + 1];
        out.push(Complex::new(
            (f32::from(i_byte) - 128.0) / 128.0,
            (f32::from(q_byte) - 128.0) / 128.0,
        ));
    }
    out
}

fn run_am_demod(iq: &[Complex<f32>]) -> Vec<f32> {
    #[allow(clippy::cast_precision_loss)]
    let params = AmDemodParams {
        sample_rate_hz: SAMPLE_RATE_HZ as f32,
        ..AmDemodParams::default()
    };
    let mut demod = AmDemod::new(params).expect("am demod");
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

fn rms(xs: &[f32]) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    let n = xs.len() as f64;
    let s: f64 = xs.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    #[allow(clippy::cast_possible_truncation)]
    let r = (s / n).sqrt() as f32;
    r
}

#[test]
fn sigidwiki_am_iq_demods_to_finite_audio_with_rms() {
    let iq = read_am_iq_wav();
    assert_eq!(iq.len(), DURATION_SAMPLES, "expected 5 s @ 64 kHz");

    let audio = run_am_demod(&iq);

    // Drop the first 0.5 s — the coherent PLL inside `ampmodem` needs
    // a beat to find the residual carrier. Without warmup the head
    // can squeal while lock is acquired, which would inflate RMS for
    // the wrong reason.
    let warmup = SAMPLE_RATE_HZ as usize / 2;
    let body = &audio[warmup..];

    for &x in body {
        assert!(x.is_finite(), "non-finite sample in AmDemod audio output");
    }

    let r = rms(body);
    // Permissive threshold: we want to catch all-zero output / NaN
    // poisoning / total PLL failure, not tune for a specific
    // recording amplitude. The sigidwiki sample is loud enough that
    // a working coherent demod produces well above 1e-3 RMS.
    assert!(
        r > 1e-3,
        "expected non-trivial audio RMS post-warmup, got {r}",
    );
}
