//! HTTP and WebSocket route handlers.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
    Json,
};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::session::{AppState, OpenSpec};

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

/// Legacy `/ws` echo — kept so the Phase A smoke test still passes. The
/// real per-session stream upgrades on `/ws/:session_id` (see [`ws_session`]).
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

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct OpenRequest {
    pub sample_rate_hz: Option<f64>,
    pub center_freq_hz: Option<f64>,
    pub tone_freq_abs_hz: Option<f64>,
    pub amplitude: Option<f32>,
    pub fft_size: Option<usize>,
    pub fft_rate_hz: Option<f32>,
    pub floor_dbfs: Option<f32>,
    pub ceil_dbfs: Option<f32>,
    pub alpha: Option<f32>,
}

#[derive(Serialize)]
pub struct StreamDescriptor {
    pub stream_id: u16,
    pub payload_type: &'static str,
    pub size: usize,
    pub rate_hz: f32,
}

#[derive(Serialize)]
pub struct OpenResponse {
    pub session_id: String,
    pub ws_url: String,
    pub fft: StreamDescriptor,
}

#[derive(Serialize)]
pub struct ApiError {
    pub error: ApiErrorBody,
}

#[derive(Serialize)]
pub struct ApiErrorBody {
    pub code: &'static str,
    pub message: String,
}

pub async fn open_session(
    State(state): State<AppState>,
    Json(req): Json<OpenRequest>,
) -> Result<Json<OpenResponse>, (StatusCode, Json<ApiError>)> {
    let d = OpenSpec::default();
    let spec = OpenSpec {
        sample_rate_hz: req.sample_rate_hz.unwrap_or(d.sample_rate_hz),
        center_freq_hz: req.center_freq_hz.unwrap_or(d.center_freq_hz),
        tone_freq_abs_hz: req.tone_freq_abs_hz.unwrap_or(d.tone_freq_abs_hz),
        amplitude: req.amplitude.unwrap_or(d.amplitude),
        fft_size: req.fft_size.unwrap_or(d.fft_size),
        fft_rate_hz: req.fft_rate_hz.unwrap_or(d.fft_rate_hz),
        floor_dbfs: req.floor_dbfs.unwrap_or(d.floor_dbfs),
        ceil_dbfs: req.ceil_dbfs.unwrap_or(d.ceil_dbfs),
        alpha: req.alpha.unwrap_or(d.alpha),
    };
    let opened = state.open(spec).await.map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: ApiErrorBody {
                    code: "INVALID_SETTINGS",
                    message: e.to_string(),
                },
            }),
        )
    })?;
    let ws_url = format!("/ws/{}", opened.id);
    Ok(Json(OpenResponse {
        session_id: opened.id,
        ws_url,
        fft: StreamDescriptor {
            stream_id: crate::ws_frame::FFT_STREAM,
            payload_type: "fft_u8",
            size: opened.fft_size,
            rate_hz: opened.fft_rate_hz,
        },
    }))
}

pub async fn close_session(State(state): State<AppState>, Path(id): Path<String>) -> StatusCode {
    if state.close(&id).await {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::NOT_FOUND
    }
}

pub async fn ws_session(
    ws: WebSocketUpgrade,
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_session(socket, id, state))
}

async fn handle_session(mut socket: WebSocket, id: String, state: AppState) {
    let Some(mut rx) = state.subscribe(&id).await else {
        let _ = socket.send(Message::Close(None)).await;
        return;
    };
    tracing::debug!(session = %id, "ws subscribed");
    loop {
        tokio::select! {
            client = socket.recv() => {
                match client {
                    None | Some(Ok(Message::Close(_))) => return,
                    Some(Err(err)) => {
                        tracing::debug!(?err, "ws recv");
                        return;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if socket.send(Message::Pong(p)).await.is_err() { return; }
                    }
                    // The protocol is server-push only for now. Ignore
                    // text/binary/pong from the client.
                    _ => {}
                }
            }
            frame = rx.recv() => match frame {
                Ok(bytes) => {
                    if socket.send(Message::Binary((*bytes).clone())).await.is_err() {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "ws subscriber lagged");
                }
            }
        }
    }
}
