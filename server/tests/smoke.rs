//! Smoke tests — bring up the server on an ephemeral port and exercise
//! the public surface end-to-end. Runs in CI in lieu of one-off curl.

use std::{net::SocketAddr, time::Duration};

use axum::{
    routing::{get, post},
    Router,
};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[path = "../src/ws_frame.rs"]
mod ws_frame;

#[path = "../src/session.rs"]
#[allow(dead_code)]
mod session;

#[path = "../src/routes.rs"]
mod routes;

async fn spawn_app() -> SocketAddr {
    let state = session::AppState::new();
    let app = Router::new()
        .route("/api/hello", get(routes::hello))
        .route("/api/device/open", post(routes::open_session))
        .route("/api/device/:id/close", post(routes::close_session))
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
