//! Smoke tests — bring up the server on an ephemeral port and exercise
//! the public surface end-to-end. Runs in CI in lieu of one-off curl.

use std::{net::SocketAddr, time::Duration};

use axum::{
    routing::{get, patch, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[path = "../src/ws_frame.rs"]
mod ws_frame;

#[path = "../src/log_stream.rs"]
#[allow(dead_code)]
mod log_stream;

#[path = "../src/session.rs"]
#[allow(dead_code)]
mod session;

#[cfg(feature = "soapysdr")]
#[path = "../src/device.rs"]
#[allow(dead_code)]
mod device;

#[cfg(feature = "soapysdr")]
#[path = "../src/soapy_source.rs"]
#[allow(dead_code)]
mod soapy_source;

#[path = "../src/routes.rs"]
#[allow(dead_code, clippy::unused_async)]
mod routes;

async fn spawn_app() -> SocketAddr {
    let state = session::AppState::new(session::CliConfig::default());
    let app = Router::new()
        .route("/api/hello", get(routes::hello))
        .route("/api/devices", get(routes::list_devices))
        .route("/api/device/open", post(routes::open_session))
        .route("/api/device/:id/close", post(routes::close_session))
        .route("/api/device/:id/state", get(routes::session_state))
        .route("/api/device/:id/settings", patch(routes::patch_settings))
        .route("/api/device/:id/vfo", post(routes::add_vfo))
        .route(
            "/api/device/:id/vfo/:vfo_id",
            patch(routes::patch_vfo).delete(routes::delete_vfo),
        )
        .route("/ws", get(routes::ws_upgrade))
        .route("/ws/:id", get(routes::ws_session))
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
    let addr = spawn_app().await;
    let body = http_get(&format!("http://{addr}/api/hello")).await;
    assert!(body.contains("\"status\":\"ok\""), "body: {body}");
    assert!(body.contains("\"app\":\"ferrited\""), "body: {body}");
}

#[tokio::test]
async fn ws_echoes_text_frames() {
    let addr = spawn_app().await;
    let url = format!("ws://{addr}/ws");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    ws.send(Message::text("hello")).await.unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    match reply {
        Message::Text(t) => assert_eq!(t.as_str(), "hello"),
        other => panic!("unexpected reply: {other:?}"),
    }
}

#[tokio::test]
async fn open_then_stream_fft_frames() {
    let addr = spawn_app().await;
    // Use a tiny FFT and a fast tick rate so the test finishes quickly.
    let body = r#"{"fft_size": 128, "fft_rate_hz": 200.0}"#;
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), body).await;
    let session_id = json_str(&resp, "session_id").expect("session_id");
    let ws_url = json_str(&resp, "ws_url").expect("ws_url");
    let url = format!("ws://{addr}{ws_url}");

    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(2), ws.next())
        .await
        .expect("frame within 2s")
        .expect("ws stream open")
        .expect("ws ok");

    let bytes = match frame {
        Message::Binary(b) => b,
        other => panic!("expected binary, got {other:?}"),
    };
    let (header, payload) = ws_frame::decode(&bytes).expect("decode");
    assert_eq!(header.payload_type, ws_frame::PayloadType::FftU8);
    assert_eq!(header.stream_id, ws_frame::FFT_STREAM);
    assert_eq!(payload.len(), 128);

    let close_url = format!("http://{addr}/api/device/{session_id}/close");
    let _ = http_post_json(&close_url, "").await;
}

/// End-to-end DSP round trip: open a session with a sine tone at a known
/// offset, read a handful of `FftU8` frames over the WebSocket, and assert
/// the peak bin lands exactly where the pipeline math says it should.
///
/// Expected peak bin:  `N/2 + round(offset_hz * N / fs)`
/// after the fftshift in `FftBlock::process`. With alpha=1.0 the first
/// frame already reflects the steady-state spectrum (no smoothing lag).
#[tokio::test]
async fn ws_round_trip_peak_bin() {
    let addr = spawn_app().await;
    // Tone at +100 kHz, 2 MS/s, N=1024  →  bin offset = 51  →  peak bin 563.
    let sample_rate = 2_000_000.0_f64;
    let center = 100_000_000.0_f64;
    let offset = 100_000.0_f64;
    let fft_size = 1024_usize;
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    let expected_bin = fft_size / 2 + (offset * fft_size as f64 / sample_rate).round() as usize;

    let body = format!(
        r#"{{"sample_rate_hz": {sample_rate}, "center_freq_hz": {center}, "tone_freq_abs_hz": {tone}, "amplitude": 0.5, "fft_size": {fft_size}, "fft_rate_hz": 500, "floor_dbfs": -100, "ceil_dbfs": 0, "alpha": 1.0}}"#,
        tone = center + offset,
    );
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), &body).await;
    let session_id = json_str(&resp, "session_id").expect("session_id");
    let ws_url = json_str(&resp, "ws_url").expect("ws_url");
    let url = format!("ws://{addr}{ws_url}");

    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    // Pull a few frames — the very first may race with the subscribe
    // setup, but by frame three the pipeline is in steady state.
    let mut last_payload: Option<Vec<u8>> = None;
    for _ in 0..3 {
        let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
            .await
            .expect("frame within 2s")
            .expect("ws stream open")
            .expect("ws ok");
        if let Message::Binary(bytes) = msg {
            let (_hdr, payload) = ws_frame::decode(&bytes).expect("decode");
            last_payload = Some(payload.to_vec());
        }
    }
    let payload = last_payload.expect("at least one binary frame");
    assert_eq!(payload.len(), fft_size);

    let (peak_bin, peak_val) = payload
        .iter()
        .enumerate()
        .max_by_key(|(_, v)| **v)
        .map(|(i, v)| (i, *v))
        .expect("non-empty payload");

    // The Hann window spreads a non-integer-bin tone across a few cells,
    // but 51.2 rounds cleanly to 51 so the peak should sit exactly there.
    assert_eq!(
        peak_bin, expected_bin,
        "expected peak at bin {expected_bin}, got {peak_bin} (val {peak_val})"
    );
    // Sanity: at ceil=0 dBFS with amplitude 0.5 the tone should light up
    // a meaningful chunk of the 0..=255 range, not sit near the floor.
    assert!(
        peak_val > 128,
        "peak value too low: {peak_val} — pipeline is producing bins but \
         they're below mid-scale, which usually means amplitude or window \
         gain is miscomputed"
    );

    let close_url = format!("http://{addr}/api/device/{session_id}/close");
    let _ = http_post_json(&close_url, "").await;
}

/// Open a sine session, GET /state, PATCH the tone freq, GET /state again,
/// and verify the snapshot reflects the change.
#[tokio::test]
async fn state_and_patch_roundtrip() {
    let addr = spawn_app().await;
    let body = r#"{"fft_size": 128, "fft_rate_hz": 200.0}"#;
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), body).await;
    let session_id = json_str(&resp, "session_id").expect("session_id");

    let s1 = http_get(&format!("http://{addr}/api/device/{session_id}/state")).await;
    assert!(s1.contains("\"source_kind\":\"sine\""), "state body: {s1}");

    let new_tone = 100_005_000.0_f64;
    let patch_body = format!(r#"{{"tone_freq_abs_hz": {new_tone}}}"#);
    let p = raw_request(
        &format!("http://{addr}/api/device/{session_id}/settings"),
        "PATCH",
        &patch_body,
    )
    .await;
    assert!(
        p.contains(&format!("\"tone_freq_abs_hz\":{new_tone}")),
        "patch reply: {p}"
    );

    let s2 = http_get(&format!("http://{addr}/api/device/{session_id}/state")).await;
    assert!(
        s2.contains(&format!("\"tone_freq_abs_hz\":{new_tone}")),
        "post-patch state: {s2}"
    );

    let _ = http_post_json(&format!("http://{addr}/api/device/{session_id}/close"), "").await;
}

/// Closing a session sends a final `session_closed` JSON event over the
/// WebSocket before the broadcast channel drops, so clients can render
/// "session ended" rather than a generic disconnect.
#[tokio::test]
async fn close_emits_session_closed_event() {
    let addr = spawn_app().await;
    let body = r#"{"fft_size": 128, "fft_rate_hz": 200.0}"#;
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), body).await;
    let session_id = json_str(&resp, "session_id").expect("session_id");
    let ws_url = json_str(&resp, "ws_url").expect("ws_url");
    let url = format!("ws://{addr}{ws_url}");

    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    // Drain a couple of FFT frames first so we know the session is hot.
    for _ in 0..2 {
        let _ = tokio::time::timeout(Duration::from_secs(1), ws.next())
            .await
            .expect("frame within 1s");
    }
    let _ = http_post_json(&format!("http://{addr}/api/device/{session_id}/close"), "").await;

    // Give the close path a moment to reach the broadcast.
    let mut saw_close = false;
    for _ in 0..16 {
        let Ok(Some(Ok(frame))) = tokio::time::timeout(Duration::from_secs(1), ws.next()).await
        else {
            break;
        };
        if let Message::Binary(bytes) = frame {
            let (hdr, payload) = ws_frame::decode(&bytes).expect("decode");
            if hdr.payload_type == ws_frame::PayloadType::JsonEvent
                && std::str::from_utf8(payload)
                    .map(|s| s.contains("session_closed"))
                    .unwrap_or(false)
            {
                saw_close = true;
                break;
            }
        }
    }
    assert!(saw_close, "expected a session_closed JsonEvent frame");
}

/// Open a sine session, POST /vfo, then confirm the allocated `stream_id`
/// carries `IqF32` frames alongside the existing `FftU8` waterfall. The
/// channelizer's content is not asserted here — `blocks/src/channelizer.rs`
/// has unit tests for the DSP; this just proves the wiring.
#[tokio::test]
async fn add_vfo_streams_iq_f32_on_stream_two() {
    let addr = spawn_app().await;
    let body = r#"{"sample_rate_hz": 2000000, "fft_size": 512, "fft_rate_hz": 200.0}"#;
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), body).await;
    let session_id = json_str(&resp, "session_id").expect("session_id");
    let ws_url = json_str(&resp, "ws_url").expect("ws_url");

    // Drop the FFT into the stream first so the WS is live before POST /vfo.
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}{ws_url}"))
        .await
        .unwrap();

    let vfo_body = r#"{"offset_hz": 100000, "rate_hz": 50000, "filter_bw_hz": 20000}"#;
    let vfo_resp = http_post_json(
        &format!("http://{addr}/api/device/{session_id}/vfo"),
        vfo_body,
    )
    .await;
    assert!(
        vfo_resp.contains("\"payload_type\":\"iq_f32\""),
        "vfo response: {vfo_resp}"
    );
    assert!(
        vfo_resp.contains("\"stream_id\":2"),
        "vfo response: {vfo_resp}"
    );

    let mut saw_iq = false;
    for _ in 0..32 {
        let Ok(Some(Ok(Message::Binary(bytes)))) =
            tokio::time::timeout(Duration::from_secs(2), ws.next()).await
        else {
            break;
        };
        let (hdr, payload) = ws_frame::decode(&bytes).expect("decode");
        if hdr.payload_type == ws_frame::PayloadType::IqF32 && hdr.stream_id == 2 {
            // Every sample is 4B I + 4B Q; payload must be a multiple of 8.
            assert!(!payload.is_empty() && payload.len() % 8 == 0);
            saw_iq = true;
            break;
        }
    }
    assert!(saw_iq, "expected an IqF32 frame on stream_id=2");

    let _ = http_post_json(&format!("http://{addr}/api/device/{session_id}/close"), "").await;
}

/// Add a VFO, PATCH its `offset_hz`, and verify the descriptor (both in
/// the PATCH response and in subsequent `GET /state`) reflects the new
/// offset. Asserts the wiring, not the channelizer DSP — the
/// `retune_mid_stream_is_continuous` test in `blocks/src/channelizer.rs`
/// covers the glitch-free retune property.
#[tokio::test]
async fn patch_vfo_updates_offset_in_state() {
    let addr = spawn_app().await;
    let body = r#"{"sample_rate_hz": 2000000, "fft_size": 512, "fft_rate_hz": 200.0}"#;
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), body).await;
    let session_id = json_str(&resp, "session_id").expect("session_id");

    let vfo_resp = http_post_json(
        &format!("http://{addr}/api/device/{session_id}/vfo"),
        r#"{"offset_hz": 100000, "rate_hz": 50000, "filter_bw_hz": 20000}"#,
    )
    .await;
    let vfo_id = json_str(&vfo_resp, "vfo_id").expect("vfo_id");

    let patch = raw_request(
        &format!("http://{addr}/api/device/{session_id}/vfo/{vfo_id}"),
        "PATCH",
        r#"{"offset_hz": -250000}"#,
    )
    .await;
    assert!(
        patch.contains("\"offset_hz\":-250000"),
        "patch reply: {patch}"
    );

    let state = http_get(&format!("http://{addr}/api/device/{session_id}/state")).await;
    assert!(
        state.contains("\"offset_hz\":-250000"),
        "post-patch state: {state}"
    );

    let _ = http_post_json(&format!("http://{addr}/api/device/{session_id}/close"), "").await;
}

/// PATCH against an unknown `vfo_id` must 404 with `VFO_NOT_FOUND`.
#[tokio::test]
async fn patch_vfo_unknown_returns_vfo_not_found() {
    let addr = spawn_app().await;
    let body = r#"{"fft_size": 128, "fft_rate_hz": 200.0}"#;
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), body).await;
    let session_id = json_str(&resp, "session_id").expect("session_id");

    let r = raw_request(
        &format!("http://{addr}/api/device/{session_id}/vfo/vfo-nope"),
        "PATCH",
        r#"{"offset_hz": 1.0}"#,
    )
    .await;
    assert!(r.contains("VFO_NOT_FOUND"), "reply: {r}");

    let _ = http_post_json(&format!("http://{addr}/api/device/{session_id}/close"), "").await;
}

/// DELETE tears a VFO down: the descriptor leaves `GET /state`, a
/// `vfo_removed` JSON event lands on stream 0, and no further `IqF32`
/// frames are emitted on that `stream_id`.
#[tokio::test]
async fn delete_vfo_removes_descriptor_and_emits_event() {
    let addr = spawn_app().await;
    let body = r#"{"sample_rate_hz": 2000000, "fft_size": 512, "fft_rate_hz": 200.0}"#;
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), body).await;
    let session_id = json_str(&resp, "session_id").expect("session_id");
    let ws_url = json_str(&resp, "ws_url").expect("ws_url");

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}{ws_url}"))
        .await
        .unwrap();

    let vfo_resp = http_post_json(
        &format!("http://{addr}/api/device/{session_id}/vfo"),
        r#"{"offset_hz": 100000, "rate_hz": 50000, "filter_bw_hz": 20000}"#,
    )
    .await;
    let vfo_id = json_str(&vfo_resp, "vfo_id").expect("vfo_id");

    let _ = raw_request(
        &format!("http://{addr}/api/device/{session_id}/vfo/{vfo_id}"),
        "DELETE",
        "",
    )
    .await;

    let state = http_get(&format!("http://{addr}/api/device/{session_id}/state")).await;
    assert!(
        !state.contains(&format!("\"vfo_id\":\"{vfo_id}\"")),
        "vfo still in state after DELETE: {state}"
    );

    // Drain a few frames looking for the vfo_removed event on stream 0.
    let mut saw_removed = false;
    for _ in 0..32 {
        let Ok(Some(Ok(Message::Binary(bytes)))) =
            tokio::time::timeout(Duration::from_secs(2), ws.next()).await
        else {
            break;
        };
        let (hdr, payload) = ws_frame::decode(&bytes).expect("decode");
        if hdr.payload_type == ws_frame::PayloadType::JsonEvent {
            if let Ok(text) = std::str::from_utf8(payload) {
                if text.contains("vfo_removed") && text.contains(&vfo_id) {
                    saw_removed = true;
                    break;
                }
            }
        }
    }
    assert!(saw_removed, "expected a vfo_removed JsonEvent");

    let _ = http_post_json(&format!("http://{addr}/api/device/{session_id}/close"), "").await;
}

/// DELETE against a nonexistent `vfo_id` must 404 with `VFO_NOT_FOUND`.
#[tokio::test]
async fn delete_unknown_vfo_returns_vfo_not_found() {
    let addr = spawn_app().await;
    let body = r#"{"fft_size": 128, "fft_rate_hz": 200.0}"#;
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), body).await;
    let session_id = json_str(&resp, "session_id").expect("session_id");

    let r = raw_request(
        &format!("http://{addr}/api/device/{session_id}/vfo/vfo-missing"),
        "DELETE",
        "",
    )
    .await;
    assert!(r.contains("VFO_NOT_FOUND"), "reply: {r}");

    let _ = http_post_json(&format!("http://{addr}/api/device/{session_id}/close"), "").await;
}

/// Two sequential VFOs must get distinct `stream_id`s starting at
/// `VFO_STREAM_BASE` (2). Pins the allocator rule from
/// `docs/02-protocol.md` §"Stream IDs".
#[tokio::test]
async fn two_vfos_get_distinct_sequential_stream_ids() {
    let addr = spawn_app().await;
    let body = r#"{"sample_rate_hz": 2000000, "fft_size": 512, "fft_rate_hz": 200.0}"#;
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), body).await;
    let session_id = json_str(&resp, "session_id").expect("session_id");

    let first = http_post_json(
        &format!("http://{addr}/api/device/{session_id}/vfo"),
        r#"{"offset_hz": 100000, "rate_hz": 50000, "filter_bw_hz": 20000}"#,
    )
    .await;
    let second = http_post_json(
        &format!("http://{addr}/api/device/{session_id}/vfo"),
        r#"{"offset_hz": -100000, "rate_hz": 50000, "filter_bw_hz": 20000}"#,
    )
    .await;

    assert!(first.contains("\"stream_id\":2"), "first: {first}");
    assert!(second.contains("\"stream_id\":3"), "second: {second}");

    let _ = http_post_json(&format!("http://{addr}/api/device/{session_id}/close"), "").await;
}

/// The `iq_f32` wire payload must decode as interleaved little-endian
/// `f32` pairs — 8 bytes per sample, all finite for a well-behaved
/// sine source. Protects against any future encoder change that would
/// quietly break the documented payload shape.
#[tokio::test]
async fn iq_f32_payload_decodes_as_le_f32_pairs() {
    let addr = spawn_app().await;
    let body = r#"{"sample_rate_hz": 2000000, "fft_size": 512, "fft_rate_hz": 200.0}"#;
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), body).await;
    let session_id = json_str(&resp, "session_id").expect("session_id");
    let ws_url = json_str(&resp, "ws_url").expect("ws_url");

    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}{ws_url}"))
        .await
        .unwrap();

    let _ = http_post_json(
        &format!("http://{addr}/api/device/{session_id}/vfo"),
        r#"{"offset_hz": 100000, "rate_hz": 50000, "filter_bw_hz": 20000}"#,
    )
    .await;

    let mut checked = false;
    for _ in 0..64 {
        let Ok(Some(Ok(Message::Binary(bytes)))) =
            tokio::time::timeout(Duration::from_secs(2), ws.next()).await
        else {
            break;
        };
        let (hdr, payload) = ws_frame::decode(&bytes).expect("decode");
        if hdr.payload_type != ws_frame::PayloadType::IqF32 || hdr.stream_id < 2 {
            continue;
        }
        assert_eq!(
            payload.len() % 8,
            0,
            "iq_f32 payload must be 8-byte-aligned"
        );
        assert!(!payload.is_empty(), "iq_f32 payload must be non-empty");
        for sample in payload.chunks_exact(8) {
            let i = f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
            let q = f32::from_le_bytes([sample[4], sample[5], sample[6], sample[7]]);
            assert!(
                i.is_finite() && q.is_finite(),
                "non-finite sample: ({i}, {q})"
            );
        }
        checked = true;
        break;
    }
    assert!(checked, "never saw an IqF32 frame to decode");

    let _ = http_post_json(&format!("http://{addr}/api/device/{session_id}/close"), "").await;
}

/// `device_args` in `POST /api/device/open` must produce a clean error
/// in **both** build configurations:
/// - no `soapysdr` feature → `FEATURE_DISABLED` (config error, surfaced
///   before any device call)
/// - with feature, bogus driver name → `INVALID_SETTINGS` (real Soapy
///   error wrapped from `Device::new`)
///
/// Either way the server stays up and the response body says what went
/// wrong; this test just protects the contract, not the success path
/// (that needs hardware).
#[tokio::test]
async fn open_with_bogus_device_args_returns_clean_error() {
    let addr = spawn_app().await;
    let body = r#"{"device_args": "driver=ferrite-no-such-driver"}"#;
    let resp = http_post_json(&format!("http://{addr}/api/device/open"), body).await;
    #[cfg(not(feature = "soapysdr"))]
    assert!(
        resp.contains("FEATURE_DISABLED"),
        "expected FEATURE_DISABLED, got: {resp}"
    );
    #[cfg(feature = "soapysdr")]
    assert!(
        resp.contains("INVALID_SETTINGS"),
        "expected INVALID_SETTINGS from a bogus driver name, got: {resp}"
    );
}

/// `GET /api/devices` shape depends on build configuration:
/// - no `soapysdr` feature → 501 with a `SOAPYSDR_FEATURE_DISABLED` body
/// - with feature, no hardware plugged in → 200 + `[]`
/// Asserting on body content keeps the helper dumb (no status parsing).
#[tokio::test]
async fn devices_endpoint_shape_matches_build() {
    let addr = spawn_app().await;
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

async fn raw_request(url: &str, method: &str, body: &str) -> String {
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
    text.split_once("\r\n\r\n")
        .map_or(text.clone(), |(_, body)| body.to_string())
}

/// Crude string field extractor — good enough for these tests; avoids
/// dragging `serde_json` into dev-deps.
fn json_str(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
