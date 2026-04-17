//! Smoke tests — bring up the server on an ephemeral port and exercise
//! both endpoints. Replaces the one-off curl workflow; this runs in CI.

use std::net::SocketAddr;

use axum::{routing::get, Router};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[path = "../src/routes.rs"]
mod routes;

async fn spawn_app() -> SocketAddr {
    let app = Router::new()
        .route("/api/hello", get(routes::hello))
        .route("/ws", get(routes::ws_upgrade));

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
    let body = reqwest_like_get(&format!("http://{addr}/api/hello")).await;
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

/// Tiny HTTP GET helper — avoids pulling reqwest into dev-deps just for
/// a couple of assertions.
async fn reqwest_like_get(url: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let url = url.trim_start_matches("http://");
    let (host, path) = url.split_once('/').map_or((url, "/"), |(h, p)| (h, p));
    let path = format!("/{path}");
    let mut stream = tokio::net::TcpStream::connect(host).await.unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf).to_string();
    text.split_once("\r\n\r\n")
        .map_or(text.clone(), |(_, body)| body.to_string())
}
