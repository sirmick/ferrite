//! End-to-end AIS round-trip against the sigidwiki reference sample.
//!
//! Loads the 5-second slice of `AIS IQ.wav` shipped under
//! `samples/sigidwiki/AIS_IQ_5s.wav` (see that directory's `SOURCES.md`
//! for provenance), feeds the raw I/Q through the same `FmDemod`
//! Ferrite uses live, then through `AisDemod`, and asserts at least
//! one well-formed AIVDM sentence emerges.
//!
//! The sample is a single-channel I/Q recording at 48 kHz (left = I,
//! right = Q) — its bandwidth covers only one AIS leg, so we feed the
//! demodulated audio into `AisDemod.ch_a` and a zero-filled buffer into
//! `ch_b`. That mirrors what the live `ais.json` preset does for
//! channel A; channel B's chain runs in parallel against an empty
//! signal here, which is fine — the GMSK PLL on a silent leg just
//! never finds a sync.

#![cfg(feature = "ais")]

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{AisDemod, AisDemodParams, Block, FmDemod, FmDemodParams};
use num_complex::Complex;

/// Read a 48 kHz stereo s16 WAV from the sigidwiki sample dir, return
/// `(left, right)` as parallel `Vec<i16>`. Bare-minimum WAV parser —
/// matches what `analyze-packet-wav` does for its 22 kHz mono case.
fn read_ais_iq_wav() -> (Vec<i16>, Vec<i16>) {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "samples",
        "sigidwiki",
        "AIS_IQ_5s.wav",
    ]
    .iter()
    .collect();
    let mut bytes = Vec::new();
    File::open(&path)
        .expect("open AIS_IQ_5s.wav — has the sample been pruned?")
        .read_to_end(&mut bytes)
        .unwrap();
    // Find "data" chunk start.
    let mut pos = 0;
    while pos + 8 < bytes.len() {
        if &bytes[pos..pos + 4] == b"data" {
            pos += 8;
            break;
        }
        pos += 1;
    }
    assert!(pos < bytes.len(), "no data chunk in AIS_IQ_5s.wav");
    let pcm = &bytes[pos..];
    let n = pcm.len() / 4;
    let mut left = Vec::with_capacity(n);
    let mut right = Vec::with_capacity(n);
    for i in 0..n {
        left.push(i16::from_le_bytes([pcm[i * 4], pcm[i * 4 + 1]]));
        right.push(i16::from_le_bytes([pcm[i * 4 + 2], pcm[i * 4 + 3]]));
    }
    (left, right)
}

/// Run an `FmDemod` over an IQ slice, return the real audio output.
/// Same shape as the live AIS pipeline's per-channel chain
/// (`Channelizer → FmDemod → RealF32Resamp`), minus the resampler since
/// the fixture is already at 48 kHz.
fn fm_demod_iq(iq: &[Complex<f32>]) -> Vec<f32> {
    let mut demod = FmDemod::new(FmDemodParams {
        sample_rate_hz: 48_000.0,
        max_deviation_hz: 5_000.0,
    })
    .expect("fm demod");
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

/// Push audio into `AisDemod` in 4800-sample (0.1 s) chunks — matches
/// roughly what the live runtime tick loop hands the block, and small
/// enough that any intra-frame state drift would show up.
fn decode_ais(audio: &[f32]) -> usize {
    let mut block = AisDemod::new(AisDemodParams::default()).expect("ais demod");
    let silence = vec![0.0_f32; 4_800];
    let chunk = 4_800;
    let mut idx = 0;
    let lines = with_ais_capture(|| {
        while idx < audio.len() {
            let take = chunk.min(audio.len() - idx);
            let mut inputs = [
                InputPort {
                    name: "ch_a",
                    meta: PortMeta::default(),
                    buf: InBuf::RealF32(&audio[idx..idx + take]),
                },
                InputPort {
                    name: "ch_b",
                    meta: PortMeta::default(),
                    buf: InBuf::RealF32(&silence[..take]),
                },
            ];
            let mut outputs: [OutputPort; 0] = [];
            let mut io = BlockIo {
                inputs: &mut inputs,
                outputs: &mut outputs,
            };
            block.process(&mut io).unwrap();
            idx += take;
        }
    });
    let aivdm: Vec<&String> = lines.iter().filter(|l| l.contains("!AIVDM,")).collect();
    aivdm.len()
}

/// Capture every `decoder::ais` info line emitted while `f` runs.
/// Mirrors `morse_e2e::with_cw_capture` — different target, same
/// custom-subscriber technique.
fn with_ais_capture<F: FnOnce()>(f: F) -> Vec<String> {
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct VecWriter(Arc<Mutex<Vec<u8>>>);
    impl Write for VecWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> MakeWriter<'a> for VecWriter {
        type Writer = VecWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let writer = VecWriter(Arc::clone(&buf));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_target(true)
        .with_ansi(false)
        .without_time()
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, f);

    let bytes = buf.lock().unwrap();
    let s = String::from_utf8_lossy(&bytes);
    s.lines()
        .filter(|l| l.contains("decoder::ais"))
        .map(str::to_string)
        .collect()
}

#[test]
fn sigidwiki_ais_iq_decodes_at_least_one_aivdm() {
    let (left, right) = read_ais_iq_wav();
    assert!(!left.is_empty(), "WAV had no PCM");
    assert_eq!(left.len(), right.len(), "stereo legs out of sync");

    // Treat WAV stereo as I/Q (per sigidwiki's labelling — not
    // post-FmDemod audio despite the .wav filename).
    let iq: Vec<Complex<f32>> = left
        .iter()
        .zip(right.iter())
        .map(|(&l, &r)| Complex::new(f32::from(l) / 32_768.0, f32::from(r) / 32_768.0))
        .collect();

    // FM-demod recovers the GMSK transitions as audio (the standard
    // FM-discriminator-then-bit-slicer trick aisdecoder is built on).
    let audio = fm_demod_iq(&iq);

    let n = decode_ais(&audio);
    assert!(
        n >= 1,
        "expected ≥1 AIVDM frame from the sigidwiki AIS sample, got {n}"
    );
}
