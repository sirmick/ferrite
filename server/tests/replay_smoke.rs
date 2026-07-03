//! Replay smoke test: drive the full RX chain from a recorded IQ fixture
//! (no SDR) and assert a decode lands in the shared `DecoderStore`.
//!
//! Clones the `record_smoke.rs` `AppState` harness, but instead of a
//! `SineSource` it patches in a `ModulatedFileSource` over the shipped
//! ADS-B 1090 MHz IQ reference and loads the `adsb` preset. The node
//! half env-split injects an `EventStore` for the `ui:adsb` sink, which
//! `attach_bridge_sinks` wires to the store — so a decoded aircraft
//! shows up under `snapshot_kind("adsb")`. Deterministic: the same
//! dump1090 reference fixture the `AdsbDemod` unit path uses, looped for
//! the length of the test.

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

/// Repo path from the server crate dir (`CARGO_MANIFEST_DIR` = `server/`).
fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(rel)
}

fn adsb_preset() -> FlowgraphDoc {
    let json = std::fs::read_to_string(repo_path("flowgraphs/adsb.json"))
        .expect("read flowgraphs/adsb.json");
    serde_json::from_str(&json).expect("parse adsb preset")
}

/// `ModulatedFileSource` over the ADS-B IQ reference — the same source
/// config the `replay_capture` MCP verb builds, upmixing the capture
/// verbatim (`kind = iq`). The `.iq` is headerless raw f32, so the rate
/// hint is mandatory.
fn adsb_replay_source() -> SourceConfig {
    let path = repo_path("samples/uhf/adsb_1090mhz_iq-f32-2ms.iq");
    SourceConfig {
        type_name: "ModulatedFileSource".into(),
        params: json!({
            "path": path.to_string_lossy(),
            "kind": "iq",
            "rate_hz_hint": 2_000_000.0,
            "center_freq_hz": 1_090_000_000.0,
            "loop_playback": true,
        }),
    }
}

#[tokio::test]
async fn adsb_replay_lands_a_decode_in_the_store() {
    // The reference fixture must exist (bootstrap provisions samples).
    let iq = repo_path("samples/uhf/adsb_1090mhz_iq-f32-2ms.iq");
    assert!(iq.exists(), "ADS-B IQ fixture missing at {}", iq.display());

    let state = app_state::AppState::new(
        adsb_preset(),
        adsb_replay_source(),
        Duration::from_millis(2),
    );
    state.start().await.expect("start replay pipeline");
    let store = state.decoder_store();

    // Poll until an ADS-B record lands or the deadline elapses. The clip
    // loops, so decodes accrue continuously; live it lands sub-second,
    // but CI runners are slow — give it a generous window and poll
    // rather than sleeping a fixed duration.
    let deadline = Duration::from_secs(20);
    let start = std::time::Instant::now();
    let mut found = false;
    while start.elapsed() < deadline {
        if let Some(kind) = store.snapshot_kind("adsb") {
            if !kind.recent.is_empty() || !kind.current.is_empty() {
                found = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(state.stop().await, "pipeline was running");
    assert!(
        found,
        "no ADS-B record reached the decoder store within {deadline:?} — \
         the replay → demod → EventStore → store path is broken",
    );
}
