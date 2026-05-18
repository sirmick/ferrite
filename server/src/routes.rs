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
        Path, Query, State,
    },
    response::IntoResponse,
    Json,
};
use ferrite_runtime::{FlowgraphDoc, Profile, SourceConfig};
use futures_util::{SinkExt, StreamExt};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::protocol::Message as TungMessage;

use crate::app_state::{
    AppState, CaptureEntry, PipelineBlock, PipelineStatus, PresetEntry, UiSink,
};

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
    // An idle log stream (no events for a while) used to sit silent
    // until the Vite dev proxy / browser killed it on its ~60 s idle
    // timeout, and the client reconnected — logging "server logs
    // connected" every minute. A periodic ping keeps the connection
    // hot so a quiet runtime doesn't churn the socket.
    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(25));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
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
            },
            _ = keepalive.tick() => {
                if socket.send(Message::Ping(Vec::new())).await.is_err() {
                    return;
                }
            }
            // Drain client-side frames so a Close (or the read half
            // erroring) tears the task down promptly instead of only
            // being noticed on the next send.
            client = socket.recv() => match client {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            },
        }
    }
}

/// Default location of the ferrite-ai sidecar — TCP on localhost. The
/// sidecar holds an Anthropic Claude Agent SDK session per browser
/// connection; ferrited reverse-proxies its WS so the browser only
/// ever talks to a single port. Override with the `FERRITE_AI_URL`
/// env var (e.g. point at a unix-socket-fronting proxy or a different
/// machine on a trusted LAN).
const DEFAULT_FERRITE_AI_URL: &str = "ws://127.0.0.1:10002/ws/chat";

/// `GET /ws/chat` — reverse-proxy to the ferrite-ai sidecar. The
/// browser opens this WS to ferrited; we open a second WS upstream to
/// the sidecar and pump frames bidirectionally until either side
/// closes. If the sidecar isn't reachable, we surface a structured
/// error event the UI's chat panel renders, then close the client
/// socket so the front end can show "ferrite-ai offline" cleanly.
pub async fn ws_chat(ws: WebSocketUpgrade) -> impl IntoResponse {
    let upstream_url =
        std::env::var("FERRITE_AI_URL").unwrap_or_else(|_| DEFAULT_FERRITE_AI_URL.to_string());
    ws.on_upgrade(move |socket| ws_chat_proxy(socket, upstream_url))
}

async fn ws_chat_proxy(mut client_ws: WebSocket, upstream_url: String) {
    let upstream = match tokio_tungstenite::connect_async(&upstream_url).await {
        Ok((s, _resp)) => s,
        Err(e) => {
            // Mirror the sidecar's `ferrite_ai_error` envelope shape so
            // the UI's chat store renders this in the same red-banner
            // path it'd use for an upstream-side error.
            let body = serde_json::json!({
                "type": "ferrite_ai_error",
                "message": format!("ferrite-ai unreachable at {upstream_url}: {e}"),
            });
            let _ = client_ws.send(Message::Text(body.to_string())).await;
            let _ = client_ws.close().await;
            return;
        }
    };

    let (mut up_tx, mut up_rx) = upstream.split();
    let (mut cli_tx, mut cli_rx) = client_ws.split();

    // Browser → sidecar: text/binary forward, close on Close. We don't
    // attempt to translate ping/pong — both sides exchange them via
    // their own keepalives.
    let c2u = async move {
        while let Some(msg) = cli_rx.next().await {
            let Ok(msg) = msg else { break };
            let upstream_msg = match msg {
                Message::Text(t) => TungMessage::Text(t),
                Message::Binary(b) => TungMessage::Binary(b),
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => continue,
            };
            if up_tx.send(upstream_msg).await.is_err() {
                break;
            }
        }
        // Politely close the upstream when the browser hangs up so the
        // sidecar's per-connection session state can be freed.
        let _ = up_tx.close().await;
    };

    // Sidecar → browser: same shape in reverse.
    let u2c = async move {
        while let Some(msg) = up_rx.next().await {
            let Ok(msg) = msg else { break };
            let client_msg = match msg {
                TungMessage::Text(t) => Message::Text(t),
                TungMessage::Binary(b) => Message::Binary(b),
                TungMessage::Close(_) => break,
                TungMessage::Ping(_) | TungMessage::Pong(_) | TungMessage::Frame(_) => continue,
            };
            if cli_tx.send(client_msg).await.is_err() {
                break;
            }
        }
        let _ = cli_tx.close().await;
    };

    // First side to finish closes the bridge — `select!` cancels the
    // other branch when one resolves. Avoids leaking a pump task on
    // an asymmetric disconnect.
    tokio::select! {
        _ = c2u => {},
        _ = u2c => {},
    }
}

/// `GET /api/decoder/recent?since=<unix_ms>&category=<prefix>` — pull
/// every log line matching `target.starts_with(category)` (e.g.
/// `decoder::pocsag`, or just `decoder` for everything decoder-side)
/// emitted *strictly after* `since`. Returns a flat array of
/// structured `LogEntry { target, level, message, at_ms }`.
///
/// Polling pattern for ferrite-ctl / AI agents: pass the largest
/// `at_ms` seen on the previous poll as the next `since`. The
/// underlying ring is capped at ~4096 entries; a polling interval
/// faster than the carrier emits is the right shape (1–2 s).
#[derive(Deserialize)]
pub struct RecentQuery {
    pub since: Option<u64>,
    pub category: Option<String>,
}

#[derive(Serialize)]
pub struct RecentResponse {
    pub entries: Vec<crate::log_stream::LogEntry>,
    /// Server's current `Date.now()`-shaped timestamp, so a polling
    /// client can use it as the next `since` watermark when the buffer
    /// returned no entries (otherwise their watermark would never
    /// advance through quiet stretches).
    pub now: u64,
}

pub async fn recent_decoder(
    State(state): State<AppState>,
    Query(q): Query<RecentQuery>,
) -> Result<Json<RecentResponse>, (StatusCode, Json<ApiError>)> {
    let logs = state.logs().ok_or_else(|| {
        bad_request(
            "LOGS_DISABLED",
            "log broadcast not configured on this server",
        )
    })?;
    let since = q.since.unwrap_or(0);
    let entries = logs.recent(since, q.category.as_deref());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    Ok(Json(RecentResponse { entries, now }))
}

// Make `LogEntry` serialize-friendly. Defined here rather than in
// log_stream.rs so log_stream stays serde-free; the API layer is the
// right place to gate JSON shape.
mod log_entry_serde {
    use super::*;
    impl Serialize for crate::log_stream::LogEntry {
        fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
            use serde::ser::SerializeStruct;
            let mut st = s.serialize_struct("LogEntry", 4)?;
            st.serialize_field("target", &self.target)?;
            st.serialize_field("level", &self.level)?;
            st.serialize_field("message", &self.message)?;
            st.serialize_field("at_ms", &self.at_ms)?;
            st.end()
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
    let is_src = id == ferrite_runtime::SOURCE_ID;
    let plan = state
        .apply_block_params(&id, delta)
        .await
        .map_err(|e| bad_request("RECONFIGURE_FAILED", format!("{e:#}")))?;
    let mut resp = reconfigure_response(plan);
    // Source goes through `patch_source` internally; mirror the
    // `/api/source` handler so a single-key `src` write still gets the
    // driver readback for optimistic-state reconciliation.
    if is_src {
        resp.source_readback = state.source_readback().await;
    }
    Ok(Json(resp))
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

/// `GET /api/captures` — enumerate replayable IQ/audio fixtures under
/// the `samples/` tree (with `*.json` sidecar metadata) so the File
/// source tab can list them instead of asking for a server path.
pub async fn list_captures(
    State(state): State<AppState>,
) -> Result<Json<Vec<CaptureEntry>>, (StatusCode, Json<ApiError>)> {
    state
        .list_captures()
        .await
        .map(Json)
        .map_err(|e| bad_request("LIST_CAPTURES_FAILED", format!("{e:#}")))
}

#[derive(serde::Deserialize)]
pub struct LoadPresetRequest {
    pub name: String,
    /// Optional profile to apply atomically with the preset load. When
    /// present, [`AppState::set_profile`] runs before the doc is
    /// composed so the `apply_profile` pass sees the new value on the
    /// very first compose. Useful for the UI's "swap preset and
    /// disable audio in one click" flow.
    #[serde(default)]
    pub profile: Option<Profile>,
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
    if let Some(profile) = req.profile {
        state.set_profile(profile).await;
    }
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

/// `GET /api/profile` — snapshot the active runtime [`Profile`]. Lets
/// the UI hydrate its chip row from server state without guessing.
pub async fn get_profile(State(state): State<AppState>) -> Json<Profile> {
    Json(state.get_profile().await)
}

/// `PATCH /api/profile` — full replacement of the active runtime
/// profile, then re-compose the active preset so the new value takes
/// effect on the running pipeline. Body is a `Profile` JSON object;
/// missing fields fall back to type defaults (see `serde(default)` on
/// `Profile`). Returns the new profile so the UI can confirm.
pub async fn patch_profile(
    State(state): State<AppState>,
    Json(profile): Json<Profile>,
) -> Result<Json<Profile>, (StatusCode, Json<ApiError>)> {
    // Transactional swap: stash the previous profile, apply the new
    // one, then re-compose. If the compose fails (e.g. flipping
    // `demod_placement` produces an unsupported browser→node wire),
    // roll the profile back so subsequent preset loads don't carry
    // forward the bad state.
    let prev = state.get_profile().await;
    state.set_profile(profile.clone()).await;
    let current = state.get_flowgraph().await;
    if let Err(e) = state.patch_flowgraph(current).await {
        state.set_profile(prev).await;
        return Err(bad_request("PROFILE_APPLY_FAILED", format!("{e:#}")));
    }
    Ok(Json(profile))
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
    } else if entry.message.starts_with("[transcribe]") {
        // In-browser STT is the one decoder that runs browser-side, so
        // its output never reached the node decoder log the embedded
        // AI / `ferrite-ctl decoder recent` reads. File it under a
        // literal `decoder::transcribe` target (same branch-on-content
        // trick as flowdiag — `tracing` needs a literal `target:`) so
        // it's first-class observable without a screenshot/DOM.
        match entry.level.as_str() {
            "error" => {
                tracing::error!(target: "decoder::transcribe", source = src, "{}", entry.message)
            }
            "warn" => {
                tracing::warn!(target: "decoder::transcribe", source = src, "{}", entry.message)
            }
            "debug" => {
                tracing::debug!(target: "decoder::transcribe", source = src, "{}", entry.message)
            }
            _ => tracing::info!(target: "decoder::transcribe", source = src, "{}", entry.message),
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

// ─── UI-view bridge ────────────────────────────────────────────────
//
// `GET /api/ui-views/:pane`: ferrited asks the connected browser tab
// for a PNG snapshot of one of the four canvases (wide spectrum, wide
// waterfall, channel spectrum, channel waterfall) and streams it back
// as `image/png`. `/ws/ui-views`: the browser side of that
// conversation. See `view_bridge.rs` for the broker.
//
// The AI reaches this via `ferrite-ctl view <pane>`, which is just a
// thin GET that writes the response body to a temp file and prints
// the path. The AI then `Read`s the PNG as an image content block.

/// Allowed pane names. Validated server-side so a typo from the AI
/// surfaces as a 400 instead of an opaque browser-side miss.
const KNOWN_PANES: &[&str] = &[
    "wide-spectrum",
    "wide-waterfall",
    "channel-spectrum",
    "channel-waterfall",
];

pub async fn get_ui_view(
    State(state): State<AppState>,
    Path(pane): Path<String>,
) -> impl IntoResponse {
    if !KNOWN_PANES.contains(&pane.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            format!("unknown pane {pane:?}; expected one of {KNOWN_PANES:?}"),
        )
            .into_response();
    }
    match state.view_bridge().request(&pane).await {
        Ok(bytes) => (
            StatusCode::OK,
            [
                (axum::http::header::CONTENT_TYPE, "image/png"),
                (axum::http::header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(e @ crate::view_bridge::ViewError::NoViewer) => {
            (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response()
        }
        Err(e @ crate::view_bridge::ViewError::Timeout) => {
            (StatusCode::GATEWAY_TIMEOUT, e.to_string()).into_response()
        }
    }
}

pub async fn ws_ui_views(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| ws_ui_views_loop(socket, state))
}

async fn ws_ui_views_loop(mut socket: WebSocket, state: AppState) {
    // Install ourselves as the active viewer. Last-connect-wins —
    // matches D06. The returned `our_sender` is our identity token
    // for `detach_viewer_if_current` so we don't trample a newer
    // subscriber's installation on disconnect.
    let bridge = state.view_bridge().clone();
    let (our_sender, mut req_rx) = bridge.install_viewer().await;

    loop {
        tokio::select! {
            // ferrited → browser: a new snapshot request from the HTTP
            // side. Serialise as JSON and send over the WS.
            req = req_rx.recv() => {
                let Some(req) = req else { break };
                let wire = crate::view_bridge::WireRequest {
                    kind: "view_request",
                    req_id: req.req_id,
                    pane: &req.pane,
                };
                let Ok(json) = serde_json::to_string(&wire) else { continue };
                if socket.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
            // browser → ferrited: a view_response landing the PNG for a
            // prior request.
            msg = socket.recv() => {
                let Some(msg) = msg else { break };
                let Ok(msg) = msg else { break };
                match msg {
                    Message::Text(t) => {
                        if let Ok(resp) =
                            serde_json::from_str::<crate::view_bridge::WireResponse>(&t)
                        {
                            bridge.complete(resp).await;
                        }
                    }
                    Message::Close(_) => break,
                    Message::Ping(p) => match socket.send(Message::Pong(p)).await {
                        Ok(()) => {}
                        Err(_) => break,
                    },
                    _ => {}
                }
            }
        }
    }

    bridge.detach_viewer_if_current(&our_sender).await;
}
