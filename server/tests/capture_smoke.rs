//! Server-side capture-orchestration smoke test.
//!
//! Drives the preset-swap wideband-IQ path end to end through the same
//! `AppState` entry point `POST /api/capture/iq {wideband:true}` calls:
//! load the `capture_fm` preset, reuse the live source, run a
//! `Source → FileIqSink` slice, and poll the job to `done`. A SineSource
//! stands in for hardware (via `force:true`), so it needs no SDR and is
//! deterministic.
//!
//! It nets three things Phase 6.3 introduced:
//!   * the orchestration runs *inside ferrited* (no ferrite-ctl loop);
//!   * the finished job carries the block's sidecar JSON, read with the
//!     block's own `<path>.json` naming (the old ferrite-ctl reader
//!     guessed `<path>.cf32.json` and always saw `null`);
//!   * the capture **reuses the live source** rather than building a
//!     fresh one — asserted by the post-capture source still being the
//!     SineSource (tone param intact), only its centre retuned.

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

#[path = "../src/decoder_store.rs"]
#[allow(dead_code)]
mod decoder_store;

#[path = "../src/preset_pipeline.rs"]
#[allow(dead_code)]
mod preset_pipeline;

#[path = "../src/view_bridge.rs"]
#[allow(dead_code)]
mod view_bridge;

#[path = "../src/band_plan.rs"]
#[allow(dead_code)]
mod band_plan;

#[path = "../src/source_policy.rs"]
#[allow(dead_code)]
mod source_policy;

#[path = "../src/capture.rs"]
#[allow(dead_code)]
mod capture;

#[path = "../src/error.rs"]
#[allow(dead_code)]
mod error;

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

/// A trivial starting preset; the capture swaps in `capture_fm`.
fn idle_preset() -> FlowgraphDoc {
    serde_json::from_value(json!({
        "name": "idle",
        "environments": ["node"],
        "blocks": {
            "src": { "type": "Source", "placement": "node",
                     "params": { "center_freq_hz": 100_000_000.0,
                                 "sample_rate_hz": 2_400_000.0 } }
        },
        "wires": []
    }))
    .unwrap()
}

/// SineSource stand-in with a distinctive tone so we can prove the
/// capture reused it (rather than fabricating a fresh source).
fn sine_source() -> SourceConfig {
    SourceConfig {
        type_name: "SineSource".into(),
        params: json!({
            "rate_hz": 2_400_000.0,
            "center_freq_hz": 100_000_000.0,
            "tone_freq_abs_hz": 100_050_000.0,
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
    base.push(format!("ferrite-capture-smoke-{tag}-{pid}-{ts}"));
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn flowgraphs_dir() -> PathBuf {
    // server/ is the crate; the shipped presets live at the workspace root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../flowgraphs")
}

// Multi-threaded: the capture runs as a background task while the test
// polls, and the pipeline tick loop is itself a spawned task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wideband_iq_capture_completes_reuses_source_and_reads_sidecar() {
    let dir = tempdir("wbiq");
    let iq = dir.join("cap.cf32");

    let state = app_state::AppState::new(idle_preset(), sine_source(), Duration::from_millis(2))
        .with_presets_dir(flowgraphs_dir());

    // Same entry point POST /api/capture/iq {wideband:true} calls. `force`
    // bypasses the software-source guard so the SineSource stands in for RF.
    let job = state
        .start_capture_iq(capture::CaptureIqReq {
            duration_s: 0.6,
            freq_hz: Some(100_000_000.0),
            out: Some(iq.to_string_lossy().into_owned()),
            wideband: true,
            force: true,
            ..Default::default()
        })
        .await
        .expect("capture accepted");
    let job0 = serde_json::to_value(&job).unwrap();
    assert_eq!(job0["status"], "running", "returns immediately as running");
    assert_eq!(job0["kind"], "iq");
    let job_id = job0["job_id"].as_str().expect("job_id").to_string();

    // Poll to done — no fixed sleeps. Generous deadline (load + 0.6 s
    // record + 0.5 s tail + slack).
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let final_job = loop {
        let j = state.capture_job(&job_id).await.expect("job still tracked");
        let v = serde_json::to_value(&j).unwrap();
        match v["status"].as_str() {
            Some("done") => break v,
            Some("failed") => panic!("capture failed: {:?}", v["error"]),
            _ => {}
        }
        assert!(
            std::time::Instant::now() < deadline,
            "capture did not finish within deadline: {v}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };

    // The recording landed with real IQ.
    assert_eq!(final_job["output_path"], iq.to_string_lossy().as_ref());
    let bytes = std::fs::read(&iq).expect("cf32 written");
    assert!(
        bytes.len() > 8 * 1024,
        "expected a non-trivial cf32 recording, got {} bytes",
        bytes.len()
    );

    // The FileIqSink sidecar (`cap.json`, not `cap.cf32.json`) is surfaced
    // inline — the sidecar-naming regression net.
    assert!(
        dir.join("cap.json").exists(),
        "block writes <stem>.json next to the recording"
    );
    assert!(
        final_job["sidecar"].is_object(),
        "job must carry the block sidecar, got {:?}",
        final_job["sidecar"]
    );

    // The capture REUSED the live source: still a SineSource with its tone
    // param intact, only the centre retuned. A from-scratch rebuild would
    // have dropped the tone / changed the type.
    let src = state.get_source().await;
    assert_eq!(src.type_name, "SineSource", "live source type preserved");
    assert_eq!(
        src.params.get("tone_freq_abs_hz").and_then(|v| v.as_f64()),
        Some(100_050_000.0),
        "live tone param inherited by the capture source"
    );
    assert_eq!(
        src.params.get("center_freq_hz").and_then(|v| v.as_f64()),
        Some(100_000_000.0),
        "capture retuned centre to the requested freq"
    );

    let _ = state.stop().await;
    std::fs::remove_dir_all(dir).ok();
}
