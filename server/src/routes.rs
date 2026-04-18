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

use crate::session::{AppState, OpenSpec, SourceKind};

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
    /// `SoapySDR` device args (e.g. `driver=rtlsdr,serial=00000001`).
    /// When set, this session opens that device instead of the CLI
    /// default source. Requires the `soapysdr` feature on the server.
    pub device_args: Option<String>,
    pub antenna: Option<String>,
    pub gain_db: Option<f64>,
    pub agc: Option<bool>,
    pub bandwidth_hz: Option<f64>,
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

/// One row in the `GET /api/devices` response. Probing can fail per-device
/// (e.g. driver loaded but hardware already held by another process), so
/// each entry is either `Available` with the full capability schema or
/// `Unavailable` with the enumerate-time info plus the probe error.
#[cfg(feature = "soapysdr")]
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DeviceEntry {
    Available(Box<crate::device::DeviceCapabilities>),
    Unavailable {
        info: crate::device::DeviceInfo,
        error: String,
    },
}

/// `GET /api/devices` — enumerate every `SoapySDR` device and probe each
/// one for its full capability schema. Returns 501 on builds without the
/// `soapysdr` feature so the web UI can render a clear "server built
/// without hardware support" state instead of hitting a 404.
///
/// Probing opens + closes every device in turn. Soapy serialises device
/// access, so an in-use device's entry will be `Unavailable` rather than
/// failing the whole response.
#[cfg(feature = "soapysdr")]
pub async fn list_devices() -> Result<Json<Vec<DeviceEntry>>, (StatusCode, Json<ApiError>)> {
    let entries = tokio::task::spawn_blocking(probe_all_devices)
        .await
        .map_err(|e| internal(format!("device probe task panicked: {e}")))?
        .map_err(|e| internal(format!("{e:#}")))?;
    Ok(Json(entries))
}

#[cfg(feature = "soapysdr")]
fn probe_all_devices() -> anyhow::Result<Vec<DeviceEntry>> {
    let devices = crate::device::list_devices()?;
    let mut entries = Vec::with_capacity(devices.len());
    for info in devices {
        let args = info.args_string();
        match crate::device::probe(&args) {
            Ok(caps) => entries.push(DeviceEntry::Available(Box::new(caps))),
            Err(err) => entries.push(DeviceEntry::Unavailable {
                info,
                error: format!("{err:#}"),
            }),
        }
    }
    Ok(entries)
}

#[cfg(feature = "soapysdr")]
fn internal(message: String) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ApiError {
            error: ApiErrorBody {
                code: "DEVICE_PROBE_FAILED",
                message,
            },
        }),
    )
}

#[cfg(not(feature = "soapysdr"))]
pub async fn list_devices() -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(ApiError {
            error: ApiErrorBody {
                code: "SOAPYSDR_FEATURE_DISABLED",
                message: "ferrited was built without the `soapysdr` feature; \
                          rebuild with `--features soapysdr`"
                    .to_string(),
            },
        }),
    )
}

pub async fn open_session(
    State(state): State<AppState>,
    Json(req): Json<OpenRequest>,
) -> Result<Json<OpenResponse>, (StatusCode, Json<ApiError>)> {
    let d = state.default_spec();
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
    let override_kind = source_override(&req).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: ApiErrorBody {
                    code: "FEATURE_DISABLED",
                    message: e,
                },
            }),
        )
    })?;
    let opened = state.open(spec, override_kind).await.map_err(|e| {
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

/// Translate the request's `device_args` (and friends) into a per-session
/// [`SourceKind`] override. Returns `Ok(None)` when the request keeps the
/// CLI default; returns `Err` when the user asked for a Soapy device but
/// the server was built without the feature.
//
// The `Result` is structurally needed by the feature-disabled stub but
// is never an error in this build; clippy correctly notices that and we
// suppress it here so the call site stays uniform across configs.
#[cfg(feature = "soapysdr")]
#[allow(clippy::unnecessary_wraps)]
fn source_override(req: &OpenRequest) -> Result<Option<SourceKind>, String> {
    let Some(args) = req.device_args.clone().filter(|s| !s.trim().is_empty()) else {
        return Ok(None);
    };
    Ok(Some(SourceKind::Soapy {
        args,
        antenna: req.antenna.clone(),
        gain_db: req.gain_db,
        agc: req.agc,
        bandwidth_hz: req.bandwidth_hz,
    }))
}

/// Feature-disabled stub: any non-empty `device_args` is rejected up front.
#[cfg(not(feature = "soapysdr"))]
fn source_override(req: &OpenRequest) -> Result<Option<SourceKind>, String> {
    if req
        .device_args
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty())
    {
        return Err("ferrited was built without the `soapysdr` feature; \
                    rebuild with `--features soapysdr` to open hardware devices"
            .into());
    }
    Ok(None)
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
