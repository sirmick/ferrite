//! End-to-end POCSAG + FLEX round-trip against sigidwiki reference
//! samples.
//!
//! Loads the 22 050 Hz mono WAVs under `samples/sigidwiki/22050_mono/`
//! (converted from the upstream MP3s by `convert.py`), feeds the
//! audio through `PagerDemod`, and asserts on what comes out.
//!
//! POCSAG, one test per baud variant, asserting at least one line in
//! the `decoder::pocsag` tracing target:
//! - `pocsag_1200_decodes_at_least_one_message` — `POCSAG_1200.wav`,
//!   the most common rate on US carrier networks.
//! - `pocsag_512_decodes_at_least_one_message` — `POCSAG_512.wav`,
//!   exercises the same plumbing at the slower legacy rate.
//!
//! FLEX, asserting on the `events` port records (what the store, the
//! Journal and `decodes(kind="pager")` see):
//! - `flex_3200_4_long_address_pages_decode_once_and_whole` —
//!   `FLEX.wav`, 3200 bps 4-level with long addresses. Pins the
//!   single-decoder contract: every page once, as `FLEX_NEXT`, with a
//!   sane capcode and the whole message (the classic decoder used to
//!   add a second copy with a 32-bit-wrapped capcode and the first
//!   characters missing).
//! - `flex_1600_2_short_address_pages_decode` — `FLEX_2-LVL_1600_bps.wav`.
//!
//! `PagerDemod` runs four multimon decoders in parallel
//! (POCSAG512/1200/2400, FLEX_NEXT). The POCSAG tests assert on
//! `decoder::pocsag` only — whichever inner decoder locks the bursts
//! produces the lines, but they all log into the shared category.

#![cfg(feature = "multimon")]
#![allow(clippy::doc_markdown)]

mod common;

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
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

/// Pump audio through `PagerDemod` and collect the `events` port as
/// parsed JSON records — the same bytes `ui:pager` carries to the store.
fn pager_events(audio: &[f32]) -> Vec<serde_json::Value> {
    let mut block = PagerDemod::new(PagerDemodParams::default()).expect("pager demod");
    let mut emitted: Vec<u8> = Vec::new();
    let mut scratch = vec![0u8; 65_536];
    let chunk = 4_096;
    let mut idx = 0;
    while idx < audio.len() {
        let take = chunk.min(audio.len() - idx);
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(&audio[idx..idx + take]),
        }];
        let mut outputs = [OutputPort {
            name: "events",
            meta: PortMeta::default(),
            buf: OutBuf::Events(&mut scratch),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = block.process(&mut io).unwrap();
        let n = w.produced[0];
        if n > 0 {
            emitted.extend_from_slice(&scratch[..n]);
        }
        idx += take;
    }
    let text = String::from_utf8(emitted).expect("events bytes must be UTF-8");
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("invalid pager JSON {l:?}: {e}"))
        })
        .collect()
}

/// The window a `uint32_t` short-address subtraction wraps into
/// (`aw1 - 0x8000` for `aw1 < 0x8000`): a capcode here is the classic
/// decoder's arithmetic bug, never a real address.
fn is_u32_wrapped(capcode: u64) -> bool {
    (u64::from(u32::MAX) - 0x8000 + 1..=u64::from(u32::MAX)).contains(&capcode)
}

fn assert_flex_contract(records: &[serde_json::Value], fixture: &str) {
    let flex: Vec<&serde_json::Value> = records.iter().filter(|r| r["proto"] == "FLEX").collect();
    assert!(
        !flex.is_empty(),
        "expected ≥1 FLEX record from {fixture}, got 0"
    );
    for r in &flex {
        // One FLEX decoder → one protocol tag. A `FLEX` (classic) record
        // means both are running again and every page is doubled.
        assert_eq!(
            r["protocol"], "FLEX_NEXT",
            "record from the classic FLEX decoder in {fixture}: {r}"
        );
        if let Some(c) = r["capcode"].as_u64() {
            assert!(
                !is_u32_wrapped(c) && c <= 4_297_068_542,
                "wrapped/out-of-range capcode {c} in {fixture}: {r}"
            );
        }
        let msg = r["message"].as_str().unwrap_or_default();
        assert!(
            !(msg.len() >= 5
                && msg.as_bytes()[1] == b'.'
                && msg.as_bytes()[3] == b'.'
                && msg.as_bytes().get(5) == Some(&b'|')),
            "fragment tag left in message text in {fixture}: {r}"
        );
    }
}

#[test]
fn flex_3200_4_long_address_pages_decode_once_and_whole() {
    let audio = read_22050_mono_wav("FLEX.wav");
    let audio = common::full_chain_fm(&audio, 22_050.0);
    let records = pager_events(&audio);
    assert_flex_contract(&records, "FLEX.wav");
    // Long-address alpha pages come out whole. The classic decoder
    // mis-framed these and printed "om: bigmessagesllc1@…".
    assert!(
        records.iter().any(|r| r["format"] == "alpha"
            && r["message"]
                .as_str()
                .is_some_and(|m| m.starts_with("From: "))),
        "no whole long-address alpha page (\"From: …\") in FLEX.wav: {records:#?}"
    );
    // The frag flag rides as its own field.
    assert!(
        records.iter().any(|r| r["frag"] == "K"),
        "no complete (frag=K) alpha page in FLEX.wav"
    );
}

#[test]
fn flex_1600_2_short_address_pages_decode() {
    let audio = read_22050_mono_wav("FLEX_2-LVL_1600_bps.wav");
    let audio = common::full_chain_fm(&audio, 22_050.0);
    let records = pager_events(&audio);
    assert_flex_contract(&records, "FLEX_2-LVL_1600_bps.wav");
    assert!(
        records
            .iter()
            .any(|r| r["format"] == "alpha" && r["capcode"].as_u64().is_some()),
        "no alpha page with a capcode in FLEX_2-LVL_1600_bps.wav: {records:#?}"
    );
}
