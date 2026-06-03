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
    api_error(StatusCode::BAD_REQUEST, code, message)
}

fn api_error(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
) -> (StatusCode, Json<ApiError>) {
    (
        status,
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

/// `GET /ws/state` — the store mirror feed. On connect sends a full
/// snapshot (`{"snapshot": {seq, kinds}}`), then streams each change as
/// `{"delta": {kind, seq, op}}`. The browser keeps a 1:1 mirror and
/// de-dupes by `seq` (skips deltas ≤ the snapshot's seq). REST
/// `/api/state` is the same data for MCP/headless.
pub async fn ws_state(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    let store = state.decoder_store();
    ws.on_upgrade(move |socket| ws_state_forward(socket, store))
}

async fn ws_state_forward(
    mut socket: WebSocket,
    store: std::sync::Arc<crate::decoder_store::DecoderStore>,
) {
    // Subscribe *before* snapshotting so no delta is lost in the gap; the
    // client de-dupes any overlap by `seq`.
    let mut rx = store.subscribe();
    let snapshot = serde_json::json!({ "snapshot": store.snapshot() });
    if socket
        .send(Message::Text(snapshot.to_string()))
        .await
        .is_err()
    {
        return;
    }

    let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(25));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(delta) => {
                    let msg = serde_json::json!({ "delta": delta }).to_string();
                    if socket.send(Message::Text(msg)).await.is_err() {
                        return;
                    }
                }
                // On lag, tell the client to re-fetch the snapshot via REST
                // rather than try to reconcile a gap.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let _ = socket
                        .send(Message::Text(r#"{"resync":true}"#.to_string()))
                        .await;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
            _ = keepalive.tick() => {
                if socket.send(Message::Ping(Vec::new())).await.is_err() {
                    return;
                }
            }
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

/// Process-wide fan-out for externally-injected chat events. The
/// headless ops layer (`ferrite-ctl chat-post` / the `chat_post` MCP
/// tool) POSTs to `/api/ai/chat-inject`; every live `/ws/chat` browser
/// proxy subscribes here and forwards the frames to its socket. That
/// lets an agent driving ferrited from *outside* the browser mirror its
/// conversation into the UI's AI panel. One ferrited process → a single
/// global channel is the whole story; nothing per-connection lives here.
fn chat_inject_hub() -> &'static broadcast::Sender<String> {
    static HUB: std::sync::OnceLock<broadcast::Sender<String>> = std::sync::OnceLock::new();
    HUB.get_or_init(|| broadcast::channel::<String>(256).0)
}

/// `GET /ws/chat` — reverse-proxy to the ferrite-ai sidecar. The
/// browser opens this WS to ferrited; we open a second WS upstream to
/// the sidecar and pump frames bidirectionally until either side
/// closes. If the sidecar isn't reachable, we surface a structured
/// error event the UI's chat panel renders, then close the client
/// socket so the front end can show "ferrite-ai offline" cleanly.
pub async fn ws_chat(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    let upstream_url =
        std::env::var("FERRITE_AI_URL").unwrap_or_else(|_| DEFAULT_FERRITE_AI_URL.to_string());
    let store = state.decoder_store();
    ws.on_upgrade(move |socket| ws_chat_proxy(socket, upstream_url, store))
}

async fn ws_chat_proxy(
    mut client_ws: WebSocket,
    upstream_url: String,
    store: std::sync::Arc<crate::decoder_store::DecoderStore>,
) {
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
    let store_c2u = std::sync::Arc::clone(&store);
    let c2u = async move {
        while let Some(msg) = cli_rx.next().await {
            let Ok(msg) = msg else { break };
            let upstream_msg = match msg {
                Message::Text(t) => {
                    // Record the operator's prompt in the `ai` transcript on
                    // its way upstream (the sidecar never echoes it back).
                    fold_ai_user_frame(&store_c2u, t.as_str());
                    TungMessage::Text(t)
                }
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

    // Sidecar → browser, plus externally-injected chat frames
    // (`POST /api/ai/chat-inject`). Both write to the same client
    // socket, so we select over them: a headless driver's mirrored
    // turns interleave with the (usually idle) sidecar stream.
    let mut inject_rx = chat_inject_hub().subscribe();
    let store_u2c = std::sync::Arc::clone(&store);
    let u2c = async move {
        loop {
            tokio::select! {
                up = up_rx.next() => {
                    let Some(Ok(msg)) = up else { break };
                    let client_msg = match msg {
                        TungMessage::Text(t) => {
                            // Record the assistant's reply text from the
                            // sidecar's full end-of-turn `assistant` message
                            // (the live cursor still streams from deltas in
                            // the browser; this is the turn-granular log).
                            // Injected frames are folded in `post_chat_inject`,
                            // not here, so they aren't double-counted.
                            fold_ai_assistant_frame(&store_u2c, t.as_str());
                            Message::Text(t)
                        }
                        TungMessage::Binary(b) => Message::Binary(b),
                        TungMessage::Close(_) => break,
                        TungMessage::Ping(_) | TungMessage::Pong(_) | TungMessage::Frame(_) => {
                            continue
                        }
                    };
                    if cli_tx.send(client_msg).await.is_err() {
                        break;
                    }
                }
                inj = inject_rx.recv() => {
                    match inj {
                        Ok(text) => {
                            if cli_tx.send(Message::Text(text)).await.is_err() {
                                break;
                            }
                        }
                        // A slow tab missed some injects — keep serving.
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        // The sender is a 'static OnceLock, so Closed
                        // shouldn't happen; ignore defensively.
                        Err(broadcast::error::RecvError::Closed) => continue,
                    }
                }
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

/// `POST /api/ai/chat-inject` — fan an externally-authored chat event
/// (or a batch) out to every connected `/ws/chat` browser. Body is a
/// single event object, an array of events, or `{ "events": [ … ] }`;
/// each is broadcast verbatim, so the event vocabulary
/// (`ferrite_ai_user_turn`, `stream_event`, `ferrite_ai_done`, …) lives
/// in the ops layer that builds them, not here. Returns how many live
/// browser subscribers the frames reached. Deliberately *not* tagged
/// for `ai_activity_layer` (the ops layer omits the command header) so
/// mirroring a conversation doesn't also flood the activity transcript.
///
/// Injected turns are also folded into the `ai` decoder-store kind here
/// (not in the per-connection `/ws/chat` proxy), so a headless driver's
/// conversation is recorded even with no browser tab open — and so the
/// proxy's inject branch can't double-count what this already logged.
pub async fn post_chat_inject(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    let events: Vec<serde_json::Value> = match body {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(o) if o.contains_key("events") => o
            .get("events")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default(),
        other => vec![other],
    };
    fold_ai_inject_batch(&state.decoder_store(), &events);
    let hub = chat_inject_hub();
    let subscribers = hub.receiver_count();
    let mut sent = 0usize;
    for ev in &events {
        if hub.send(ev.to_string()).is_ok() {
            sent += 1;
        }
    }
    Json(serde_json::json!({ "subscribers": subscribers, "events": sent }))
}

/// Fold a browser→sidecar frame into the `ai` transcript when it carries a
/// user prompt. A send payload is `{ text, mode, … }` with no `type`;
/// control envelopes (`reset_session` / `stop` / `request_snapshot`) carry
/// a `type` field and are skipped.
fn fold_ai_user_frame(store: &crate::decoder_store::DecoderStore, frame: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(frame) else {
        return;
    };
    if v.get("type").is_some() {
        return; // control envelope, not a prompt
    }
    let Some(text) = v.get("text").and_then(serde_json::Value::as_str) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    let mut rec = serde_json::json!({ "role": "user", "text": text });
    if let Some(mode) = v.get("mode").and_then(serde_json::Value::as_str) {
        rec["mode"] = serde_json::json!(mode);
    }
    store.apply("ai", rec);
}

/// Fold a sidecar→browser frame into the `ai` transcript when it's a full
/// assistant message carrying text (emitted at end-of-turn and per
/// tool-loop step). The browser accumulates the same text from
/// `stream_event` deltas for the live cursor; this records it
/// turn-granular for MCP read-back.
fn fold_ai_assistant_frame(store: &crate::decoder_store::DecoderStore, frame: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(frame) else {
        return;
    };
    if v.get("type").and_then(serde_json::Value::as_str) != Some("assistant") {
        return;
    }
    let text = v
        .get("message")
        .and_then(|m| m.get("content"))
        .map(ai_text_from_content)
        .unwrap_or_default();
    if text.is_empty() {
        return;
    }
    store.apply(
        "ai",
        serde_json::json!({ "role": "assistant", "text": text }),
    );
}

/// Join the `text` of every text block in an Agent SDK message `content`
/// (an array of typed blocks, or a bare string). tool_use / tool_result /
/// image blocks are skipped.
fn ai_text_from_content(content: &serde_json::Value) -> String {
    let Some(arr) = content.as_array() else {
        return content.as_str().unwrap_or_default().to_string();
    };
    let mut out = String::new();
    for block in arr {
        if block.get("type").and_then(serde_json::Value::as_str) == Some("text") {
            if let Some(t) = block.get("text").and_then(serde_json::Value::as_str) {
                out.push_str(t);
            }
        }
    }
    out
}

/// Fold a chat-inject batch into the `ai` transcript. Mirrors the
/// `chat_post` event vocabulary: a `ferrite_ai_user_turn` becomes a user
/// record, and the `text_delta`s in an assistant batch coalesce into one
/// assistant record. `chat_post` POSTs one self-contained role-turn per
/// call, so reducing within the batch is sufficient. tool_use
/// (`input_json_delta`) frames are a different delta type and are ignored.
fn fold_ai_inject_batch(
    store: &crate::decoder_store::DecoderStore,
    events: &[serde_json::Value],
) {
    let mut assistant = String::new();
    for ev in events {
        match ev.get("type").and_then(serde_json::Value::as_str) {
            Some("ferrite_ai_user_turn") => {
                if let Some(text) = ev.get("text").and_then(serde_json::Value::as_str) {
                    if !text.is_empty() {
                        store.apply("ai", serde_json::json!({ "role": "user", "text": text }));
                    }
                }
            }
            Some("stream_event") => {
                let delta = ev.get("event").and_then(|e| e.get("delta"));
                let is_text =
                    delta.and_then(|d| d.get("type")).and_then(serde_json::Value::as_str)
                        == Some("text_delta");
                if is_text {
                    if let Some(t) =
                        delta.and_then(|d| d.get("text")).and_then(serde_json::Value::as_str)
                    {
                        assistant.push_str(t);
                    }
                }
            }
            _ => {}
        }
    }
    if !assistant.is_empty() {
        store.apply(
            "ai",
            serde_json::json!({ "role": "assistant", "text": assistant }),
        );
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
    /// Return only entries strictly after this unix-ms watermark. The
    /// precise "what's new since I last polled" cursor — pass back the
    /// previous response's `now` (or the largest `at_ms` seen).
    pub since: Option<u64>,
    /// Convenience relative window: only entries from the last N seconds.
    /// Resolved against the server clock into a `since` watermark, so it
    /// composes with (and is overridden by an explicit) `since`. Default
    /// behaviour with neither set is "all retained" (since = 0).
    pub lookback: Option<f64>,
    pub category: Option<String>,
    /// Keep only the newest `limit` entries (0 / unset = no cap). Applied
    /// after filtering, so `tail`-style "just the last few" is cheap.
    pub limit: Option<usize>,
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    // Resolve the watermark: explicit `since` wins; else `lookback`
    // seconds back from now; else everything retained.
    let since = q.since.unwrap_or_else(|| match q.lookback {
        Some(secs) if secs > 0.0 => now.saturating_sub((secs * 1000.0) as u64),
        _ => 0,
    });
    let mut entries = logs.recent(since, q.category.as_deref());
    // `tail`: keep the newest `limit`. `recent` returns oldest→newest,
    // so trim from the front.
    if let Some(limit) = q.limit {
        if limit > 0 && entries.len() > limit {
            entries.drain(..entries.len() - limit);
        }
    }
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

/// `POST /api/devices/reload` — tear down + re-load every SoapySDR
/// driver module to recover from an in-process driver wedge. Returns
/// `409 Conflict` when the pipeline is running (a live `SoapySDRDevice`
/// would dangle past unload); stop the pipeline first.
pub async fn reload_devices(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    tracing::info!("POST /api/devices/reload");
    if let Err(err) = state.reload_devices().await {
        let msg = format!("{err:#}");
        // The only structural refusal is "pipeline is running" — surface
        // it as 409 so the UI can distinguish "wrong state" from a true
        // server fault. Anything else is a 500.
        if msg.contains("pipeline is running") {
            return Err(api_error(
                StatusCode::CONFLICT,
                "RELOAD_REFUSED_RUNNING",
                msg,
            ));
        }
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "RELOAD_FAILED",
            msg,
        ));
    }
    Ok(Json(serde_json::json!({ "status": "ok" })))
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

/// `GET /api/state` — the entire server-side store as
/// `{ seq, kinds: { <kind>: { recent[], current{} } } }`. One read path
/// for all live state: decoder kinds (aprs/adsb/ft8/…), `signals`, the
/// AI transcript (`ai`), plus the live `readback` (driver gain/AGC) and
/// `flowdiag` (per-block throughput) folded by the pipeline diag tick.
/// MCP slices the kind it needs out of this; the browser mirror uses it
/// for the initial fetch + resync (the live feed is `/ws/state`).
pub async fn get_state(
    State(state): State<AppState>,
) -> Json<crate::decoder_store::StoreSnapshot> {
    Json(state.decoder_store().snapshot())
}

/// `POST /api/state/:kind/reset` — clear one store kind (operator /
/// MCP "clear"). Broadcasts a reset delta to mirrors.
pub async fn reset_state_kind(
    State(state): State<AppState>,
    Path(kind): Path<String>,
) -> Json<serde_json::Value> {
    state.decoder_store().reset_kind(&kind);
    Json(serde_json::json!({ "ok": true, "kind": kind }))
}

/// Body of `POST /api/tune`. `freq_hz` is the target listen frequency
/// in Hz; `span_hz` (optional) is a desired sample-rate / passband
/// floor the caller wants spanned (band presets compute this from the
/// band's low/high). `offset_ratio` is the per-driver DC-spike dodge —
/// fraction of the channelizer's output rate to keep between
/// `src_center` and the target when a snap happens. Defaults to 0,
/// meaning "don't dodge, just tune".
#[derive(Deserialize)]
pub struct TuneRequest {
    pub freq_hz: f64,
    #[serde(default)]
    pub span_hz: Option<f64>,
    #[serde(default)]
    pub offset_ratio: f64,
    /// Opt-in: when the target is already inside the digitised span, retune
    /// the channelizer only and leave the LO/graph put — no source restart,
    /// no node-side worker reload. Falls back to a normal tune out-of-span.
    #[serde(default)]
    pub keep_lo: bool,
}

/// `POST /api/tune` — single tuning intent. Every caller that "asks to
/// listen at a frequency" (UI tuner-Enter, band-preset rx/tune,
/// ferrite-ctl tune, AI tune via control plane) routes here so the
/// DC-spike dodge and the "already in range" channelizer-shift fast
/// path apply uniformly. Raw `PATCH /api/source { center_freq_hz }`
/// stays as a low-level escape hatch (no dodge).
pub async fn post_tune(
    State(state): State<AppState>,
    Json(req): Json<TuneRequest>,
) -> Result<Json<ReconfigureResponse>, (StatusCode, Json<ApiError>)> {
    tracing::info!(
        freq_hz = req.freq_hz,
        span_hz = ?req.span_hz,
        offset_ratio = req.offset_ratio,
        keep_lo = req.keep_lo,
        "POST /api/tune"
    );
    let plan = state
        .tune(req.freq_hz, req.span_hz, req.offset_ratio, req.keep_lo)
        .await
        .map_err(|e| bad_request("TUNE_FAILED", format!("{e:#}")))?;
    let mut resp = reconfigure_response(plan);
    resp.source_readback = state.source_readback().await;
    Ok(Json(resp))
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
    tracing::info!(block_id = %id, delta = ?delta, "POST /api/pipeline/blocks/.../params");
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

/// Query for `GET /api/band-plan/at?hz=NNN` — single Hz value the
/// caller wants resolved.
#[derive(Deserialize)]
pub struct BandAtQuery {
    pub hz: f64,
}

/// `GET /api/band-plan/at?hz=NNN` — every US-allocation band whose
/// `[start, end)` window contains the supplied frequency. Multiple
/// matches are common (a primary allocation plus a sub-band carving
/// inside it); the AI sorts to pick the narrowest for naming. Source
/// is Arrin Clark [KN1E]'s `bandplan-usa.json`, embedded into
/// `ferrited` at compile time.
pub async fn band_at(
    Query(q): Query<BandAtQuery>,
) -> Result<Json<Vec<crate::band_plan::BandEntry>>, (StatusCode, Json<ApiError>)> {
    if !q.hz.is_finite() || q.hz < 0.0 {
        return Err(bad_request(
            "BAND_AT_BAD_FREQ",
            format!("hz must be a finite non-negative number, got {}", q.hz),
        ));
    }
    Ok(Json(crate::band_plan::at(q.hz)))
}

/// `GET /api/presets/:name` — return one preset's full JSON. Used by
/// the AI's `preset_detail` MCP tool when picking which preset to
/// load: enumerating via `list_presets` gives label + description
/// but not the full `ai_notes` / block graph / `signal_wiki_url`
/// fields. Name is the on-disk basename (e.g. `wbfm`, `nwr`,
/// `digital-hf-20m`). 404 when the file isn't there or the name has
/// path separators.
pub async fn get_preset(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<ApiError>)> {
    state
        .read_preset_doc(&name)
        .await
        .map(Json)
        .map_err(|e| api_error(StatusCode::NOT_FOUND, "PRESET_NOT_FOUND", format!("{e:#}")))
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
    // one, then re-compose. If the compose fails (a malformed preset
    // could still yield an unsupported browser→node wire), roll the
    // profile back so subsequent preset loads don't carry forward the
    // bad state. (The `audio_split` cut is monotonic by construction and
    // can't itself create one, but the rollback stays as a guard.)
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
    // (Browser-side flowdiag no longer comes through here — it feeds
    // the Flow store directly in-browser via a dedicated worker
    // message, never touching the log stream or this round-trip.)
    if entry.message.starts_with("[transcribe]") {
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

/// `POST /api/screenshot` body. `png_b64` is a base64 PNG (with or
/// without a `data:image/png;base64,` prefix); `label` is an optional
/// short slug folded into the filename so a stored shot is greppable
/// (e.g. `nwr-transcribe`).
#[derive(Deserialize)]
pub struct ScreenshotReq {
    pub png_b64: String,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Serialize)]
pub struct ScreenshotResp {
    pub path: String,
}

/// `POST /api/screenshot` — the top-bar Screenshot button. The browser
/// snapshots its primary canvas and hands us the PNG; we persist it so
/// the operator (or the embedded AI, via the log line) has a durable
/// artifact rather than the ephemeral request-response `/api/ui-views`
/// round-trip. Saved under `FERRITE_SCREENSHOTS_DIR` (default
/// `/tmp/ferrite-views`, the same place `ferrite-ctl view` parks PNGs).
pub async fn save_screenshot(Json(req): Json<ScreenshotReq>) -> impl IntoResponse {
    use base64::Engine as _;
    let raw = req
        .png_b64
        .split_once("base64,")
        .map_or(req.png_b64.as_str(), |(_, b)| b);
    let bytes = match base64::engine::general_purpose::STANDARD.decode(raw.trim()) {
        Ok(b) => b,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("bad png_b64: {e}")).into_response(),
    };
    let dir = std::env::var_os("FERRITE_SCREENSHOTS_DIR").map_or_else(
        || std::path::PathBuf::from("/tmp/ferrite-views"),
        Into::into,
    );
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("mkdir {}: {e}", dir.display()),
        )
            .into_response();
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let slug: String = req
        .label
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let name = if slug.trim_matches('-').is_empty() {
        format!("screenshot-{ms}.png")
    } else {
        format!("screenshot-{ms}-{}.png", slug.trim_matches('-'))
    };
    let path = dir.join(name);
    if let Err(e) = tokio::fs::write(&path, &bytes).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write {}: {e}", path.display()),
        )
            .into_response();
    }
    let path_str = path.display().to_string();
    // Log it so the embedded AI / `ferrite-ctl tail` learns the path
    // without a DOM peek — same observability rationale as the
    // browser-side decoder surfacing.
    tracing::info!(target: "browser", source = "screenshot", "screenshot saved: {path_str}");
    (StatusCode::OK, Json(ScreenshotResp { path: path_str })).into_response()
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
            // ferrited → browser: an outbound command — either a
            // snapshot request (response comes back via /ws/ui-views)
            // or a chrome-state push. Serialise tagged-enum-style and
            // send over the WS.
            out = req_rx.recv() => {
                let Some(out) = out else { break };
                let wire = match &out {
                    crate::view_bridge::Outbound::RequestSnapshot { req_id, pane } => {
                        crate::view_bridge::WireOutbound::ViewRequest {
                            req_id: *req_id,
                            pane: pane.as_str(),
                        }
                    }
                    crate::view_bridge::Outbound::SetViewState(patch) => {
                        crate::view_bridge::WireOutbound::SetViewState { state: patch }
                    }
                };
                let Ok(json) = serde_json::to_string(&wire) else { continue };
                if socket.send(Message::Text(json)).await.is_err() {
                    break;
                }
            }
            // browser → ferrited: a tagged inbound — either a
            // view_response landing the PNG for a prior request, or a
            // view_state push refreshing the chrome cache.
            msg = socket.recv() => {
                let Some(msg) = msg else { break };
                let Ok(msg) = msg else { break };
                match msg {
                    Message::Text(t) => {
                        match serde_json::from_str::<crate::view_bridge::WireInbound>(&t) {
                            Ok(parsed) => bridge.ingest(parsed).await,
                            Err(err) => {
                                tracing::warn!(target: "view_bridge", %err,
                                    "dropping malformed /ws/ui-views frame");
                            }
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

/// `POST /api/ui-view/set` — push a chrome-state patch to the active
/// browser tab. The browser applies the supplied fields to its
/// client controls (`main_pane` → `client.workspace.mainPane`;
/// `channel_detail_visible` → `client.workspace.narrowVisible`).
/// Returns 503 when no tab is connected.
pub async fn set_view_state(
    State(state): State<AppState>,
    Json(patch): Json<crate::view_bridge::UiViewStatePatch>,
) -> impl IntoResponse {
    tracing::info!(?patch, "POST /api/ui-view/set");
    match state.view_bridge().push_state(patch).await {
        Ok(()) => (StatusCode::OK, Json(serde_json::json!({ "status": "ok" }))).into_response(),
        Err(e) => (StatusCode::SERVICE_UNAVAILABLE, e.to_string()).into_response(),
    }
}

/// `GET /api/view` — the cached browser-authored chrome state.
///
/// Returns 200 with a [`UiViewState`] JSON when a viewer has pushed at
/// least once; 503 with the same "open the UI first" message as the
/// snapshot path when nothing has connected yet. The cache is overwritten
/// in full on each browser push, so this is always the latest snapshot
/// (no merge / staleness window beyond one WS hop).
pub async fn get_view_state(State(state): State<AppState>) -> impl IntoResponse {
    match state.view_bridge().state().await {
        Some(s) => (StatusCode::OK, Json(s)).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            crate::view_bridge::ViewError::NoViewer.to_string(),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod ai_fold_tests {
    use super::*;
    use crate::decoder_store::DecoderStore;

    #[test]
    fn user_frame_records_prompt_and_skips_control() {
        let s = DecoderStore::new();
        // A send payload (no `type`) → user record carrying mode.
        fold_ai_user_frame(&s, r#"{"text":"what is on 2m?","mode":"explorer"}"#);
        // Control envelopes are skipped.
        fold_ai_user_frame(&s, r#"{"type":"reset_session","reason":"clear"}"#);
        fold_ai_user_frame(&s, r#"{"type":"stop"}"#);
        // Empty / malformed are skipped.
        fold_ai_user_frame(&s, r#"{"text":""}"#);
        fold_ai_user_frame(&s, "not json");
        let k = s.snapshot_kind("ai").unwrap();
        assert_eq!(k.recent.len(), 1, "only the real prompt is logged");
        assert_eq!(k.recent[0].data["role"], "user");
        assert_eq!(k.recent[0].data["text"], "what is on 2m?");
        assert_eq!(k.recent[0].data["mode"], "explorer");
    }

    #[test]
    fn assistant_frame_joins_text_blocks_and_drops_tool_only() {
        let s = DecoderStore::new();
        fold_ai_assistant_frame(
            &s,
            r#"{"type":"assistant","message":{"content":[
                {"type":"text","text":"Tuning "},
                {"type":"tool_use","id":"x","name":"tune","input":{}},
                {"type":"text","text":"to 145.5."}]}}"#,
        );
        // A tool-only assistant message contributes no transcript text.
        fold_ai_assistant_frame(
            &s,
            r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"y","name":"status","input":{}}]}}"#,
        );
        // Non-assistant frames are ignored.
        fold_ai_assistant_frame(&s, r#"{"type":"ferrite_ai_done"}"#);
        let k = s.snapshot_kind("ai").unwrap();
        assert_eq!(k.recent.len(), 1);
        assert_eq!(k.recent[0].data["role"], "assistant");
        assert_eq!(k.recent[0].data["text"], "Tuning to 145.5.");
    }

    #[test]
    fn inject_batch_folds_user_and_coalesced_assistant() {
        let s = DecoderStore::new();
        // A headless `chat_post role=user` batch.
        fold_ai_inject_batch(
            &s,
            &[serde_json::json!({"type":"ferrite_ai_user_turn","text":"ping","t":1})],
        );
        // A headless `chat_post role=assistant` batch: text_delta + done.
        fold_ai_inject_batch(
            &s,
            &[
                serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta",
                    "delta":{"type":"text_delta","text":"po"}}}),
                serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta",
                    "delta":{"type":"text_delta","text":"ng"}}}),
                serde_json::json!({"type":"ferrite_ai_done"}),
            ],
        );
        // A tool batch (input_json_delta) adds nothing to the transcript.
        fold_ai_inject_batch(
            &s,
            &[serde_json::json!({"type":"stream_event","event":{"type":"content_block_delta",
                "delta":{"type":"input_json_delta","partial_json":"{}"}}})],
        );
        let k = s.snapshot_kind("ai").unwrap();
        assert_eq!(k.recent.len(), 2);
        assert_eq!(k.recent[0].data["text"], "ping");
        assert_eq!(k.recent[1].data["role"], "assistant");
        assert_eq!(k.recent[1].data["text"], "pong");
    }
}
