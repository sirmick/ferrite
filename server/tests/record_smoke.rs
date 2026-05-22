//! Record-to-disk smoke test: compose a fm-audio-record-shaped preset,
//! drive it with a SineSource, let it run briefly, and verify the
//! FileAudioSink produced a parseable mono 16-bit WAV of non-trivial
//! size and non-silent content.
//!
//! This is the unit-testable half of the "record an FM station" flow —
//! a SOAPYSDR_REAL_HW-gated companion sits alongside it for when
//! hardware is available.

use std::{path::PathBuf, time::Duration};

use ferrite_runtime::{FlowgraphDoc, SourceConfig};
use serde_json::json;

#[path = "../src/log_stream.rs"]
#[allow(dead_code)]
mod log_stream;

#[path = "../src/frame_bus.rs"]
#[allow(dead_code)]
mod frame_bus;

#[path = "../src/bridge_sink.rs"]
#[allow(dead_code)]
mod bridge_sink;

#[path = "../src/preset_pipeline.rs"]
#[allow(dead_code)]
mod preset_pipeline;

// `app_state` + `routes` reference `crate::view_bridge::…`; this
// #[path] re-include has to mirror every src module they touch or the
// test binary won't resolve them. view_bridge.rs is self-contained
// (std + serde + tokio, no crate:: deps), so the shim is a plain mod.
#[path = "../src/view_bridge.rs"]
#[allow(dead_code)]
mod view_bridge;

#[path = "../src/band_plan.rs"]
#[allow(dead_code)]
mod band_plan;

#[path = "../src/app_state.rs"]
#[allow(dead_code)]
mod app_state;

#[path = "../src/device.rs"]
#[allow(dead_code)]
mod device;

#[path = "../src/device_cache.rs"]
#[allow(dead_code)]
mod device_cache;

#[path = "../src/block_schema.rs"]
#[allow(dead_code)]
mod block_schema;

#[path = "../src/routes.rs"]
#[allow(dead_code, clippy::unused_async)]
mod routes;

fn record_preset(wav_path: &std::path::Path) -> FlowgraphDoc {
    // Mirrors flowgraphs/fm-audio-record.json but lets the caller pin
    // the output path to a tempfile. Keeping the test self-contained
    // (rather than loading the shipped preset and patching) makes it
    // obvious what the fixture exercises.
    serde_json::from_value(json!({
        "name": "fm-audio-record-smoke",
        "environments": ["node"],
        "blocks": {
            "src":  { "type": "Source", "placement": "node",
                      "params": { "center_freq_hz": 100_000_000.0,
                                  "sample_rate_hz": 2_400_000.0 } },
            "chan": { "type": "Channelizer", "placement": "node",
                      "params": { "input_rate_hz": 2_400_000.0,
                                  "freq_shift_hz": 0.0,
                                  "output_rate_hz": 240_000.0 } },
            "demod": { "type": "FmDemod", "placement": "node",
                       "params": { "sample_rate_hz": 240_000,
                                   "max_deviation_hz": 75_000 } },
            "decim": { "type": "RealF32Resamp", "placement": "node",
                       "params": { "output_rate_hz": 48_000.0,
                                   "stopband_db": 60.0 } },
            "audio": { "type": "FileAudioSink", "placement": "node",
                       "params": { "path": wav_path.to_string_lossy(),
                                   "rate_hz": 48_000.0,
                                   "max_seconds": 5.0 } }
        },
        "wires": [
            ["src.out", "chan.in"],
            ["chan.out", "demod.in"],
            ["demod.out", "decim.in"],
            ["decim.out", "audio.in"]
        ]
    }))
    .unwrap()
}

fn sine_source_at(rate_hz: f64, offset_hz: f64) -> SourceConfig {
    SourceConfig {
        type_name: "SineSource".into(),
        params: json!({
            "rate_hz": rate_hz,
            "center_freq_hz": 100_000_000.0,
            "tone_freq_abs_hz": 100_000_000.0 + offset_hz,
            "amplitude": 0.5,
        }),
    }
}

fn tempdir(tag: &str) -> PathBuf {
    let mut base = std::env::temp_dir();
    let pid = std::process::id();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    base.push(format!("ferrite-record-smoke-{tag}-{pid}-{ts}"));
    std::fs::create_dir_all(&base).unwrap();
    base
}

/// Parses just enough of a mono-PCM WAV to validate header fields and
/// return the (sample_rate, sample_count, rms) triple. Independent
/// from the writer so a bug that writes a broken header shows up here.
fn parse_mono_wav(bytes: &[u8]) -> (u32, usize, f32) {
    assert!(bytes.len() >= 44, "wav too short: {}", bytes.len());
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    assert_eq!(&bytes[12..16], b"fmt ");
    let channels = u16::from_le_bytes([bytes[22], bytes[23]]);
    assert_eq!(channels, 1, "expected mono");
    let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
    let bits = u16::from_le_bytes([bytes[34], bytes[35]]);
    assert_eq!(bits, 16, "expected 16-bit PCM");
    assert_eq!(&bytes[36..40], b"data");
    let data_len = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]) as usize;
    assert_eq!(bytes.len(), 44 + data_len, "file length must match header");
    let sample_count = data_len / 2;
    let mut sum_sq: f64 = 0.0;
    for chunk in bytes[44..].chunks_exact(2) {
        let s = i16::from_le_bytes([chunk[0], chunk[1]]);
        let f = f32::from(s) / 32_767.0;
        sum_sq += f64::from(f * f);
    }
    let rms = if sample_count > 0 {
        (sum_sq / sample_count as f64).sqrt() as f32
    } else {
        0.0
    };
    (rate, sample_count, rms)
}

#[tokio::test]
async fn fm_record_preset_produces_non_silent_wav_with_sine_source() {
    let dir = tempdir("fm");
    let wav = dir.join("fm.wav");
    let state = app_state::AppState::new(
        record_preset(&wav),
        // Tone 5 kHz off-center, well inside the 240 kHz channel the
        // Channelizer keeps. After 5× decim we land at 48 kHz; FM
        // discriminator on a constant-phase-rate signal produces a DC
        // level, which clips to a non-zero RMS.
        sine_source_at(2_400_000.0, 5_000.0),
        Duration::from_millis(2),
    );
    state.start().await.expect("start pipeline");
    // 500 ms of wall time is enough for the 2.4 MS/s → 48 kHz chain to
    // push a few thousand audio samples through FileAudioSink even on
    // a slow CI runner. The sink finalises on stop().
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(state.stop().await, "pipeline was running");

    let bytes = std::fs::read(&wav).expect("wav written");
    let (rate, samples, rms) = parse_mono_wav(&bytes);
    assert_eq!(rate, 48_000, "sample rate stamped into header");
    assert!(
        samples >= 1_000,
        "at least a few thousand samples, got {samples}"
    );
    assert!(rms > 1e-3, "rms should be non-silent, got {rms}");

    std::fs::remove_file(&wav).ok();
    std::fs::remove_dir_all(dir).ok();
}
