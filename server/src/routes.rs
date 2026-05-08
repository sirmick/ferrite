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
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::app_state::{AppState, PipelineBlock, PipelineStatus, PresetEntry, UiSink};

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
/// one for its full capability schema. Probes go through the per-process
/// [`DeviceCache`](crate::device_cache::DeviceCache); only the very
/// first hit per device touches the driver, and cache entries for
/// devices no longer enumerated are pruned.
pub async fn list_devices(
    State(state): State<AppState>,
) -> Result<Json<Vec<DeviceEntry>>, (StatusCode, Json<ApiError>)> {
    let devices = tokio::task::spawn_blocking(|| {
        crate::device::list_devices_with_timeout(crate::device::DEFAULT_PROBE_TIMEOUT)
    })
    .await
    .map_err(|e| internal(format!("device enumerate task panicked: {e}")))?
    .map_err(|e| internal(format!("{e:#}")))?;

    let cache = state.device_cache();
    let mut present = std::collections::HashSet::with_capacity(devices.len());
    let mut entries = Vec::with_capacity(devices.len());
    for info in devices {
        present.insert(crate::device_cache::stable_device_key(&info));
        match cache.ensure(&info).await {
            Ok(caps) => entries.push(DeviceEntry::Available(Box::new(caps))),
            Err(err) => entries.push(DeviceEntry::Unavailable {
                info,
                error: format!("{err:#}"),
            }),
        }
    }
    cache.prune(&present).await;
    Ok(Json(entries))
}

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
    /// Post-apply snapshot of the driver state for the `src` block.
    /// Present on `PATCH /api/source` against a running `SoapySource`
    /// so the UI can reconcile its optimistic `params` with what the
    /// hardware actually accepted (drivers silently clamp BW, AGC
    /// varies IFGR, tune-step rounds the centre freq, etc).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_readback: Option<ferrite_blocks::SoapyReadback>,
}

fn reconfigure_response(plan: Option<ferrite_runtime::ReconfigurePlan>) -> ReconfigureResponse {
    match plan {
        None => ReconfigureResponse {
            applied: false,
            overall: None,
            changes: Vec::new(),
            structural_count: 0,
            noop: true,
            source_readback: None,
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
            source_readback: None,
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
/// `PATCH /api/flowgraph`. The response carries a `source_readback` when
/// the running source is a `SoapySource`, so the UI can reconcile its
/// optimistic `params` with what the driver actually accepted.
pub async fn patch_source(
    State(state): State<AppState>,
    Json(new_source): Json<SourceConfig>,
) -> Result<Json<ReconfigureResponse>, (StatusCode, Json<ApiError>)> {
    tracing::info!(type_name = %new_source.type_name, params = ?new_source.params, "PATCH /api/source");
    let plan = state
        .patch_source(new_source)
        .await
        .map_err(|e| bad_request("RECONFIGURE_FAILED", format!("{e:#}")))?;
    let mut resp = reconfigure_response(plan);
    resp.source_readback = state.source_readback().await;
    Ok(Json(resp))
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
    tracing::info!("POST /api/pipeline/start");
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

/// `GET /api/source/capabilities` — probe the currently-active source
/// and return whatever the hardware exposes (discrete sample rates,
/// gain elements, antennas, frequency ranges). Software sources return
/// `{kind: "software"}` with no capability blob so the UI can hide the
/// hardware-only controls (sample-rate dropdown, antenna picker, gain).
///
/// This sits alongside `GET /api/devices` — that enumerates every
/// attached device, this asks "what can the source I'm actually using
/// do right now?". Avoids the UI re-probing on every SourceConfig
/// change and keeps args-string parsing on the server.
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SourceCapabilitiesResponse {
    Hardware {
        type_name: String,
        capabilities: Box<crate::device::DeviceCapabilities>,
    },
    Software {
        type_name: String,
    },
    Unavailable {
        type_name: String,
        error: String,
    },
}

pub async fn get_source_capabilities(
    State(state): State<AppState>,
) -> Result<Json<SourceCapabilitiesResponse>, (StatusCode, Json<ApiError>)> {
    let source = state.get_source().await;
    let type_name = source.type_name.clone();
    if type_name != "SoapySource" {
        return Ok(Json(SourceCapabilitiesResponse::Software { type_name }));
    }
    let args = source
        .params
        .get("args")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let response = match state.device_cache().ensure_args(&args).await {
        Ok(caps) => SourceCapabilitiesResponse::Hardware {
            type_name,
            capabilities: Box::new(caps),
        },
        Err(err) => SourceCapabilitiesResponse::Unavailable {
            type_name,
            error: format!("{err:#}"),
        },
    };
    Ok(Json(response))
}

/// `GET /api/presets` — enumerate every `*.json` preset the server can
/// load. Returns an empty list when no presets dir is configured.
pub async fn list_presets(
    State(state): State<AppState>,
) -> Result<Json<Vec<PresetEntry>>, (StatusCode, Json<ApiError>)> {
    state
        .list_presets()
        .await
        .map(Json)
        .map_err(|e| bad_request("LIST_PRESETS_FAILED", format!("{e:#}")))
}

#[derive(serde::Deserialize)]
pub struct LoadPresetRequest {
    pub name: String,
}

#[derive(Serialize)]
pub struct LoadPresetResponse {
    pub name: String,
    pub reconfigure: ReconfigureResponse,
}

/// `POST /api/preset` — swap the active preset by basename. Body:
/// `{"name": "wbfm"}`. Rejects names that aren't plain `[A-Za-z0-9_-]+`
/// so the lookup cannot escape the presets dir. Running pipelines are
/// hot-reconfigured; stopped ones just store the doc for next `start`.
pub async fn load_preset(
    State(state): State<AppState>,
    Json(req): Json<LoadPresetRequest>,
) -> Result<Json<LoadPresetResponse>, (StatusCode, Json<ApiError>)> {
    let started = std::time::Instant::now();
    tracing::info!(name = %req.name, "POST /api/preset");
    let (doc, plan) = state
        .load_preset_by_name(&req.name)
        .await
        .map_err(|e| bad_request("LOAD_PRESET_FAILED", format!("{e:#}")))?;
    let dt_ms = started.elapsed().as_millis();
    let scope = plan.as_ref().map_or("no-pipeline", |p| match p.overall {
        ferrite_blocks::ReconfigureScope::SelfBlock => "self-block",
        ferrite_blocks::ReconfigureScope::Downstream => "downstream",
        ferrite_blocks::ReconfigureScope::SourceRestart => "source-restart",
    });
    let structural = plan.as_ref().map_or(0, |p| p.structural.len());
    if dt_ms > 500 {
        tracing::warn!(
            name = %req.name, elapsed_ms = dt_ms as u64, scope, structural,
            "[slow] POST /api/preset"
        );
    } else {
        tracing::info!(
            name = %req.name, elapsed_ms = dt_ms as u64, scope, structural,
            "POST /api/preset ok"
        );
    }
    Ok(Json(LoadPresetResponse {
        name: doc.name,
        reconfigure: reconfigure_response(plan),
    }))
}

/// `GET /ws/preset` — single WebSocket endpoint for preset sample
/// frames. Subscribes to the AppState's shared [`FrameBus`] and
/// forwards every frame as a binary message. Survives pipeline
/// start/stop cycles — a subscriber connected while stopped picks up
/// frames the moment the pipeline spins up.
///
/// Per-subscriber backpressure: each WS connection gets its own
/// 1024-frame bounded queue. A slow consumer loses only its own
/// frames (surfaces as a `seq` gap) and cannot stall the scheduler
/// or starve other subscribers.
///
/// [`FrameBus`]: crate::frame_bus::FrameBus
pub async fn ws_preset(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_preset(socket, state))
}

/// Payload for `POST /api/debug/log` — a single log entry forwarded
/// from the browser so it shows up in the ferrited log stream
/// alongside server-origin messages. `source` tags the origin (e.g.
/// `"console"`, `"client"`, `"pipeline"`) so the server log stays
/// filterable; `level` is `error | warn | info | debug`.
#[derive(Deserialize)]
pub struct BrowserLogEntry {
    pub level: String,
    pub message: String,
    #[serde(default)]
    pub source: Option<String>,
}

/// `POST /api/debug/log` — browser → server log forwarder. Dev-only
/// diagnostic: the UI's `logs.push('client', ...)` calls and any
/// hooked `console.*` calls fan out through here so the operator
/// watching ferrited stdout gets the full story without opening
/// DevTools. Fire-and-forget on the client side.
pub async fn browser_log(Json(entry): Json<BrowserLogEntry>) -> StatusCode {
    let src = entry.source.as_deref().unwrap_or("browser");
    // Browser-side flowdiag rides through here too — route it under
    // the same `flowdiag` target the node-side runtime emits with, so
    // `RUST_LOG=flowdiag=info` continues to isolate every flow snapshot
    // regardless of which runtime side produced it. `tracing` requires
    // `target:` to be a string literal, so we branch on the route
    // rather than computing a runtime string.
    // Browser-side flowdiag is now tagged with the `flowdiag::browser`
    // category prefix at source. It still carries the legacy
    // `flowdiag side=browser` substring so the receiving regex on
    // either side keeps parsing it; the `contains` check finds either
    // shape so we route correctly even if the prefix gets stripped.
    let is_flowdiag = entry.message.contains("flowdiag side=");
    if is_flowdiag {
        match entry.level.as_str() {
            "error" => {
                tracing::error!(target: "flowdiag::browser", source = src, "{}", entry.message)
            }
            "warn" => {
                tracing::warn!(target: "flowdiag::browser", source = src, "{}", entry.message)
            }
            "debug" => {
                tracing::debug!(target: "flowdiag::browser", source = src, "{}", entry.message)
            }
            _ => tracing::info!(target: "flowdiag::browser", source = src, "{}", entry.message),
        }
    } else {
        match entry.level.as_str() {
            "error" => tracing::error!(target: "browser", source = src, "{}", entry.message),
            "warn" => tracing::warn!(target: "browser", source = src, "{}", entry.message),
            "debug" => tracing::debug!(target: "browser", source = src, "{}", entry.message),
            _ => tracing::info!(target: "browser", source = src, "{}", entry.message),
        }
    }
    StatusCode::NO_CONTENT
}

async fn handle_preset(mut socket: WebSocket, state: AppState) {
    let mut rx = state.subscribe();
    tracing::debug!("ws preset subscribed");
    loop {
        tokio::select! {
            client = socket.recv() => {
                // collapsible_match wants the inner if turned into a match
                // guard with an `await` side-effect — uglier than the if.
                #[allow(clippy::collapsible_match)]
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
                Some(bytes) => {
                    if socket.send(Message::Binary((*bytes).clone())).await.is_err() {
                        return;
                    }
                }
                None => return,
            }
        }
    }
}
