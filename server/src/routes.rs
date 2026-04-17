//! HTTP and WebSocket route handlers.

use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    Json,
};
use serde::Serialize;

#[derive(Serialize)]
pub struct Hello {
    pub app: &'static str,
    pub version: &'static str,
    pub status: &'static str,
}

pub async fn hello() -> Json<Hello> {
    Json(Hello {
        app: "ferrited",
        version: env!("CARGO_PKG_VERSION"),
        status: "ok",
    })
}

/// WS upgrade for `/ws`. In Phase A this only echoes text frames so the
/// full transport layer is exercised end-to-end. The binary frame codec
/// and stream multiplexer arrive in Phase B.
pub async fn ws_upgrade(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(ws_echo)
}

async fn ws_echo(mut socket: WebSocket) {
    tracing::debug!("ws connected");
    while let Some(msg) = socket.recv().await {
        let msg = match msg {
            Ok(m) => m,
            Err(err) => {
                tracing::debug!(?err, "ws recv error");
                return;
            }
        };
        match msg {
            Message::Text(text) => {
                if socket.send(Message::Text(text)).await.is_err() {
                    return;
                }
            }
            Message::Binary(bytes) => {
                if socket.send(Message::Binary(bytes)).await.is_err() {
                    return;
                }
            }
            Message::Ping(p) => {
                let _ = socket.send(Message::Pong(p)).await;
            }
            Message::Close(_) | Message::Pong(_) => return,
        }
    }
}
