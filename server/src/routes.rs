//! HTTP and WebSocket route handlers.
//!
//! Ferrited is preset-first: the server holds exactly one flowgraph
//! preset, one [`SourceConfig`], and one (optional) running pipeline.
//! The routes below surface each of those as a small REST resource —
//! no session ids, no per-request pipeline spawning. `/ws/preset` is
//! the only WebSocket stream for sample data.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    response::IntoResponse,
    Json,
};
use ferrite_runtime::{FlowgraphDoc, SourceConfig};
use http::StatusCode;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::app_state::{AppState, PipelineBlock, PipelineStatus, UiSink};

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

#[derive(Serialize)]
pub struct ApiError {
    pub error: ApiErrorBody,
}

#[derive(Serialize)]
pub struct ApiErrorBody {
    pub code: &'static str,
    pub message: String,
}

fn bad_request(code: &'static str, message: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: ApiErrorBody {
                code,
                message: message.into(),
            },
        }),
    )
}

/// Streams every `tracing` log line as a text WS message.
pub async fn ws_logs(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    let Some(logs) = state.logs().cloned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "logs disabled").into_response();
    };
    ws.on_upgrade(move |socket| ws_logs_forward(socket, logs))
        .into_response()
}

async fn ws_logs_forward(mut socket: WebSocket, logs: crate::log_stream::LogBroadcast) {
    let mut rx = logs.subscribe();
    loop {
        match rx.recv().await {
            Ok(line) => {
                if socket.send(Message::Text(line)).await.is_err() {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                let _ = socket
                    .send(Message::Text(format!(
                        "[WARN] log stream lagged by {n} lines"
                    )))
                    .await;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
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

/// One entry in the `changes` array returned by a reconfigure. Mirrors
/// `ferrite_runtime::ParamChange` on the wire so a TS client can
/// deserialize it directly.
#[derive(Serialize)]
pub struct ParamChangeDto {
    pub block_id: String,
    pub param_key: String,
    pub old_value: serde_json::Value,
    pub new_value: serde_json::Value,
    pub scope: &'static str,
}

/// Reconfigure result returned by `PATCH /api/flowgraph` and
/// `PATCH /api/source`. `applied=false` means the patch was stored but
/// no pipeline was running, so there was nothing to reconfigure.
#[derive(Serialize)]
pub struct ReconfigureResponse {
    pub applied: bool,
    pub overall: Option<&'static str>,
    pub changes: Vec<ParamChangeDto>,
    pub structural_count: usize,
    pub noop: bool,
}

fn reconfigure_response(plan: Option<ferrite_runtime::ReconfigurePlan>) -> ReconfigureResponse {
    match plan {
        None => ReconfigureResponse {
            applied: false,
            overall: None,
            changes: Vec::new(),
            structural_count: 0,
            noop: true,
        },
        Some(p) => ReconfigureResponse {
            applied: true,
            overall: Some(p.overall.as_wire_str()),
            changes: p
                .changes
                .iter()
                .map(|c| ParamChangeDto {
                    block_id: c.block_id.clone(),
                    param_key: c.param_key.clone(),
                    old_value: c.old_value.clone(),
                    new_value: c.new_value.clone(),
                    scope: c.scope.as_wire_str(),
                })
                .collect(),
            structural_count: p.structural.len(),
            noop: p.is_noop(),
        },
    }
}

/// `GET /api/flowgraph` — snapshot the preset doc as JSON.
pub async fn get_flowgraph(State(state): State<AppState>) -> Json<FlowgraphDoc> {
    Json(state.get_flowgraph().await)
}

/// `PATCH /api/flowgraph` — store a new preset. Reconfigures the
/// running pipeline if there is one; otherwise the doc is queued for
/// the next `start`.
pub async fn patch_flowgraph(
    State(state): State<AppState>,
    Json(new_doc): Json<FlowgraphDoc>,
) -> Result<Json<ReconfigureResponse>, (StatusCode, Json<ApiError>)> {
    let plan = state
        .patch_flowgraph(new_doc)
        .await
        .map_err(|e| bad_request("RECONFIGURE_FAILED", format!("{e:#}")))?;
    Ok(Json(reconfigure_response(plan)))
}

/// `GET /api/source` — snapshot the current `SourceConfig`.
pub async fn get_source(State(state): State<AppState>) -> Json<SourceConfig> {
    Json(state.get_source().await)
}

/// `PATCH /api/source` — store a new source config. Same rules as
/// `PATCH /api/flowgraph`.
pub async fn patch_source(
    State(state): State<AppState>,
    Json(new_source): Json<SourceConfig>,
) -> Result<Json<ReconfigureResponse>, (StatusCode, Json<ApiError>)> {
    let plan = state
        .patch_source(new_source)
        .await
        .map_err(|e| bad_request("RECONFIGURE_FAILED", format!("{e:#}")))?;
    Ok(Json(reconfigure_response(plan)))
}

#[derive(Serialize)]
pub struct PipelineStatusResponse {
    pub status: PipelineStatus,
}

/// `GET /api/pipeline` — report whether the pipeline is running.
pub async fn pipeline_status(State(state): State<AppState>) -> Json<PipelineStatusResponse> {
    Json(PipelineStatusResponse {
        status: state.status().await,
    })
}

/// `POST /api/pipeline/start` — compose preset+source, spawn the
/// runtime. Idempotent: already-running returns 200.
pub async fn pipeline_start(
    State(state): State<AppState>,
) -> Result<Json<PipelineStatusResponse>, (StatusCode, Json<ApiError>)> {
    state
        .start()
        .await
        .map_err(|e| bad_request("PIPELINE_START_FAILED", format!("{e:#}")))?;
    Ok(Json(PipelineStatusResponse {
        status: PipelineStatus::Running,
    }))
}

/// `GET /api/ui-sinks` — enumerate every `ui:<name>` sink in the
/// composed preset and the `stream_id` env_split allocates for it. The
/// client uses this to subscribe to the right frame streams.
pub async fn list_ui_sinks(
    State(state): State<AppState>,
) -> Result<Json<Vec<UiSink>>, (StatusCode, Json<ApiError>)> {
    state
        .ui_sinks()
        .await
        .map(Json)
        .map_err(|e| bad_request("UI_SINKS_FAILED", format!("{e:#}")))
}

/// `POST /api/pipeline/stop` — tear the runtime down. Returns the
/// post-stop status (always `stopped`).
pub async fn pipeline_stop(State(state): State<AppState>) -> Json<PipelineStatusResponse> {
    state.stop().await;
    Json(PipelineStatusResponse {
        status: PipelineStatus::Stopped,
    })
}

/// `GET /api/blocks` — every registered block's capability schema as
/// a sorted array. Client uses this to render the flowgraph options
/// dialog without hard-coding field shapes per block type.
pub async fn list_block_schemas() -> Json<Vec<crate::block_schema::BlockSchemaDto>> {
    Json(crate::block_schema::all_block_schemas())
}

/// `GET /api/pipeline/blocks` — every block in the currently-loaded
/// composed preset, with its full spec and current param values.
/// Source for the generic `<BlockParams>` UI component. See D24 in
/// `docs/09-decisions.md`.
pub async fn list_pipeline_blocks(
    State(state): State<AppState>,
) -> Result<Json<Vec<PipelineBlock>>, (StatusCode, Json<ApiError>)> {
    state
        .list_blocks()
        .await
        .map(Json)
        .map_err(|e| bad_request("LIST_PIPELINE_BLOCKS_FAILED", format!("{e:#}")))
}

/// `POST /api/pipeline/blocks/{id}/params` — apply a params delta to
/// one block. The body is a JSON object of `{ param_key: new_value }`
/// pairs; keys not in the delta are left alone. Writes against the
/// `src` placeholder route to [`AppState::patch_source`]; everything
/// else routes to [`AppState::patch_flowgraph`]. The returned plan
/// shape matches `PATCH /api/flowgraph` and `PATCH /api/source` so
/// the UI can share reconfigure-handling code.
pub async fn patch_pipeline_block(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(delta): Json<serde_json::Value>,
) -> Result<Json<ReconfigureResponse>, (StatusCode, Json<ApiError>)> {
    let plan = state
        .apply_block_params(&id, delta)
        .await
        .map_err(|e| bad_request("RECONFIGURE_FAILED", format!("{e:#}")))?;
    Ok(Json(reconfigure_response(plan)))
}

/// `GET /ws/preset` — single WebSocket endpoint for preset sample
/// frames. Subscribes to the AppState's broadcast channel and
/// forwards every frame as a binary message. Survives pipeline
/// start/stop cycles — a subscriber connected while stopped picks up
/// frames the moment the pipeline spins up.
pub async fn ws_preset(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_preset(socket, state))
}

async fn handle_preset(mut socket: WebSocket, state: AppState) {
    let mut rx = state.subscribe();
    tracing::debug!("ws preset subscribed");
    loop {
        tokio::select! {
            client = socket.recv() => {
                match client {
                    None | Some(Ok(Message::Close(_))) => return,
                    Some(Err(err)) => {
                        tracing::debug!(?err, "ws preset recv");
                        return;
                    }
                    Some(Ok(Message::Ping(p))) => {
                        if socket.send(Message::Pong(p)).await.is_err() { return; }
                    }
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
                    tracing::warn!(skipped = n, "ws preset subscriber lagged");
                }
            }
        }
    }
}
