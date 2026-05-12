//! End-to-end FT8 round-trip against a sigidwiki-shape reference WAV.
//!
//! Loads `samples/sigidwiki/FT8_websdr_test.wav` (12 kHz mono s16,
//! kgoba/ft8_lib's upstream test corpus — one full FT8 slot of real
//! off-air HF audio with multiple decodable transmissions), feeds it
//! through `ferrite_ft8::Monitor`, and asserts that
//! `decode_slot()` yields ≥1 message containing a callsign-shaped
//! token.
//!
//! ### Why drive `Monitor` directly instead of the `Ft8Demod` block
//!
//! `Ft8Demod` aligns its slot decode to UTC (15 s boundaries) so a
//! live source streaming wall-clock-asynchronously decodes only on
//! the seconds when an FT8 transmission actually starts. Offline
//! tests don't have a UTC alignment to honour — the test WAV is *one*
//! slot of audio and we want to decode it whole. Going through
//! `Monitor` skips the wall-clock gate while still exercising the C
//! decoder and the safe Rust wrapper end-to-end. The block-layer
//! tests in `blocks/src/ft8.rs` cover spec / construct / params.

#![cfg(feature = "ft8")]

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use ferrite_ft8::{Monitor, MonitorConfig};

fn read_12000_mono_wav(name: &str) -> Vec<f32> {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "samples",
        "sigidwiki",
        name,
    ]
    .iter()
    .collect();
    let mut bytes = Vec::new();
    File::open(&path)
        .unwrap_or_else(|e| panic!("open {name}: {e} (path={})", path.display()))
        .read_to_end(&mut bytes)
        .unwrap();
    // Skip ahead to the data chunk; same 16-bit PCM extractor as
    // pager_e2e / packet_e2e use — keeps the test free of hound dep.
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

#[test]
fn ft8_decodes_websdr_test_wav() {
    let audio = read_12000_mono_wav("FT8_websdr_test.wav");
    // Sanity: ~15 s @ 12 kHz = 180 000 samples ± a few. The fixture
    // is a deliberately full slot of real off-air audio.
    assert!(
        audio.len() >= 150_000,
        "FT8_websdr_test.wav unexpectedly short ({} samples) — re-fetch from kgoba/ft8_lib?",
        audio.len()
    );

    let mut mon = Monitor::new(&MonitorConfig::ft8_default()).expect("Monitor::new");
    let block = mon.block_size();
    assert!(block > 0, "block_size must be positive");

    for chunk in audio.chunks_exact(block) {
        mon.process_block(chunk).expect("process_block");
    }

    let decoded = mon.decode_slot(40);

    // Print everything the test saw, win or lose — failure mode
    // visibility for a future me staring at "0 decoded but the C lib
    // worked yesterday." Captured to test stdout (visible with
    // `cargo test -- --nocapture`).
    eprintln!("FT8 decode_slot returned {} message(s):", decoded.len());
    for d in &decoded {
        eprintln!(
            "  freq={:.0} Hz  SNR={:+.0} dB  ldpc_err={}  text={:?}",
            d.freq_hz, d.snr_db, d.ldpc_errors, d.text
        );
    }

    assert!(
        !decoded.is_empty(),
        "expected ≥1 FT8 decode from FT8_websdr_test.wav, got 0",
    );

    // Spot-check: at least one decoded message should contain a
    // plausible callsign token. Real FT8 is a mix of `CQ <call>
    // <grid>`, `<call> <call> <report>`, etc — every message has at
    // least one alphanumeric token of length 3–11 with at least one
    // digit. Loose match keeps the test resilient to which messages
    // upstream's WAV captured at recording time.
    let callsign_like = decoded.iter().any(|d| {
        d.text.split_whitespace().any(|tok| {
            (3..=11).contains(&tok.len())
                && tok.chars().any(|c| c.is_ascii_digit())
                && tok.chars().all(|c| c.is_ascii_alphanumeric() || c == '/')
        })
    });
    assert!(
        callsign_like,
        "no callsign-shaped token in any decoded message — sanity check failed",
    );
}
