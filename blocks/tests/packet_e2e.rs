//! End-to-end APRS / packet round-trip against the sigidwiki AFSK1200
//! reference sample.
//!
//! Loads `samples/sigidwiki/22050_mono/AFSK1200_Sound.wav` (22 050 Hz
//! mono s16, derived from the upstream MP3 by `convert.py`), runs it
//! through the full FM RX chain + `PacketDemod`, and asserts the
//! `events` port emits ≥1 APRS frame whose JSON satisfies the
//! `ui:aprs` store contract. `rows > 0` also proves the decode chain
//! still locks the carrier, so this single test subsumes the old
//! line-count check — kept to one decode because the vendored
//! multimon C state isn't safe to exercise from parallel test
//! threads (same constraint as `wspr_e2e`).
//!
//! `PacketDemod` runs five multimon decoders in parallel: AFSK1200
//! (the APRS workhorse), three AFSK2400 timing variants, and FSK9600.
//! AFSK1200_Sound is an AFSK1200 capture, so AFSK1200 is the one that
//! hits; with `aprs_mode` on it prints TNC2 form the events parser
//! consumes.

#![cfg(feature = "multimon")]
#![allow(clippy::doc_markdown)]

mod common;

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{Block, PacketDemod, PacketDemodParams};

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

/// Ship-gate for the `ui:aprs` advanced view: drive the real
/// `events` port (PacketDemod's split_tnc2 → parse_aprs →
/// AprsSpot::write_json path) over the AFSK1200 fixture and assert
/// the emitted newline-JSON satisfies the contract the web `aprs`
/// store parses (see `web/src/lib/aprs/store.svelte.ts`).
#[test]
fn aprs_events_emit_store_contract_json() {
    let audio = common::full_chain_fm(&read_22050_mono_wav("AFSK1200_Sound.wav"), 22_050.0);
    let mut block = PacketDemod::new(PacketDemodParams::default()).expect("packet demod");

    let mut emitted: Vec<u8> = Vec::new();
    let mut scratch = vec![0u8; 16_384];
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
    let mut rows = 0;
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("invalid APRS JSON {line:?}: {e}"));
        // Fields the store reads unconditionally.
        assert!(v["call"].is_string(), "call missing in {line:?}");
        assert!(v["kind"].is_string(), "kind missing in {line:?}");
        assert!(v["raw"].is_string(), "raw missing in {line:?}");
        // lat/lon must appear together when present.
        assert_eq!(
            v.get("lat").is_some(),
            v.get("lon").is_some(),
            "lat/lon must co-occur in {line:?}"
        );
        rows += 1;
    }
    assert!(
        rows > 0,
        "AFSK1200_Sound.wav should yield ≥1 APRS frame on the events port \
         (aprs_mode TNC2 form); got none"
    );
    eprintln!("APRS contract OK: {rows} frames");
}
