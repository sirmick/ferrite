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
use tokio_tungstenite::tungstenite::protocol::Message as TungMessage;

use crate::app_state::{
    AppState, CaptureEntry, PipelineBlock, PipelineStatus, PresetEntry, UiSink,
};
use crate::error::AppError;

#[derive(Serialize)]
pub struct Hello {
    pub app: &'static str,
    pub version: &'static str,
    pub status: &'static str,
    /// Event-plane broker addresses (`null` with `--no-mqtt`): `tcp`
    /// for native MQTT clients (ferrite-ctl, agents), `ws` behind the
    /// `/mqtt` reverse proxy for browsers.
    pub mqtt: Option<MqttInfo>,
}

#[derive(Serialize)]
pub struct MqttInfo {
    pub tcp: std::net::SocketAddr,
    pub ws: std::net::SocketAddr,
}

pub async fn hello(State(state): State<AppState>) -> Json<Hello> {
    Json(Hello {
        app: "ferrited",
        version: env!("CARGO_PKG_VERSION"),
        status: "ok",
        mqtt: state.event_plane().map(|p| MqttInfo {
            tcp: p.tcp,
            ws: p.ws,
        }),
    })
}

/// `GET /mqtt` — reverse-proxy a browser WebSocket to the embedded
/// MQTT broker's WS listener, so the page talks to one origin (vite in
/// dev, ferrited in prod) and the broker port stays an implementation
/// detail. MQTT-over-WS is binary frames with subprotocol `mqtt`; we
/// echo the subprotocol to the browser and request it upstream.
pub async fn ws_mqtt(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    let Some(plane) = state.event_plane().cloned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, "event plane disabled").into_response();
    };
    ws.protocols(["mqtt"])
        .on_upgrade(move |socket| ws_mqtt_proxy(socket, plane.ws))
        .into_response()
}

async fn ws_mqtt_proxy(client_ws: WebSocket, upstream: std::net::SocketAddr) {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    // The broker binds the same interface as HTTP (often 0.0.0.0); as a
    // *destination* that means "this host", so dial loopback.
    let upstream = if upstream.ip().is_unspecified() {
        std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), upstream.port())
    } else {
        upstream
    };
    let mut request = match format!("ws://{upstream}/mqtt").into_client_request() {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "mqtt proxy: bad upstream request");
            let _ = client_ws.close().await;
            return;
        }
    };
    request.headers_mut().insert(
        "sec-websocket-protocol",
        http::HeaderValue::from_static("mqtt"),
    );
    let upstream_ws = match tokio_tungstenite::connect_async(request).await {
        Ok((s, _resp)) => s,
        Err(e) => {
            tracing::warn!(error = %e, %upstream, "mqtt proxy: broker unreachable");
            let _ = client_ws.close().await;
            return;
        }
    };
    let (mut up_tx, mut up_rx) = upstream_ws.split();
    let (mut cli_tx, mut cli_rx) = client_ws.split();

    let c2u = async move {
        while let Some(Ok(msg)) = cli_rx.next().await {
            let out = match msg {
                Message::Binary(b) => TungMessage::Binary(b),
                Message::Text(t) => TungMessage::Text(t),
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => continue,
            };
            if up_tx.send(out).await.is_err() {
                break;
            }
        }
        let _ = up_tx.close().await;
    };
    let u2c = async move {
        while let Some(Ok(msg)) = up_rx.next().await {
            let out = match msg {
                TungMessage::Binary(b) => Message::Binary(b),
                TungMessage::Text(t) => Message::Text(t),
                TungMessage::Close(_) => break,
                TungMessage::Ping(_) | TungMessage::Pong(_) | TungMessage::Frame(_) => continue,
            };
            if cli_tx.send(out).await.is_err() {
                break;
            }
        }
        let _ = cli_tx.close().await;
    };
    tokio::select! {
        _ = c2u => {},
        _ = u2c => {},
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
///
/// This is the AI *control* channel only (prompts, stop, reset, the
/// sidecar's hello + session binding). The conversation itself — every
/// session, whichever client drives it — is the `ai` store kind: the
/// sidecar and headless drivers publish records through the event
/// plane's ingest door, and every panel renders from the mirror.
pub async fn ws_chat(ws: WebSocketUpgrade) -> impl IntoResponse {
    let upstream_url =
        std::env::var("FERRITE_AI_URL").unwrap_or_else(|_| DEFAULT_FERRITE_AI_URL.to_string());
    ws.on_upgrade(move |socket| ws_chat_proxy(socket, upstream_url))
}

/// The sidecar's HTTP origin, from `FERRITE_AI_URL` (`ws://host:port/…`
/// → `http://host:port`).
fn ai_http_base() -> String {
    let ws = std::env::var("FERRITE_AI_URL").unwrap_or_else(|_| DEFAULT_FERRITE_AI_URL.to_string());
    let http = ws
        .replacen("wss://", "https://", 1)
        .replacen("ws://", "http://", 1);
    match http.find("://").map(|i| i + 3) {
        Some(start) => match http[start..].find('/') {
            Some(slash) => http[..start + slash].to_string(),
            None => http,
        },
        None => http,
    }
}

/// `POST /api/ai/prompt`, `POST /api/ai/cancel`, `GET /api/ai/sessions`
/// — the REST side of the ferrite-ai sidecar (its `/prompt`, `/cancel`,
/// `/sessions`), passed through. A prompt started here runs like a
/// panel's: the conversation shows on every AI panel via the `ai` kind.
/// `{"wait": true}` holds the request until the turn ends.
pub async fn ai_proxy(
    method: http::Method,
    uri: http::Uri,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let tail = uri.path().trim_start_matches("/api/ai");
    let url = format!("{}{}", ai_http_base(), tail);
    let client = reqwest::Client::new();
    let req = if method == http::Method::POST {
        client
            .post(&url)
            .header("content-type", "application/json")
            .body(body.to_vec())
    } else {
        client.get(&url)
    };
    match req.send().await {
        Ok(r) => {
            let status = StatusCode::from_u16(r.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let bytes = r.bytes().await.unwrap_or_default();
            (status, [(http::header::CONTENT_TYPE, "application/json")], bytes).into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": format!("ferrite-ai sidecar unreachable at {url}: {e}") })),
        )
            .into_response(),
    }
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

    // Sidecar → browser.
    let u2c = async move {
        while let Some(Ok(msg)) = up_rx.next().await {
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
        () = c2u => {},
        () = u2c => {},
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
) -> Result<Json<RecentResponse>, AppError> {
    let logs = state.logs().ok_or_else(|| {
        AppError::bad_request(
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
) -> Result<Json<Vec<DeviceEntry>>, AppError> {
    let devices = tokio::task::spawn_blocking(|| {
        crate::device::list_devices_with_timeout(crate::device::DEFAULT_PROBE_TIMEOUT)
    })
    .await
    .map_err(|e| {
        AppError::internal(
            "DEVICE_PROBE_FAILED",
            format!("device enumerate task panicked: {e}"),
        )
    })?
    .map_err(|e| AppError::internal("DEVICE_PROBE_FAILED", format!("{e:#}")))?;

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
) -> Result<Json<serde_json::Value>, AppError> {
    tracing::info!("POST /api/devices/reload");
    // The 409-vs-500 distinction is typed now: `reload_devices` returns
    // `AppError::Conflict` while the pipeline is running, so there is no
    // message string-matching here.
    state.reload_devices().await?;
    Ok(Json(serde_json::json!({ "status": "ok" })))
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
    /// `POST /api/tune` with `carrier_hz`: the dial it set and why
    /// (`{carrier_hz, tone_hz, sideband, dial_hz}`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tuned: Option<serde_json::Value>,
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
            tuned: None,
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
            tuned: None,
        },
    }
}

/// `GET /api/flowgraph` — snapshot the preset doc as JSON.
pub async fn get_flowgraph(State(state): State<AppState>) -> Json<FlowgraphDoc> {
    Json(state.get_flowgraph().await)
}

/// `GET /api/flowgraph/browser-half` — the composed doc carved for the
/// browser environment, for the in-browser WASM runtime to run verbatim.
/// See [`AppState::browser_half`]: this is the single source of truth for
/// what the browser runs, so its `WsBridge` stream IDs line up with the
/// node half by construction (no client-side re-derivation).
pub async fn get_flowgraph_browser_half(
    State(state): State<AppState>,
) -> Result<Json<FlowgraphDoc>, AppError> {
    state
        .browser_half()
        .await
        .map(Json)
        .map_err(|e| AppError::bad_request("COMPOSE_FAILED", format!("{e:#}")))
}

/// `PATCH /api/flowgraph` — store a new preset. Reconfigures the
/// running pipeline if there is one; otherwise the doc is queued for
/// the next `start`.
pub async fn patch_flowgraph(
    State(state): State<AppState>,
    Json(new_doc): Json<FlowgraphDoc>,
) -> Result<Json<ReconfigureResponse>, AppError> {
    let plan = state
        .patch_flowgraph(new_doc)
        .await
        .map_err(|e| AppError::radio_or_bad_request("RECONFIGURE_FAILED", &e))?;
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
) -> Result<Json<ReconfigureResponse>, AppError> {
    tracing::info!(type_name = %new_source.type_name, params = ?new_source.params, "PATCH /api/source");
    let plan = state
        .patch_source(new_source)
        .await
        .map_err(|e| AppError::radio_or_bad_request("RECONFIGURE_FAILED", &e))?;
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
) -> Result<Json<PipelineStatusResponse>, AppError> {
    tracing::info!("POST /api/pipeline/start");
    state
        .start()
        .await
        .map_err(|e| AppError::radio_or_bad_request("PIPELINE_START_FAILED", &e))?;
    Ok(Json(PipelineStatusResponse {
        status: PipelineStatus::Running,
    }))
}

/// `GET /api/ui-sinks` — enumerate every `ui:<name>` sink in the
/// composed preset and the `stream_id` env_split allocates for it. The
/// client uses this to subscribe to the right frame streams.
pub async fn list_ui_sinks(State(state): State<AppState>) -> Result<Json<Vec<UiSink>>, AppError> {
    state
        .ui_sinks()
        .await
        .map(Json)
        .map_err(|e| AppError::bad_request("UI_SINKS_FAILED", format!("{e:#}")))
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
/// for the initial fetch + resync (the live feed is the event bus).
pub async fn get_state(State(state): State<AppState>) -> Json<crate::decoder_store::StoreSnapshot> {
    Json(state.decoder_store().snapshot())
}

/// `POST /api/state/:kind/reset` — clear one store kind (operator /
/// MCP "clear"). Broadcasts a reset delta to mirrors.
///
/// Resetting `ai` also appends one `{type:"clear"}` record. The AI panel
/// interleaves the `activity` and `analysis` kinds with the
/// conversation; it hides anything older than the newest `clear`
/// record, so a clear empties the whole timeline for every panel (and
/// survives a reload or restart) without wiping those shared logs.
pub async fn reset_state_kind(
    State(state): State<AppState>,
    Path(kind): Path<String>,
) -> Json<serde_json::Value> {
    let store = state.decoder_store();
    store.reset_kind(&kind);
    if kind == "ai" {
        store.apply("ai", serde_json::json!({ "type": "clear" }));
    }
    Json(serde_json::json!({ "ok": true, "kind": kind }))
}

/// Body of `POST /api/tune`. `freq_hz` is the target listen frequency
/// in Hz; `span_hz` (optional) is a desired sample-rate / passband
/// floor the caller wants spanned (band presets compute this from the
/// band's low/high). `offset_ratio` is the per-driver DC-spike dodge —
/// fraction of the channelizer's output rate to keep between
/// `src_center` and the target when a snap happens. Omit it (the common
/// case) and the daemon fills the per-driver default keyed on the bound
/// `driver_key`; pass an explicit value to override (`0` = don't dodge).
#[derive(Deserialize)]
pub struct TuneRequest {
    /// Listen (dial) frequency. Omit when giving `carrier_hz`.
    #[serde(default)]
    pub freq_hz: Option<f64>,
    /// Tune so this RF carrier lands on an audio tone instead: the dial
    /// becomes `carrier − tone` (USB) or `carrier + tone` (LSB), the
    /// sideband read from the preset's `SsbDemod`. For CW / digital
    /// modes whose decoder wants the signal at a fixed tone.
    #[serde(default)]
    pub carrier_hz: Option<f64>,
    /// The tone for `carrier_hz`. Default: the decoder's own
    /// `rx_freq_hz` (CwDemod / the fldigi blocks) in the running preset.
    #[serde(default)]
    pub tone_hz: Option<f64>,
    #[serde(default)]
    pub span_hz: Option<f64>,
    #[serde(default)]
    pub offset_ratio: Option<f64>,
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
) -> Result<Json<ReconfigureResponse>, AppError> {
    tracing::info!(
        freq_hz = ?req.freq_hz,
        carrier_hz = ?req.carrier_hz,
        tone_hz = ?req.tone_hz,
        span_hz = ?req.span_hz,
        offset_ratio = ?req.offset_ratio,
        keep_lo = req.keep_lo,
        "POST /api/tune"
    );
    let (freq_hz, tuned) = match (req.freq_hz, req.carrier_hz) {
        (_, Some(carrier)) => {
            let t = state
                .dial_for_carrier(carrier, req.tone_hz)
                .await
                .map_err(|e| AppError::bad_request("TUNE_FAILED", format!("{e:#}")))?;
            let dial = t.dial_hz;
            (dial, Some(serde_json::to_value(t).unwrap_or_default()))
        }
        (Some(f), None) => (f, None),
        (None, None) => {
            return Err(AppError::bad_request(
                "TUNE_FAILED",
                "give freq_hz (dial) or carrier_hz (+ optional tone_hz)",
            ))
        }
    };
    let plan = state
        .tune(freq_hz, req.span_hz, req.offset_ratio, req.keep_lo)
        .await
        .map_err(|e| AppError::radio_or_bad_request("TUNE_FAILED", &e))?;
    let mut resp = reconfigure_response(plan);
    resp.source_readback = state.source_readback().await;
    resp.tuned = tuned;
    Ok(Json(resp))
}

/// `GET /api/pipeline/blocks` — every block in the currently-loaded
/// composed preset, with its full spec and current param values.
/// Source for the generic `<BlockParams>` UI component. See D24 in
/// `docs/09-decisions.md`.
pub async fn list_pipeline_blocks(
    State(state): State<AppState>,
) -> Result<Json<Vec<PipelineBlock>>, AppError> {
    state
        .list_blocks()
        .await
        .map(Json)
        .map_err(|e| AppError::bad_request("LIST_PIPELINE_BLOCKS_FAILED", format!("{e:#}")))
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
) -> Result<Json<ReconfigureResponse>, AppError> {
    let is_src = id == ferrite_runtime::SOURCE_ID;
    tracing::info!(block_id = %id, delta = ?delta, "POST /api/pipeline/blocks/.../params");
    let plan = state
        .apply_block_params(&id, delta)
        .await
        .map_err(|e| AppError::radio_or_bad_request("RECONFIGURE_FAILED", &e))?;
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
) -> Result<Json<SourceCapabilitiesResponse>, AppError> {
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

/// Body of `POST /api/presets/:name`.
#[derive(Debug, Default, Deserialize)]
pub struct SavePresetRequest {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub overwrite: bool,
}

/// `POST /api/presets/:name` — save the running graph (live params
/// included) as a user preset; it then lists and loads like any other.
pub async fn save_preset(
    State(state): State<AppState>,
    Path(name): Path<String>,
    body: Option<Json<SavePresetRequest>>,
) -> Result<Json<crate::app_state::PresetEntry>, AppError> {
    let req = body.map(|Json(b)| b).unwrap_or_default();
    state
        .save_user_preset(&name, req.label, req.description, req.overwrite)
        .await
        .map(Json)
}

/// `GET /api/presets` — enumerate every `*.json` preset the server can
/// load. Returns an empty list when no presets dir is configured.
pub async fn list_presets(
    State(state): State<AppState>,
) -> Result<Json<Vec<PresetEntry>>, AppError> {
    state
        .list_presets()
        .await
        .map(Json)
        .map_err(|e| AppError::bad_request("LIST_PRESETS_FAILED", format!("{e:#}")))
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
) -> Result<Json<Vec<crate::band_plan::BandEntry>>, AppError> {
    if !q.hz.is_finite() || q.hz < 0.0 {
        return Err(AppError::bad_request(
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
) -> Result<Json<serde_json::Value>, AppError> {
    state
        .read_preset_doc(&name)
        .await
        .map(Json)
        .map_err(|e| AppError::not_found("PRESET_NOT_FOUND", format!("{e:#}")))
}

/// `GET /api/presets/:name/reference-image` — the preset's reference
/// waterfall (`signal_wiki_image`, a sigidwiki picture under `samples/`):
/// what the signal it decodes looks like, for matching by eye.
pub async fn get_preset_reference_image(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<axum::response::Response, AppError> {
    let doc = state
        .read_preset_doc(&name)
        .await
        .map_err(|e| AppError::not_found("PRESET_NOT_FOUND", format!("{e:#}")))?;
    let rel = doc
        .get("signal_wiki_image")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            AppError::not_found(
                "NO_REFERENCE_IMAGE",
                format!("preset {name:?} has no reference image"),
            )
        })?;
    let root = state
        .captures_dir()
        .ok_or_else(|| AppError::unavailable("NO_SAMPLES", "no samples/ tree configured"))?;
    // `samples/sigidwiki/images/x.jpg` → under the samples root; never
    // outside it.
    let under = rel.strip_prefix("samples/").unwrap_or(rel);
    let root = std::fs::canonicalize(root)
        .map_err(|e| AppError::unavailable("NO_SAMPLES", format!("samples/: {e}")))?;
    let path = std::fs::canonicalize(root.join(under))
        .ok()
        .filter(|p| p.starts_with(&root))
        .ok_or_else(|| AppError::not_found("NO_REFERENCE_IMAGE", format!("{rel} not found")))?;
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
    {
        Some(e) if e == "png" => "image/png",
        Some(e) if e == "gif" => "image/gif",
        Some(e) if e == "webp" => "image/webp",
        _ => "image/jpeg",
    };
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| AppError::internal("READ_FAILED", format!("{rel}: {e}")))?;
    Ok(([(http::header::CONTENT_TYPE, mime)], bytes).into_response())
}

/// `GET /api/captures` — enumerate replayable IQ/audio fixtures under
/// the `samples/` tree (with `*.json` sidecar metadata) so the File
/// source tab can list them instead of asking for a server path.
pub async fn list_captures(
    State(state): State<AppState>,
) -> Result<Json<Vec<CaptureEntry>>, AppError> {
    state
        .list_captures()
        .await
        .map(Json)
        .map_err(|e| AppError::bad_request("LIST_CAPTURES_FAILED", format!("{e:#}")))
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
) -> Result<Json<LoadPresetResponse>, AppError> {
    let started = std::time::Instant::now();
    tracing::info!(name = %req.name, "POST /api/preset");
    if let Some(profile) = req.profile {
        state.set_profile(profile).await;
    }
    let (doc, plan) = state
        .load_preset_by_name(&req.name)
        .await
        .map_err(|e| AppError::radio_or_bad_request("LOAD_PRESET_FAILED", &e))?;
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

// ─── capture orchestration ──────────────────────────────────────────────
//
// ferrited owns the capture job registry + state machines (see
// `crate::capture`); ferrite-ctl's MCP verbs are thin wrappers over
// these. A `POST` registers a job and returns it immediately; the
// recording runs in a background task and every status change is a
// `jobs` record in the store (`ferrite/state/jobs/<id>` on the event
// plane, `kinds.jobs` in `GET /api/state`).

fn capture_err(e: crate::capture::CaptureError) -> AppError {
    match e {
        crate::capture::CaptureError::Invalid(m) => AppError::bad_request("CAPTURE_REJECTED", m),
        crate::capture::CaptureError::Internal(m) => AppError::internal("CAPTURE_FAILED", m),
    }
}

/// `POST /api/capture/iq` — start an async IQ capture. Non-wideband tees
/// the live post-channelizer stream; `wideband=true` swaps in `capture_fm`
/// for a full-rate slice (needs `freq_hz`). Returns the `Running` job.
pub async fn post_capture_iq(
    State(state): State<AppState>,
    Json(req): Json<crate::capture::CaptureIqReq>,
) -> Result<Json<crate::capture::CaptureJob>, AppError> {
    tracing::info!("POST /api/capture/iq");
    state
        .start_capture_iq(req)
        .await
        .map(Json)
        .map_err(capture_err)
}

/// `POST /api/capture/fft` — start an async FFT-byte-stream capture (the
/// non-disruptive `logmag` tee). Returns the `Running` job.
pub async fn post_capture_fft(
    State(state): State<AppState>,
    Json(req): Json<crate::capture::CaptureIqReq>,
) -> Result<Json<crate::capture::CaptureJob>, AppError> {
    tracing::info!("POST /api/capture/fft");
    state
        .start_capture_fft(req)
        .await
        .map(Json)
        .map_err(capture_err)
}

/// `POST /api/capture/audio` — start an async demod-audio capture (a
/// preset swap to a `*-record` preset; disruptive). Returns the job.
pub async fn post_capture_audio(
    State(state): State<AppState>,
    Json(req): Json<crate::capture::CaptureAudioReq>,
) -> Result<Json<crate::capture::CaptureJob>, AppError> {
    tracing::info!("POST /api/capture/audio");
    state
        .start_capture_audio(req)
        .await
        .map(Json)
        .map_err(capture_err)
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
) -> Result<Json<Profile>, AppError> {
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
        return Err(AppError::bad_request(
            "PROFILE_APPLY_FAILED",
            format!("{e:#}"),
        ));
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
        Err(e) => {
            return AppError::bad_request("BAD_PNG_B64", format!("bad png_b64: {e}"))
                .into_response()
        }
    };
    let dir = std::env::var_os("FERRITE_SCREENSHOTS_DIR").map_or_else(
        || std::path::PathBuf::from("/tmp/ferrite-views"),
        Into::into,
    );
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return AppError::internal(
            "SCREENSHOT_DIR_FAILED",
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
        return AppError::internal(
            "SCREENSHOT_WRITE_FAILED",
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
        return AppError::bad_request(
            "UNKNOWN_PANE",
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
            AppError::unavailable("NO_VIEWER", e.to_string()).into_response()
        }
        Err(e @ crate::view_bridge::ViewError::Timeout) => {
            AppError::timeout("VIEW_SNAPSHOT_TIMEOUT", e.to_string()).into_response()
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
        Err(e) => AppError::unavailable("NO_VIEWER", e.to_string()).into_response(),
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
        None => AppError::unavailable(
            "NO_VIEWER",
            crate::view_bridge::ViewError::NoViewer.to_string(),
        )
        .into_response(),
    }
}
