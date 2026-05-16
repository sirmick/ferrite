//! End-to-end POCSAG round-trip against sigidwiki reference samples.
//!
//! Loads the 22 050 Hz mono WAVs under `samples/sigidwiki/22050_mono/`
//! (converted from the upstream MP3s by `convert.py`), feeds the
//! audio through `PagerDemod`, and asserts at least one POCSAG line
//! emerges into the `decoder::pocsag` tracing target.
//!
//! Two tests, one per baud variant:
//! - `pocsag_1200_decodes_at_least_one_message` — `POCSAG_1200.wav`,
//!   the most common rate on US carrier networks.
//! - `pocsag_512_decodes_at_least_one_message` — `POCSAG_512.wav`,
//!   exercises the same plumbing at the slower legacy rate.
//!
//! `PagerDemod` runs all five multimon decoders in parallel
//! (POCSAG512/1200/2400, FLEX, FLEX_NEXT). The variant-specific test
//! still asserts on `decoder::pocsag` only — whichever inner decoder
//! locks the bursts produces the lines, but they all log into the
//! shared category.

#![cfg(feature = "multimon")]
#![allow(clippy::doc_markdown)]

mod common;

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutputPort, PortMeta};
use ferrite_blocks::{Block, PagerDemod, PagerDemodParams};

/// Read a 22 050 Hz mono s16 WAV from `samples/sigidwiki/22050_mono/`,
/// return as f32 samples normalised to [-1, 1].
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

fn decode_pager(audio: &[f32]) -> Vec<String> {
    let mut block = PagerDemod::new(PagerDemodParams::default()).expect("pager demod");
    let chunk = 4_096;
    let mut idx = 0;
    with_pocsag_capture(|| {
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

fn with_pocsag_capture<F: FnOnce()>(f: F) -> Vec<String> {
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
        .filter(|l| l.contains("decoder::pocsag"))
        .map(str::to_string)
        .collect()
}

#[test]
fn pocsag_1200_decodes_at_least_one_message() {
    // Full RX flowgraph: POCSAG is FSK received as NBFM.
    let audio = read_22050_mono_wav("POCSAG_1200.wav");
    let audio = common::full_chain_fm(&audio, 22_050.0);
    let lines = decode_pager(&audio);
    assert!(
        !lines.is_empty(),
        "expected ≥1 decoder::pocsag line from POCSAG_1200.wav, got 0",
    );
}

#[test]
fn pocsag_512_decodes_at_least_one_message() {
    let audio = read_22050_mono_wav("POCSAG_512.wav");
    let audio = common::full_chain_fm(&audio, 22_050.0);
    let lines = decode_pager(&audio);
    assert!(
        !lines.is_empty(),
        "expected ≥1 decoder::pocsag line from POCSAG_512.wav, got 0",
    );
}
