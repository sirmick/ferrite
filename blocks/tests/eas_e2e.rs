//! End-to-end EAS / SAME round-trip against the sigidwiki tornado
//! warning sample.
//!
//! Loads `samples/sigidwiki/22050_mono/EAS_Alert_Tornado_Warning.wav`
//! (22 050 Hz mono s16, derived from the upstream MP3 by
//! `convert.py`), feeds the audio through `EasDemod`, and asserts at
//! least one SAME header line emerges into the `decoder::eas`
//! tracing target.
//!
//! The sample is a NOAA Weather Radio recording; the tornado warning
//! header repeats three times at the head of the audio (per the SAME
//! protocol), then the announcement audio plays. multimon's
//! `demod_eas` only emits once the burst has passed, so even a
//! single-decode-line outcome is the bar.

#![cfg(feature = "multimon")]

mod common;

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutputPort, PortMeta};
use ferrite_blocks::{Block, EasDemod, EasDemodParams};

fn read_22050_mono_wav(name: &str) -> Vec<f32> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "samples",
        "sigidwiki",
        "22050_mono",
        name,
    ]
    .iter()
    .collect();
    let mut bytes = Vec::new();
    File::open(&path)
        .unwrap_or_else(|_| panic!("open {name} — pruned? run samples/sigidwiki/convert.py"))
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
    assert!(pos < bytes.len(), "no data chunk in {name}");
    let pcm = &bytes[pos..];
    let n = pcm.len() / 2;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let s = i16::from_le_bytes([pcm[i * 2], pcm[i * 2 + 1]]);
        out.push(f32::from(s) / f32::from(i16::MAX));
    }
    out
}

fn decode_eas(audio: &[f32]) -> Vec<String> {
    let mut block = EasDemod::new(EasDemodParams::default()).expect("eas demod");
    let chunk = 4_096;
    let mut idx = 0;
    with_eas_capture(|| {
        while idx < audio.len() {
            let take = chunk.min(audio.len() - idx);
            let mut inputs = [InputPort {
                name: "in",
                meta: PortMeta::default(),
                buf: InBuf::RealF32(&audio[idx..idx + take]),
            }];
            let mut outputs: [OutputPort; 0] = [];
            let mut io = BlockIo {
                inputs: &mut inputs,
                outputs: &mut outputs,
            };
            block.process(&mut io).unwrap();
            idx += take;
        }
    })
}

fn with_eas_capture<F: FnOnce()>(f: F) -> Vec<String> {
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
        .filter(|l| l.contains("decoder::eas"))
        .map(str::to_string)
        .collect()
}

#[test]
fn tornado_warning_decodes_at_least_one_same_header() {
    // Full RX flowgraph: SAME/EAS is AFSK over (broadcast) FM.
    let audio = read_22050_mono_wav("EAS_Alert_Tornado_Warning.wav");
    let audio = common::full_chain_fm(&audio, 22_050.0);
    let lines = decode_eas(&audio);
    assert!(
        !lines.is_empty(),
        "expected ≥1 decoder::eas line from EAS_Alert_Tornado_Warning.wav, got 0",
    );
}
