//! End-to-end AM regression test.
//!
//! Two guards:
//!
//! 1. **Offset immunity (synthetic).** A clean 1 kHz-tone DSB-AM signal
//!    is demodulated with the carrier placed at a sweep of offsets from
//!    DC (0 → 5 kHz). Envelope detection must recover the tone at full
//!    SINAD *regardless* of carrier offset — that immunity is the whole
//!    reason large-carrier broadcast AM uses it. This is the gate that
//!    would have caught the coherent-`ampmodem` regression: that demod's
//!    PLL pulled in only ~±500 Hz, so beyond it the output was the
//!    carrier *beat note*, not the audio (SINAD went negative).
//!
//! 2. **Real off-air loudness/finiteness.** `AM_IQ_5s_iq-s16.wav` (5 s
//!    slice of sigidwiki's `AM_IQ.zip`, 64 kHz stereo s16, L=I R=Q;
//!    transcoded from the original offset-binary u8, signal identical)
//!    must demodulate to finite audio whose post-settle RMS clears a
//!    floor tied to the known-good reference recording (~-27 dBFS at
//!    unity gain). The coherent version sat ~7 dB under this even
//!    perfectly tuned; the floor catches that.

use std::f32::consts::TAU;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{AmDemod, AmDemodParams, Block};
use num_complex::Complex;

const SAMPLE_RATE_HZ: u32 = 64_000;
const DURATION_SAMPLES: usize = 5 * SAMPLE_RATE_HZ as usize;

fn demod(iq: &[Complex<f32>], fs: f32) -> Vec<f32> {
    let mut d = AmDemod::new(AmDemodParams {
        sample_rate_hz: fs,
        audio_gain: 1.0,
    })
    .expect("am demod");
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
    d.process(&mut io).unwrap();
    out
}

fn rms(xs: &[f32]) -> f32 {
    let s: f64 = xs.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    #[allow(clippy::cast_possible_truncation)]
    {
        (s / xs.len() as f64).sqrt() as f32
    }
}

fn bin_mag(xs: &[f32], freq: f32, fs: f32) -> f32 {
    let w = f64::from(TAU) * f64::from(freq) / f64::from(fs);
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (n, &x) in xs.iter().enumerate() {
        let p = w * n as f64;
        re += f64::from(x) * p.cos();
        im -= f64::from(x) * p.sin();
    }
    #[allow(clippy::cast_possible_truncation)]
    {
        ((re * re + im * im).sqrt() / xs.len() as f64) as f32
    }
}

fn read_am_iq_wav() -> Vec<Complex<f32>> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "samples",
        "sigidwiki",
        "AM_IQ_5s_iq-s16.wav",
    ]
    .iter()
    .collect();
    let mut bytes = Vec::new();
    File::open(&path)
        .expect("open AM_IQ_5s_iq-s16.wav — pruned?")
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
    assert!(pos < bytes.len(), "no data chunk in AM_IQ_5s_iq-s16.wav");
    // s16 LE stereo, L=I R=Q → 4 bytes per complex sample. (The fixture
    // was transcoded from the original offset-binary u8; FileIqSource
    // requires s16 stereo. Signal is sample-for-sample identical.)
    let pcm = &bytes[pos..];
    let n = pcm.len() / 4;
    (0..n)
        .map(|i| {
            let i_le = i16::from_le_bytes([pcm[i * 4], pcm[i * 4 + 1]]);
            let q_le = i16::from_le_bytes([pcm[i * 4 + 2], pcm[i * 4 + 3]]);
            Complex::new(f32::from(i_le) / 32768.0, f32::from(q_le) / 32768.0)
        })
        .collect()
}

#[test]
fn envelope_demod_is_carrier_offset_immune() {
    // wbam ships the demod at 12 kHz; probe at that rate.
    let fs = 12_000.0_f32;
    let f_m = 800.0_f32;
    let m = 0.7_f32;
    let n = 24_000;

    // Offsets deliberately chosen to never coincide with f_m (800) or
    // its 2nd harmonic (1600), so the off-tone leak probe stays valid.
    for off in [0.0_f32, 50.0, 250.0, 600.0, 1_500.0, 3_000.0, 5_000.0] {
        let iq: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                let env = 1.0 + m * (TAU * f_m * t).cos();
                let ph = TAU * off * t;
                Complex::new(env * ph.cos(), env * ph.sin())
            })
            .collect();
        let audio = demod(&iq, fs);
        let body = &audio[n / 4..]; // skip DC-tracker settle

        for &x in body {
            assert!(x.is_finite(), "non-finite at offset {off} Hz");
        }
        let tone = bin_mag(body, f_m, fs);
        // Worst-case off-tone leakage proxies: the carrier-offset beat
        // (where the broken coherent demod dumped its energy) and the
        // 2nd harmonic.
        let leak = bin_mag(body, off.max(1.0), fs).max(bin_mag(body, 2.0 * f_m, fs));
        let sinad_db = 20.0 * (tone / (leak + 1e-9)).log10();
        assert!(
            sinad_db > 30.0,
            "carrier offset {off} Hz: tone must dominate (got SINAD {sinad_db:.1} dB, \
             tone={tone:.4} leak={leak:.4}) — a narrow-pull-in coherent demod fails here",
        );
    }
}

#[test]
fn sigidwiki_am_iq_demods_to_reference_loudness() {
    let iq = read_am_iq_wav();
    assert_eq!(iq.len(), DURATION_SAMPLES, "expected 5 s @ 64 kHz");

    let audio = demod(&iq, SAMPLE_RATE_HZ as f32);

    // Skip the DC-tracker settle.
    let warmup = SAMPLE_RATE_HZ as usize / 2;
    let body = &audio[warmup..];
    for &x in body {
        assert!(x.is_finite(), "non-finite sample in AmDemod audio output");
    }

    // The known-good reference recording (samples/audio/am_0.810mhz…
    // .json) lands ~-23.6 dBFS *with* its 20× post-demod gain + DC
    // blocker; this fixture decodes to ~-27 dBFS at unity gain. Floor
    // at -32 dBFS (rms 0.025): the working envelope demod clears it
    // comfortably (~0.042); the regressed coherent demod sat at ~0.018
    // (-34.9 dBFS) and would fail here.
    let r = rms(body);
    assert!(
        r > 0.025,
        "AM audio RMS {r:.5} ({:.1} dBFS) below reference floor — demod too quiet",
        20.0 * r.max(1e-12).log10(),
    );
}
