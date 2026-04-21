//! Smoke tests — bring up the server on an ephemeral port and exercise
//! the public surface end-to-end. Runs in CI in lieu of one-off curl.

use std::{net::SocketAddr, time::Duration};

use axum::{
    routing::{get, post},
    Router,
};
use ferrite_blocks::frame::Frame;
use ferrite_runtime::{FlowgraphDoc, SourceConfig};
use futures_util::StreamExt;
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

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

#[path = "../src/app_state.rs"]
#[allow(dead_code)]
mod app_state;

#[cfg(feature = "soapysdr")]
#[path = "../src/device.rs"]
#[allow(dead_code)]
mod device;

#[cfg(feature = "soapysdr")]
#[path = "../src/soapy_source.rs"]
#[allow(dead_code)]
mod soapy_source;

#[path = "../src/block_schema.rs"]
#[allow(dead_code)]
mod block_schema;

#[path = "../src/routes.rs"]
#[allow(dead_code, clippy::unused_async)]
mod routes;

fn test_preset() -> FlowgraphDoc {
    // Cross-env preset with a `Source` placeholder — identical shape
    // to the shipped presets so this exercises the compose path.
    serde_json::from_value(json!({
        "name": "smoke",
        "environments": ["node", "browser"],
        "blocks": {
            "src":  { "type": "Source", "placement": "node",
                      "params": { "center_freq_hz": 0.0, "sample_rate_hz": 1000.0 } },
            "sink": { "type": "Decimator", "placement": "browser",
                      "params": { "factor": 2, "num_taps": 17,
                                  "cutoff_normalized": 0.2 } }
        },
        "wires": [["src.out", "sink.in"]]
    }))
    .unwrap()
}

fn test_source() -> SourceConfig {
    SourceConfig {
        type_name: "SineSource".into(),
        params: json!({
            "rate_hz": 1000.0,
            "tone_freq_abs_hz": 100.0,
            "center_freq_hz": 0.0,
            "amplitude": 0.5,
        }),
    }
}

async fn spawn_app(auto_start: bool) -> SocketAddr {
    let state = app_state::AppState::new(test_preset(), test_source(), Duration::from_millis(5));
    if auto_start {
        state.start().await.expect("auto-start");
    }
    let app = Router::new()
        .route("/api/hello", get(routes::hello))
        .route("/api/devices", get(routes::list_devices))
        .route(
            "/api/flowgraph",
            get(routes::get_flowgraph).patch(routes::patch_flowgraph),
        )
        .route(
            "/api/source",
            get(routes::get_source).patch(routes::patch_source),
        )
        .route("/api/pipeline", get(routes::pipeline_status))
        .route("/api/pipeline/start", post(routes::pipeline_start))
        .route("/api/pipeline/stop", post(routes::pipeline_stop))
        .route("/api/ui-sinks", get(routes::list_ui_sinks))
        .route("/api/blocks", get(routes::list_block_schemas))
        .route("/ws/preset", get(routes::ws_preset))
        .with_state(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test]
async fn hello_returns_ok_json() {
    let addr = spawn_app(false).await;
    let body = http_get(&format!("http://{addr}/api/hello")).await;
    assert!(body.contains("\"status\":\"ok\""), "body: {body}");
    assert!(body.contains("\"app\":\"ferrited\""), "body: {body}");
}

#[tokio::test]
async fn pipeline_starts_stopped_then_runs_on_start() {
    let addr = spawn_app(false).await;
    let s0 = http_get(&format!("http://{addr}/api/pipeline")).await;
    assert!(s0.contains("\"status\":\"stopped\""), "initial: {s0}");
    let started = http_post_json(&format!("http://{addr}/api/pipeline/start"), "").await;
    assert!(
        started.contains("\"status\":\"running\""),
        "started: {started}"
    );
    let s1 = http_get(&format!("http://{addr}/api/pipeline")).await;
    assert!(s1.contains("\"status\":\"running\""), "after start: {s1}");
    let stopped = http_post_json(&format!("http://{addr}/api/pipeline/stop"), "").await;
    assert!(
        stopped.contains("\"status\":\"stopped\""),
        "stopped: {stopped}"
    );
}

#[tokio::test]
async fn ws_preset_streams_iq_frames_once_started() {
    let addr = spawn_app(true).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/preset"))
        .await
        .unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("frame within 2s")
        .expect("ws stream open")
        .expect("ws ok");
    let bytes = match msg {
        Message::Binary(b) => b,
        other => panic!("expected binary, got {other:?}"),
    };
    match Frame::from_postcard(&bytes).expect("decode") {
        Frame::IqF32 { stream_id, .. } => {
            // First cross-env crossing gets CROSS_ENV_STREAM_BASE = 1000.
            assert_eq!(stream_id, 1000);
        }
        other => panic!("expected IqF32, got {other:?}"),
    }
}

#[tokio::test]
async fn get_flowgraph_returns_stored_preset() {
    let addr = spawn_app(false).await;
    let body = http_get(&format!("http://{addr}/api/flowgraph")).await;
    assert!(body.contains("\"name\":\"smoke\""), "body: {body}");
    assert!(body.contains("\"type\":\"Source\""), "body: {body}");
}

#[tokio::test]
async fn patch_source_while_stopped_stores_but_does_not_apply() {
    let addr = spawn_app(false).await;
    let new_source = json!({
        "type": "SineSource",
        "params": { "rate_hz": 1000.0, "tone_freq_abs_hz": 250.0, "amplitude": 0.25 }
    });
    let body = http_patch(
        &format!("http://{addr}/api/source"),
        &serde_json::to_string(&new_source).unwrap(),
    )
    .await;
    assert!(body.contains("\"applied\":false"), "patch: {body}");
    let after = http_get(&format!("http://{addr}/api/source")).await;
    assert!(after.contains("\"tone_freq_abs_hz\":250"), "after: {after}");
}

#[tokio::test]
async fn patch_source_while_running_reports_applied() {
    let addr = spawn_app(true).await;
    let new_source = json!({
        "type": "SineSource",
        "params": {
            "rate_hz": 1000.0, "tone_freq_abs_hz": 300.0,
            "center_freq_hz": 0.0, "amplitude": 0.1
        }
    });
    let body = http_patch(
        &format!("http://{addr}/api/source"),
        &serde_json::to_string(&new_source).unwrap(),
    )
    .await;
    assert!(body.contains("\"applied\":true"), "patch: {body}");
}

#[tokio::test]
async fn patch_source_rate_change_triggers_source_restart() {
    // `rate_hz` is the one SineSource param scoped as `SourceRestart`
    // (see blocks/src/sine.rs). Changing it exercises the same hot-
    // reconfigure machinery a full type-swap would trip, without needing
    // a second no-hardware source type.
    let addr = spawn_app(true).await;
    let new_source = json!({
        "type": "SineSource",
        "params": {
            "rate_hz": 2000.0, "tone_freq_abs_hz": 100.0,
            "center_freq_hz": 0.0, "amplitude": 0.5
        }
    });
    let body = http_patch(
        &format!("http://{addr}/api/source"),
        &serde_json::to_string(&new_source).unwrap(),
    )
    .await;
    assert!(body.contains("\"applied\":true"), "patch: {body}");
    assert!(
        body.contains("\"overall\":\"sourceRestart\""),
        "expected sourceRestart scope, got: {body}"
    );
    // After the restart, streaming should still be alive on /ws/preset.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/preset"))
        .await
        .unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("frame within 2s after source restart")
        .expect("ws stream open")
        .expect("ws ok");
    assert!(matches!(msg, Message::Binary(_)), "got {msg:?}");
}

#[tokio::test]
async fn patch_source_invalid_type_returns_error_and_preserves_state() {
    // compose_source accepts any `type_name` (string substitution), but
    // the downstream reconfigure rejects unregistered types. The PATCH
    // should fail with RECONFIGURE_FAILED and leave the stored source
    // config unchanged — a rollback the UI depends on.
    let addr = spawn_app(true).await;
    let bad_source = json!({
        "type": "NoSuchSourceType",
        "params": { "rate_hz": 1000.0 }
    });
    let (status, body) = raw_request_with_status(
        &format!("http://{addr}/api/source"),
        "PATCH",
        &serde_json::to_string(&bad_source).unwrap(),
    )
    .await;
    assert!(
        status >= 400 && status < 500,
        "status: {status}, body: {body}"
    );
    assert!(body.contains("RECONFIGURE_FAILED"), "body: {body}");

    // GET /api/source must still return the original SineSource.
    let after = http_get(&format!("http://{addr}/api/source")).await;
    assert!(after.contains("\"type\":\"SineSource\""), "after: {after}");
    assert!(
        after.contains("\"tone_freq_abs_hz\":100"),
        "rollback lost original params: {after}"
    );
}

#[tokio::test]
async fn patch_flowgraph_invalid_returns_error_and_preserves_state() {
    // Same rollback contract for PATCH /api/flowgraph: a bad preset
    // (here: a block of an unregistered type) must 4xx and leave the
    // stored doc intact.
    let addr = spawn_app(true).await;
    let bad_doc = json!({
        "name": "bad",
        "environments": ["node", "browser"],
        "blocks": {
            "src":  { "type": "Source", "placement": "node",
                      "params": { "center_freq_hz": 0.0, "sample_rate_hz": 1000.0 } },
            "sink": { "type": "NoSuchBlockType", "placement": "browser" }
        },
        "wires": [["src.out", "sink.in"]]
    });
    let (status, body) = raw_request_with_status(
        &format!("http://{addr}/api/flowgraph"),
        "PATCH",
        &serde_json::to_string(&bad_doc).unwrap(),
    )
    .await;
    assert!(
        status >= 400 && status < 500,
        "status: {status}, body: {body}"
    );
    assert!(body.contains("RECONFIGURE_FAILED"), "body: {body}");

    // GET /api/flowgraph must still be the original smoke preset.
    let after = http_get(&format!("http://{addr}/api/flowgraph")).await;
    assert!(after.contains("\"name\":\"smoke\""), "after: {after}");
    assert!(
        !after.contains("NoSuchBlockType"),
        "bad doc leaked into stored state: {after}"
    );
}

#[tokio::test]
async fn wbfm_preset_e2e_emits_iq_and_fft_streams() {
    // End-to-end smoke: load the shipped wbfm preset, feed it a
    // SineSource (no hardware required), connect to /ws/preset, and
    // confirm both crossings light up:
    //   - stream 1000: IQ crossing on `chan.out → decim.in`
    //   - stream 1001: FFT tap on `logmag.out → ui:fft`
    // This exercises the full pipeline compose → env_split → runtime
    // → frame bus → WS path against the same preset the web UI ships.
    let preset_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("flowgraphs/wbfm.json");
    let preset: FlowgraphDoc =
        serde_json::from_str(&std::fs::read_to_string(&preset_path).expect("wbfm.json readable"))
            .expect("wbfm.json parses");

    // Match the preset's 2.4 MS/s hint so Channelizer runs at its
    // authored input rate. Tone a little below centre so the FFT has a
    // well-defined non-zero bin.
    let source = SourceConfig {
        type_name: "SineSource".into(),
        params: json!({
            "rate_hz": 2_400_000.0,
            "tone_freq_abs_hz": 100_050_000.0,
            "amplitude": 0.5,
        }),
    };
    let state = app_state::AppState::new(preset, source, Duration::from_millis(5));
    state.start().await.expect("wbfm auto-start");
    let app = Router::new()
        .route("/ws/preset", get(routes::ws_preset))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws/preset"))
        .await
        .expect("ws connect");

    // Collect frames for up to 2s, tagging each with its stream id.
    // wbfm's FFT runs on the raw 2.4 MS/s source at size 16384, so it
    // fires every ~6.8 ms; IQ crossings fire every runtime tick. Both
    // stream ids should appear well within the window.
    let mut saw_iq = false;
    let mut saw_fft = false;
    let mut counts: std::collections::BTreeMap<(u16, &'static str), usize> =
        std::collections::BTreeMap::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && (!saw_iq || !saw_fft) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = match tokio::time::timeout(remaining, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            _ => break,
        };
        let Message::Binary(bytes) = msg else {
            continue;
        };
        match Frame::from_postcard(&bytes).expect("decode") {
            Frame::IqF32 { stream_id, .. } => {
                *counts.entry((stream_id, "IqF32")).or_default() += 1;
                if stream_id == 1000 {
                    saw_iq = true;
                }
            }
            Frame::FftU8 {
                stream_id, payload, ..
            } => {
                *counts.entry((stream_id, "FftU8")).or_default() += 1;
                if stream_id == 1001 {
                    assert_eq!(payload.len(), 16384, "expected 16384 FFT bins");
                    saw_fft = true;
                }
            }
            _ => {}
        }
    }
    assert!(saw_iq, "no IqF32 on stream 1000 within 2s; saw {counts:?}");
    assert!(saw_fft, "no FftU8 on stream 1001 within 2s; saw {counts:?}");
}

#[tokio::test]
async fn ui_sinks_returns_fft_stream_id_for_server_side_tap() {
    // Preset with a single `ui:fft` sink from a node-side logmag chain.
    // env_split assigns stream_id=1000 on the first slot, payload=FftU8.
    let preset: FlowgraphDoc = serde_json::from_value(json!({
        "name": "ui_sinks_smoke",
        "environments": ["node", "browser"],
        "blocks": {
            "src":    { "type": "Source", "placement": "node",
                        "params": { "center_freq_hz": 0.0, "sample_rate_hz": 1000.0 } },
            "fft":    { "type": "FFT", "placement": "node",
                        "params": { "size": 64, "window": "hann" } },
            "logmag": { "type": "LogMagU8", "placement": "node",
                        "params": { "size": 64, "floor_dbfs": -100.0,
                                    "ceil_dbfs": 0.0, "alpha": 0.3 } }
        },
        "wires": [
            ["src.out",    "fft.in"],
            ["fft.out",    "logmag.in"],
            ["logmag.out", "ui:fft"]
        ]
    }))
    .unwrap();
    let state = app_state::AppState::new(preset, test_source(), Duration::from_millis(5));
    let app = Router::new()
        .route("/api/ui-sinks", get(routes::list_ui_sinks))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let body = http_get(&format!("http://{addr}/api/ui-sinks")).await;
    assert!(body.contains("\"name\":\"fft\""), "body: {body}");
    assert!(body.contains("\"stream_id\":1000"), "body: {body}");
    assert!(body.contains("\"payload_type\":\"FftU8\""), "body: {body}");
}

/// `GET /api/devices` shape depends on build configuration:
/// - no `soapysdr` feature → 501 with a `SOAPYSDR_FEATURE_DISABLED` body
/// - with feature, no hardware plugged in → 200 + `[]`
#[tokio::test]
async fn devices_endpoint_shape_matches_build() {
    let addr = spawn_app(false).await;
    let body = http_get(&format!("http://{addr}/api/devices")).await;
    #[cfg(not(feature = "soapysdr"))]
    assert!(
        body.contains("SOAPYSDR_FEATURE_DISABLED"),
        "expected the feature-disabled error body, got: {body}"
    );
    #[cfg(feature = "soapysdr")]
    assert!(
        body.trim() == "[]" || body.contains("\"status\":"),
        "expected empty array or tagged DeviceEntry list, got: {body}"
    );
}

/// Tiny HTTP GET helper — avoids pulling reqwest into dev-deps just for
/// a couple of assertions.
async fn http_get(url: &str) -> String {
    raw_request(url, "GET", "").await
}

async fn http_post_json(url: &str, body: &str) -> String {
    raw_request(url, "POST", body).await
}

async fn http_patch(url: &str, body: &str) -> String {
    raw_request(url, "PATCH", body).await
}

async fn raw_request(url: &str, method: &str, body: &str) -> String {
    raw_request_with_status(url, method, body).await.1
}

/// Returns `(status_code, body)`. Status is parsed from the HTTP/1.1
/// status line so tests can assert 4xx paths without pulling reqwest in.
async fn raw_request_with_status(url: &str, method: &str, body: &str) -> (u16, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let url = url.trim_start_matches("http://");
    let (host, path) = url.split_once('/').map_or((url, "/"), |(h, p)| (h, p));
    let path = format!("/{path}");
    let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n{body}",
        len = body.len(),
    );
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = text
        .split_once("\r\n\r\n")
        .map_or(text.clone(), |(_, b)| b.to_string());
    (status, body)
}
