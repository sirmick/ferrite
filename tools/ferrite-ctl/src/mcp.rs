//! MCP (Model Context Protocol) server surface for ferrite-ctl — and
//! the single operations layer the human CLI drives too.
//!
//! `ferrite-ctl mcp` speaks JSON-RPC over stdio. Each verb is
//! implemented exactly once as an `*_op` method on [`FerriteServer`]
//! that talks to a running `ferrited` over its REST API. Two thin
//! front-ends sit on top of those ops:
//!
//! - the MCP tools (`#[tool]` wrappers below) expose them to any
//!   MCP-enabled client (Claude Desktop / Code, the ferrite-ai sidecar
//!   via the Agent SDK's `mcpServers` config);
//! - the human CLI subcommands in `main.rs` call the same `*_op`
//!   methods directly and print the result.
//!
//! Because both paths funnel through one implementation, the CLI and
//! the MCP surface can't drift — a CLI verb *is* its MCP tool. Schemas
//! and descriptions for the tool args are derived from the request
//! structs via `schemars` (re-exported through `rmcp`).
//!
//! Activity logging: every mutating op stamps `x-ferrite-command`
//! (the verb) and an optional `x-ferrite-note` (the AI's "why") on the
//! request. The daemon's `ai_activity_layer` middleware turns those
//! into `ai::activity` log lines for the UI's transcript panel.
//! Read-only GETs are left untagged so polling doesn't flood the panel.
//!
//! Implementation note: stdin/stdout are reserved for the MCP
//! transport, so every message that isn't a protocol frame must go to
//! stderr (`tracing-subscriber` with `with_writer(stderr)` below). A
//! stray `println!` here corrupts the JSON-RPC stream.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use reqwest::Client;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars, tool, tool_handler, tool_router,
    transport::stdio,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tracing_subscriber::EnvFilter;

/// Thin HTTP shim around the running `ferrited`. Carries the base URL
/// and an optional default activity note (the CLI's global `--note`,
/// or the note `ferrite-ctl --note … mcp` was launched with). Mutating
/// calls stamp `x-ferrite-command` + `x-ferrite-note`; reads don't.
#[derive(Clone)]
pub(crate) struct Http {
    pub(crate) client: Client,
    pub(crate) base: String,
    /// Applied as `x-ferrite-note` on mutating calls when the per-call
    /// note is `None`. `None` = no default annotation.
    default_note: Option<String>,
    /// `x-ferrite-via`: who is calling — `cli` (a person at a terminal)
    /// or `mcp` (an agent). Lets the UI label analyses and activity.
    via: &'static str,
}

impl Http {
    pub(crate) fn new(client: Client, base: String, default_note: Option<String>) -> Self {
        Self {
            client,
            base,
            default_note,
            via: "cli",
        }
    }

    /// The same shim, marked as driven by an agent over MCP.
    #[must_use]
    pub(crate) fn via_mcp(mut self) -> Self {
        self.via = "mcp";
        self
    }

    /// Read. Never logged to the activity panel (GET is filtered out by
    /// the daemon middleware regardless of headers).
    async fn get(&self, path: &str) -> Result<Value, McpError> {
        let resp = self
            .client
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .map_err(|e| http_err(&format!("GET {path}"), e.to_string()))?;
        api_response(resp, &format!("GET {path}")).await
    }

    /// Read a binary body (an image): bytes + content type.
    async fn get_bytes(&self, path: &str) -> Result<(Vec<u8>, String), McpError> {
        let ctx = format!("GET {path}");
        let resp = self
            .client
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .map_err(|e| http_err(&ctx, e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(http_err(&ctx, format!("{status}: {body}")));
        }
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| http_err(&ctx, format!("read body: {e}")))?;
        Ok((bytes.to_vec(), mime))
    }

    /// Mutating POST, tagged with `cmd` (the verb kebab-id) + an
    /// optional per-call note so the daemon surfaces it under
    /// `ai::activity`.
    async fn post(
        &self,
        path: &str,
        body: Value,
        cmd: &str,
        note: Option<&str>,
    ) -> Result<Value, McpError> {
        let req = self
            .client
            .post(format!("{}{}", self.base, path))
            .json(&body);
        let resp = self
            .activity(req, cmd, note)
            .send()
            .await
            .map_err(|e| http_err(&format!("POST {path}"), e.to_string()))?;
        api_response(resp, &format!("POST {path}")).await
    }

    /// [`Self::post`] with its own timeout — for calls that do work on
    /// the daemon for longer than the default per-request limit.
    async fn post_slow(
        &self,
        path: &str,
        body: Value,
        cmd: &str,
        note: Option<&str>,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        let req = self
            .client
            .post(format!("{}{}", self.base, path))
            .timeout(timeout)
            .json(&body);
        let resp = self
            .activity(req, cmd, note)
            .send()
            .await
            .map_err(|e| http_err(&format!("POST {path}"), e.to_string()))?;
        api_response(resp, &format!("POST {path}")).await
    }

    /// `POST /api/ai/prompt` — start a turn of the in-app AI (CLI only:
    /// an agent prompting the in-app agent is not a tool). `wait` holds
    /// the request until the turn ends.
    pub(crate) async fn ai_prompt(&self, body: Value, wait: bool) -> Result<Value, McpError> {
        let timeout = Duration::from_secs(if wait { 3600 } else { 120 });
        self.post_slow("/api/ai/prompt", body, "ai-prompt", None, timeout)
            .await
    }

    /// Mutating PATCH, tagged like [`Self::post`].
    async fn patch(
        &self,
        path: &str,
        body: Value,
        cmd: &str,
        note: Option<&str>,
    ) -> Result<Value, McpError> {
        let req = self
            .client
            .patch(format!("{}{}", self.base, path))
            .json(&body);
        let resp = self
            .activity(req, cmd, note)
            .send()
            .await
            .map_err(|e| http_err(&format!("PATCH {path}"), e.to_string()))?;
        api_response(resp, &format!("PATCH {path}")).await
    }

    /// Attach the activity headers. Header values reject CRLF / non-ASCII
    /// control chars, so a note that won't encode is silently skipped
    /// rather than failing the request.
    fn activity(
        &self,
        req: reqwest::RequestBuilder,
        cmd: &str,
        note: Option<&str>,
    ) -> reqwest::RequestBuilder {
        let mut req = req
            .header("x-ferrite-command", cmd)
            .header("x-ferrite-via", self.via);
        let note = note.or(self.default_note.as_deref());
        if let Some(n) = note {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(n) {
                req = req.header("x-ferrite-note", v);
            }
        }
        req
    }
}

fn http_err(ctx: &str, msg: String) -> McpError {
    McpError::internal_error(format!("{ctx}: {msg}"), None)
}

/// One message off the event plane, as delivered to a `subscribe_records`
/// callback. `seq` is present for store deltas (not for log lines).
pub(crate) struct EventRecord {
    pub topic: String,
    pub retain: bool,
    pub seq: Option<u64>,
    pub value: Value,
}

impl Http {
    /// Host part of `base` (`http://host:port` → `host`), for dialling the
    /// broker on the same machine as the daemon.
    fn host(&self) -> String {
        let s = self.base.split("://").nth(1).unwrap_or(&self.base);
        let s = s.split('/').next().unwrap_or(s);
        // IPv6 literal `[::1]:12001` → keep the brackets' content.
        if let Some(rest) = s.strip_prefix('[') {
            return rest.split(']').next().unwrap_or(rest).to_string();
        }
        s.rsplit_once(':').map_or(s, |(h, _)| h).to_string()
    }

    /// Subscribe to `ferrite/state/<kind>` (and its subtree) on the
    /// daemon's embedded broker and feed every message to `on_record`
    /// until it returns `false` or `timeout` elapses. Returns whether the
    /// timeout hit. The broker's TCP port comes from `GET /api/hello`.
    /// The daemon's MQTT TCP port, from `GET /api/hello`.
    async fn broker_port(&self) -> Result<u16, McpError> {
        let hello = self.get("/api/hello").await?;
        hello
            .pointer("/mqtt/tcp")
            .and_then(Value::as_str)
            .and_then(|a| a.rsplit_once(':').map(|(_, p)| p.to_string()))
            .and_then(|p| p.parse::<u16>().ok())
            .ok_or_else(|| {
                McpError::internal_error(
                    "ferrited has no event plane (started with --no-mqtt?)".to_string(),
                    None,
                )
            })
    }

    /// Publish raw events to `ferrite/ingest/<kind>` (the event plane's
    /// ingest door — the server folds each into the store). QoS 1 and
    /// waits for every ack, so a short-lived CLI process (a hook) can't
    /// exit before delivery.
    pub(crate) async fn publish_ingest(
        &self,
        kind: &str,
        events: &[Value],
    ) -> Result<usize, McpError> {
        use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
        let port = self.broker_port().await?;
        let id = format!(
            "ferrite-ctl-pub-{}-{}",
            std::process::id(),
            now_ms() % 1_000_000
        );
        let mut opts = MqttOptions::new(id, self.host(), port);
        opts.set_keep_alive(Duration::from_secs(15));
        opts.set_max_packet_size(8 * 1024 * 1024, 8 * 1024 * 1024);
        let (client, mut eventloop) = AsyncClient::new(opts, events.len().max(1) + 4);
        let topic = format!("ferrite/ingest/{}", kind.trim_matches('/'));
        for ev in events {
            client
                .publish(topic.clone(), QoS::AtLeastOnce, false, ev.to_string())
                .await
                .map_err(|e| McpError::internal_error(format!("mqtt publish: {e}"), None))?;
        }
        let mut acked = 0usize;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while acked < events.len() {
            match tokio::time::timeout_at(deadline, eventloop.poll()).await {
                Ok(Ok(Event::Incoming(Packet::PubAck(_)))) => acked += 1,
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    return Err(McpError::internal_error(
                        format!("mqtt {}:{port}: {e}", self.host()),
                        None,
                    ))
                }
                Err(_) => {
                    return Err(McpError::internal_error(
                        format!("mqtt publish: only {acked}/{} acked in 5 s", events.len()),
                        None,
                    ))
                }
            }
        }
        let _ = client.disconnect().await;
        let _ = tokio::time::timeout(Duration::from_millis(200), eventloop.poll()).await;
        Ok(acked)
    }

    pub(crate) async fn subscribe_records(
        &self,
        kind: &str,
        timeout: Duration,
        mut on_record: impl FnMut(EventRecord) -> bool,
    ) -> Result<bool, McpError> {
        use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
        let port = self.broker_port().await?;
        let host = self.host();
        let id = format!(
            "ferrite-ctl-{}-{}",
            std::process::id(),
            now_ms() % 1_000_000
        );
        let mut opts = MqttOptions::new(id, host, port);
        opts.set_keep_alive(Duration::from_secs(15));
        let (client, mut eventloop) = AsyncClient::new(opts, 64);
        let kind = kind.trim_matches('/');
        let topic = format!("ferrite/state/{kind}");
        let deadline = tokio::time::Instant::now() + timeout;
        let mut subscribed = false;
        // Store seqs are unique per record across every kind. A keyed
        // upsert is published on both `<kind>` and `<kind>/<key>` (and a
        // broker may double-forward a retained publish on a new topic),
        // so dedup by seq — bounded, oldest evicted.
        let mut seen: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
        loop {
            let ev = match tokio::time::timeout_at(deadline, eventloop.poll()).await {
                Ok(Ok(ev)) => ev,
                Ok(Err(e)) => {
                    return Err(McpError::internal_error(
                        format!("mqtt {}:{port}: {e}", self.host()),
                        None,
                    ))
                }
                Err(_) => return Ok(true),
            };
            match ev {
                Event::Incoming(Packet::ConnAck(_)) if !subscribed => {
                    subscribed = true;
                    client
                        // `<topic>/#` also matches `<topic>` itself (MQTT
                        // 3.1.1 §4.7.1.2); subscribing to both would deliver
                        // every record twice.
                        .subscribe(format!("{topic}/#"), QoS::AtMostOnce)
                        .await
                        .map_err(|e| {
                            McpError::internal_error(format!("mqtt subscribe: {e}"), None)
                        })?;
                }
                Event::Incoming(Packet::Publish(p)) => {
                    if p.payload.is_empty() {
                        continue; // a cleared retained entity
                    }
                    let value: Value = match serde_json::from_slice(&p.payload) {
                        Ok(v) => v,
                        Err(_) => Value::String(String::from_utf8_lossy(&p.payload).into_owned()),
                    };
                    let seq = value.get("seq").and_then(Value::as_u64);
                    if let Some(s) = seq {
                        if seen.contains(&s) {
                            continue;
                        }
                        seen.push_back(s);
                        if seen.len() > 4096 {
                            seen.pop_front();
                        }
                    }
                    let rec = EventRecord {
                        topic: p.topic.clone(),
                        retain: p.retain,
                        seq,
                        value,
                    };
                    if !on_record(rec) {
                        let _ = client.disconnect().await;
                        return Ok(false);
                    }
                }
                _ => {}
            }
        }
    }
}

async fn api_response(resp: reqwest::Response, ctx: &str) -> Result<Value, McpError> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| http_err(ctx, format!("read body: {e}")))?;
    if !status.is_success() {
        // Lead with the server's typed code + message so an agent can
        // branch on it (e.g. `RADIO_UNREACHABLE`) instead of parsing a
        // raw driver string; fall back to the raw body.
        let typed = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
            let e = v.get("error")?;
            Some(format!(
                "{}: {}",
                e.get("code")?.as_str()?,
                e.get("message")?.as_str()?
            ))
        });
        return Err(http_err(
            ctx,
            typed.unwrap_or_else(|| format!("{status}: {text}")),
        ));
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| http_err(ctx, format!("parse JSON: {e}")))
}

fn ok_json(value: &Value) -> Result<CallToolResult, McpError> {
    let s = serde_json::to_string(value)
        .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

/// Percent-encode a query-string value (RFC 3986 unreserved kept).
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// [`ok_json`] plus an inline image — the agent sees the picture, and
/// the operator sees it on the tool's card in the AI tab.
fn ok_json_image(value: &Value, bytes: &[u8], mime: &str) -> Result<CallToolResult, McpError> {
    use base64::Engine as _;
    let s = serde_json::to_string(value)
        .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))?;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(CallToolResult::success(vec![
        Content::text(s),
        Content::image(data, mime.to_string()),
    ]))
}

/// The store's single-current slot key for `Replace`/`Merge` kinds —
/// matches `decoder_store::REPLACE_KEY` server-side.
const STORE_REPLACE_KEY: &str = "current";

/// Slice a single-current kind's payload out of a `GET /api/state`
/// snapshot (`kinds.<kind>.current.current.data`), or `Null` when the
/// kind has produced nothing.
fn store_current(snapshot: &Value, kind: &str) -> Value {
    snapshot
        .get("kinds")
        .and_then(|k| k.get(kind))
        .and_then(|k| k.get("current"))
        .and_then(|c| c.get(STORE_REPLACE_KEY))
        .and_then(|r| r.get("data"))
        .cloned()
        .unwrap_or(Value::Null)
}

// ─── tool request schemas ───────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SelectDeviceArgs {
    /// Soapy device-args string identifying which SDR to bind to,
    /// e.g. `driver=rtlsdr,serial=00000001` or
    /// `driver=hackrf,serial=0000000000000000b74865dc2b4f1bd7`.
    /// Use `list_devices` to see what's enumerable.
    pub args: String,
    /// Optional "why" note surfaced in the UI's activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoadPresetArgs {
    /// Preset basename — matches `flowgraphs/<name>.json` on the
    /// daemon (e.g. `wbfm`, `nwr`, `adsb`, `rtl433-433`). Use
    /// `list_presets` to enumerate.
    pub name: String,
    /// Optional "why" note surfaced in the UI's activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TuneArgs {
    /// Listen frequency in Hz (the operator-visible tune target). For
    /// zero-IF radios the server places the source LO off-target per
    /// the per-driver dodge ratio and pulls this freq back through
    /// the channelizer — see D29 in docs/09-decisions.md. Omit when
    /// giving `carrier_hz`.
    #[serde(default)]
    pub freq_hz: Option<f64>,
    /// Put this RF carrier on an audio tone instead of naming the dial:
    /// the daemon sets dial = carrier − tone (USB) / + tone (LSB) from
    /// the preset's SSB demod. Use for CW and digital modes — e.g. a CW
    /// carrier found with `spectrum` → `tune(carrier_hz=…)` lands it on
    /// the CW decoder's `rx_freq_hz`.
    #[serde(default)]
    pub carrier_hz: Option<f64>,
    /// Tone for `carrier_hz`, Hz. Omit to use the running decoder's own
    /// `rx_freq_hz` (CwDemod / fldigi blocks).
    #[serde(default)]
    pub tone_hz: Option<f64>,
    /// Optional sample-rate floor in Hz. When given and larger than
    /// the current source rate, the source rate is raised (the driver
    /// clamps to its rate ladder).
    #[serde(default)]
    pub span_hz: Option<f64>,
    /// DC-spike dodge ratio (fraction of the channelizer's
    /// `output_rate_hz`). **Omit it** — the daemon fills the per-driver
    /// default keyed on the bound SDR (HackRF / SDRplay 0.7, DC-tracking
    /// drivers 0), so you don't carry the table. Pass a value only to
    /// override: `0` forces no dodge; a non-zero override **must exceed
    /// 0.5**, or the spike sits inside the demodulated passband.
    #[serde(default)]
    pub offset_ratio: Option<f64>,
    /// Optional manual gain, dB (higher = louder on every driver).
    /// Applied to the source after the dodge-aware tune, folding
    /// `agc=false` in atomically so the manual value actually lands.
    /// Omit to leave gain / AGC untouched.
    #[serde(default)]
    pub gain_db: Option<f64>,
    /// Optional analog filter bandwidth, Hz. Omit to leave it alone (or
    /// to let an accompanying `sample_rate_hz` change auto-derive it
    /// from the per-driver ladder).
    #[serde(default)]
    pub bandwidth_hz: Option<f64>,
    /// Optional source sample rate, Hz. Distinct from `span_hz`: this
    /// sets the rate directly on the source. When set without
    /// `bandwidth_hz`, the bandwidth is auto-derived from the driver's
    /// rate→BW ladder so the IF filter tracks the new rate.
    #[serde(default)]
    pub sample_rate_hz: Option<f64>,
    /// Stay-in-span retune (opt-in). When the target is already inside the
    /// current digitised span, retune the channelizer only and leave the LO
    /// and graph in place — no source restart, so a node-side worker (e.g.
    /// whisper) doesn't reload and a wideband survey's centre stays fixed
    /// while you hop stations. Falls back to a normal LO-moving tune if the
    /// target is outside the span. Ideal for the `signals` → hop → listen loop.
    #[serde(default)]
    pub keep_lo: bool,
    /// Optional "why" note surfaced in the UI's activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BlockParamsArgs {
    /// Block id in the composed flowgraph (e.g. `src`, `chan`,
    /// `demod`, `audio_nr`). `list_blocks` (via `status` →
    /// `pipeline.blocks` or `GET /api/pipeline/blocks`) enumerates.
    pub block: String,
    /// JSON object of `{ param_key: new_value }` to PATCH onto the
    /// block. Live-tunable keys hot-apply; others rebuild the block.
    pub params: Value,
    /// Optional "why" note surfaced in the UI's activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TranscribeArgs {
    /// `true` enables the VoiceTranscribe tap; `false` disables it.
    /// The pipeline rebuilds to splice the tap in/out.
    pub enabled: bool,
    /// Where transcription runs. `"node"` is **headless**: it sets the
    /// profile's audio split to `server`, pulling the whole audio chain
    /// (demod → resample → NR → transcribe) into `ferrited` so whisper.cpp
    /// runs with no browser — read results with `decodes(kind="transcribe")`
    /// or `await_events(kind="transcribe")`. `"browser"` sets the split to `browser`
    /// (in-browser whisper, needs a UI tab). Omit to leave the current
    /// split unchanged.
    #[serde(default)]
    pub placement: Option<String>,
    /// Optional "why" note surfaced in the UI's activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ReferenceImageArgs {
    /// Preset name (`cw`, `ft8`, `pager`, … — see `list_presets`).
    pub preset: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SigwikiIdentifyArgs {
    /// The signal's frequency, Hz (a `spectrum` peak's `freq_hz`).
    pub freq_hz: f64,
    /// Its occupied bandwidth, Hz (`spectrum`'s `width_hz`, or read off
    /// a `spectrogram`).
    #[serde(default)]
    pub bandwidth_hz: Option<f64>,
    /// How it's received: `CW`, `USB`, `LSB`, `AM`, `NFM`, `WFM`, …
    /// (`keyed:true` from `spectrum` → `CW`).
    #[serde(default)]
    pub mode: Option<String>,
    /// What you saw: `OOK` (keyed carrier), `FSK`, `MFSK`, `PSK`,
    /// `AFSK`, `FM`, `OFDM`, `chirp`, `pulse`, …
    #[serde(default)]
    pub modulation: Option<String>,
    /// Candidates to return (default 10).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SigwikiSearchArgs {
    /// Words that must all appear (title, description, mode, modulation,
    /// location, categories): `olivia`, `russian radar`, `pager`.
    pub query: String,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SigwikiPageArgs {
    /// Signal page title (from `sigwiki_identify` / `sigwiki_search`).
    pub title: String,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SpotMapArgs {
    /// `ft8` (default), `ft4` or `wspr`.
    #[serde(default)]
    pub kind: Option<String>,
    /// How far back, minutes (default 30).
    #[serde(default)]
    pub minutes: Option<f64>,
    /// The receive station's Maidenhead grid (default: the daemon's
    /// `FERRITE_RX_GRID`). Gives distances + bearings.
    #[serde(default)]
    pub rx_grid: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct LogsArgs {
    /// Tracing-target prefix (default `decoder`; `ferrited` for the
    /// daemon itself, `decoder::ft8`, `driver`, …; `""` for everything).
    #[serde(default)]
    pub category: Option<String>,
    /// Only entries after this unix-ms watermark (pass back `now`).
    #[serde(default)]
    pub since_ms: Option<u64>,
    /// Only the last N seconds.
    #[serde(default)]
    pub lookback_secs: Option<f64>,
    /// Keep the newest N (after the level filter).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Lowest level to keep: `ERROR`, `WARN`, `INFO` (default: all).
    #[serde(default)]
    pub min_level: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RecentDecodesArgs {
    /// Tracing-target prefix to filter on. Defaults to `decoder`
    /// (everything decoder-side). Examples: `decoder::rtl_433`,
    /// `decoder::pocsag`, `decoder::transcribe`, `decoder::ft8`.
    #[serde(default)]
    pub category: Option<String>,
    /// **Incremental polling cursor.** Return only entries strictly after
    /// this unix-ms watermark. Pass back the previous response's `now`
    /// field (which advances even through quiet stretches) to get exactly
    /// "what's new since I last looked" — no re-reading the whole buffer.
    /// Wins over `lookback_secs` when both are set.
    #[serde(default)]
    pub since_ms: Option<u64>,
    /// Relative window: only entries from the last N seconds. Convenient
    /// for a one-shot "what landed recently?"; for repeated polling prefer
    /// `since_ms`. Default (with no `since_ms`) is "all retained".
    #[serde(default)]
    pub lookback_secs: Option<f64>,
    /// `tail` — keep only the newest `limit` entries (0 / unset = no cap).
    /// Applied after the time filter, so `limit: 5` is "the last 5 lines".
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SetFlowgraphArgs {
    /// A complete flowgraph document (the shape `get_flowgraph` returns:
    /// `{name, blocks:{id:{type, params}}, wires:[[from, to]], …}`). The
    /// daemon validates it (port types, wiring) before swapping it in.
    pub doc: Value,
    /// Optional "why" note for the activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SwapBlockArgs {
    /// Block id in the running preset (e.g. `morse`, `demod`).
    pub block: String,
    /// New block type (see `list_block_types`). Its port names must
    /// match the old block's wiring or the daemon rejects the doc.
    #[serde(rename = "type")]
    pub type_name: String,
    /// Params for the new block (replaces the old block's params).
    #[serde(default)]
    pub params: Option<Map<String, Value>>,
    /// Optional "why" note for the activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SavePresetArgs {
    /// Name to save under (`[A-Za-z0-9_-]+`; can't be a shipped preset's).
    pub name: String,
    /// Label shown in the preset list (default: the current one + "(saved)").
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Replace an existing saved preset of that name.
    #[serde(default)]
    pub overwrite: bool,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SnapshotArgs {
    /// Seconds of IQ to record (default 3, 0.5–15).
    #[serde(default)]
    pub secs: Option<f64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SpectrogramArgs {
    /// Analyse this kept snapshot (from `snapshot`); omit to take a fresh one.
    #[serde(default)]
    pub snapshot: Option<String>,
    /// Seconds for a fresh snapshot (default 3).
    #[serde(default)]
    pub secs: Option<f64>,
    pub lo_hz: f64,
    pub hi_hz: f64,
    /// Hz per column (default 10).
    #[serde(default)]
    pub res_hz: Option<f64>,
    /// ms per row (default 20).
    #[serde(default)]
    pub step_ms: Option<f64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct EnvelopeArgs {
    #[serde(default)]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub secs: Option<f64>,
    /// Signal frequency, Hz.
    pub freq_hz: f64,
    /// Bandwidth around it (default 200 Hz).
    #[serde(default)]
    pub bw_hz: Option<f64>,
    /// Integration window, ms (default 10).
    #[serde(default)]
    pub window_ms: Option<f64>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SpectrumArgs {
    /// Analyse this kept snapshot (from `snapshot`); omit to take a fresh one.
    #[serde(default)]
    pub snapshot: Option<String>,
    /// Range start, Hz (≤ 400 kHz wide, inside the radio's current span).
    pub lo_hz: f64,
    /// Range end, Hz.
    pub hi_hz: f64,
    /// Seconds of IQ to analyse (default 3, 0.5–15). Longer catches more
    /// of an intermittent signal — a beacon slot, a QSO's overs.
    #[serde(default)]
    pub secs: Option<f64>,
    /// Target resolution, Hz/bin (default 10).
    #[serde(default)]
    pub res_hz: Option<f64>,
    /// Peaks to return, strongest first (default 25).
    #[serde(default)]
    pub max_peaks: Option<usize>,
    /// Points in the max-hold profile (default 120; 0 = none).
    #[serde(default)]
    pub profile_points: Option<usize>,
    /// Optional "why" note for the activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SignalsArgs {
    /// Freshness gate (ms). The watchlist is reported empty if the
    /// `SignalList` block hasn't emitted within this window — i.e. the
    /// preset has no detector or detection stalled. Default 2000. Set `0`
    /// to disable the gate and return the last retained emission at any
    /// age (useful right after stopping the pipeline).
    #[serde(default)]
    pub max_age_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DecodesArgs {
    /// Decoder kind to fetch — `aprs`, `adsb`, `ais`, `ft8`, `wspr`,
    /// `pager`, `transcribe`, `signals`, `rds`, … Omit for the whole
    /// store (every kind at once).
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AwaitArgs {
    /// What to wait for, as a store kind or topic subtree under
    /// `ferrite/state/`: `adsb`, `aprs`, `pager`, `ft8`, `transcribe`,
    /// `signals`, `graph`, `activity`, `jobs`, `log/decoder` (every
    /// decoder log line), `log/decoder/pocsag`, … Keyed kinds also
    /// deliver their per-entity topics.
    pub kind: String,
    /// Return as soon as this many records have arrived (default 1).
    #[serde(default)]
    pub max: Option<usize>,
    /// Give up after this many seconds (default 30, max 300). A timeout is
    /// not an error: the response says `timed_out: true` with whatever
    /// arrived.
    #[serde(default)]
    pub timeout_s: Option<f64>,
    /// Only records with a store `seq` strictly greater than this — pass
    /// the last `seq` you saw to skip anything you already have.
    #[serde(default)]
    pub since_seq: Option<u64>,
    /// Also return the retained record (replace kinds: the current value;
    /// keyed kinds: every live entity) before waiting for new ones.
    /// Default false: only what arrives *after* you subscribe.
    #[serde(default)]
    pub include_retained: Option<bool>,
    /// Only records whose `record.data` matches every field here: a
    /// scalar must be equal, an array means "any of". E.g. wait for a
    /// capture to finish: `kind: "jobs/cap-3", include_retained: true,
    /// filter: {"status": ["done", "failed", "interrupted"]}`; or only
    /// APRS from one station: `kind: "aprs", filter: {"call": "WR6ABD"}`.
    #[serde(default)]
    pub filter: Option<serde_json::Map<String, Value>>,
}

/// Does this event-plane payload's `record.data` satisfy `filter`?
/// Records without data (resets, expiries) never match a non-empty one.
#[must_use]
pub fn record_matches(value: &Value, filter: &serde_json::Map<String, Value>) -> bool {
    if filter.is_empty() {
        return true;
    }
    let Some(data) = value.pointer("/record/data") else {
        return false;
    };
    filter.iter().all(|(k, want)| {
        let got = data.get(k);
        match want {
            Value::Array(any) => got.is_some_and(|g| any.contains(g)),
            w => got == Some(w),
        }
    })
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DecodesResetArgs {
    /// Decoder kind to clear (e.g. `adsb`, `aprs`, `ft8`). Drops its
    /// accumulated records (log + keyed table) and bumps the store seq so
    /// mirrors clear too.
    pub kind: String,
    /// Optional "why" note surfaced in the UI's activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PresetDetailArgs {
    /// Preset basename — matches `flowgraphs/<name>.json` on the
    /// daemon. Use `list_presets` to see what's loadable.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BandAtArgs {
    /// Frequency in Hz to look up in the US allocation chart.
    pub freq_hz: f64,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ViewSnapshotArgs {
    /// Which canvas to snapshot: `wide-spectrum`, `wide-waterfall`,
    /// `channel-spectrum`, or `channel-waterfall`. The `channel-*`
    /// panes are only meaningful when the active preset has a
    /// Channelizer.
    pub pane: String,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct StartCaptureArgs {
    /// Capture duration in seconds. The block's wall-clock cap inside
    /// ferrited stops the recording when this elapses; the job task
    /// also belt-and-braces clears `record_path` 0.5 s past `duration`
    /// to handle a missed tick.
    pub duration_s: f64,
    /// Optional retune target. When given, the task retunes through
    /// `/api/tune` (dodge ratio 0) first so the capture lands at the
    /// requested freq. Leave unset to capture wherever the pipeline is
    /// already tuned.
    #[serde(default)]
    pub freq_hz: Option<f64>,
    /// Output path. Defaults to
    /// `/tmp/ferrite-captures/<kind>-<unix_ms>-<freq>.<ext>` matching
    /// the CLI's convention (`cf32` for IQ, `bin` for FFT bytes).
    #[serde(default)]
    pub out: Option<String>,
    /// IQ only. `false` (default) = non-disruptive live narrowband
    /// record: tees the post-channelizer cf32 stream while the UI keeps
    /// running. `true` = full-rate wideband: swaps in the `capture_fm`
    /// preset for a `Source → FileIqSink` slice straight off the SDR
    /// (clobbers the live session). Wideband needs `freq_hz`.
    #[serde(default)]
    pub wideband: bool,
    /// Wideband IQ only. Source sample rate, Hz. Defaults to 2.0e6.
    #[serde(default)]
    pub sample_rate_hz: Option<f64>,
    /// Wideband IQ only. Analog bandwidth, Hz. Defaults to the rate.
    #[serde(default)]
    pub bandwidth_hz: Option<f64>,
    /// Wideband IQ only. Front-end gain, dB.
    #[serde(default)]
    pub gain_db: Option<f64>,
    /// Wideband IQ only. `cf32` (default) or `wav-s16`.
    #[serde(default)]
    pub format: Option<String>,
    /// Wideband IQ only. Proceed even when an active transcription session
    /// would be torn down by the preset swap (default false = refuse).
    /// Ignored by the non-disruptive live narrowband path.
    #[serde(default)]
    pub force: bool,
    /// Optional "why" note surfaced in the UI's activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct StartCaptureAudioArgs {
    /// Centre frequency in Hz. Optional: omitted = record what the
    /// operator is hearing right now (the live audio tee, no retune, no
    /// preset swap). Given = retune to it first (still the live tee).
    #[serde(default)]
    pub freq_hz: Option<f64>,
    /// Capture duration in seconds. Default 10.
    #[serde(default)]
    pub duration_s: Option<f64>,
    /// Source sample rate, Hz. Default 2.4e6.
    #[serde(default)]
    pub sample_rate_hz: Option<f64>,
    /// Analog bandwidth, Hz. Defaults to the sample rate.
    #[serde(default)]
    pub bandwidth_hz: Option<f64>,
    /// Front-end gain, dB.
    #[serde(default)]
    pub gain_db: Option<f64>,
    /// Recording preset — must end in a `FileAudioSink`-shaped block.
    /// Default `fm-audio-record`; others: `am-audio-record`,
    /// `capture-aprs`, `capture-pager`.
    #[serde(default)]
    pub preset: Option<String>,
    /// Output path. Defaults to
    /// `/tmp/ferrite-captures/audio-<unix_ms>-<freq>mhz.wav`.
    #[serde(default)]
    pub out: Option<String>,
    /// Proceed even when an active transcription session would be torn
    /// down by the recording-preset swap (default false = refuse with a
    /// clear error, so a live transcribe graph isn't silently dropped).
    #[serde(default)]
    pub force: bool,
    /// Optional "why" note surfaced in the UI's activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReplayCaptureArgs {
    /// Path of the sample to replay — the absolute server `path` (or the
    /// `samples/`-relative `rel`) from a `list_captures` entry. Arbitrary
    /// server paths work too; supply `kind` / `rate_hz_hint` for raw
    /// clips with no sidecar.
    pub path: String,
    /// `iq` (upmixed verbatim) or `audio` (re-modulated onto a carrier).
    /// Defaults to the capture entry's kind, else `audio`.
    #[serde(default)]
    pub kind: Option<String>,
    /// For `audio` replay: how to modulate the clip onto RF (`fm`, `am`,
    /// `usb`, `lsb`, `ssb`). Defaults to the entry's recorded modulation,
    /// else `fm`. Ignored for `iq`.
    #[serde(default)]
    pub modulation: Option<String>,
    /// Source sample-rate hint, Hz. Defaults to the entry's
    /// `sample_rate_hz`. 0 = let the block read it from the sidecar.
    #[serde(default)]
    pub rate_hz_hint: Option<f64>,
    /// Centre frequency the replay should claim, Hz (drives the band
    /// label + waterfall axis). Defaults to the entry's `center_freq_hz`.
    #[serde(default)]
    pub center_freq_hz: Option<f64>,
    /// Loop the clip when it reaches the end (default `true`) so a short
    /// sample keeps feeding the chain for the length of a test.
    #[serde(default)]
    pub loop_playback: Option<bool>,
    /// Optional "why" note surfaced in the UI's activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SetViewStateArgs {
    /// Set the operator's Main pane: `"wide"` for the
    /// FFT/Waterfall column, `"advanced"` for the per-preset advanced
    /// view (FT8 / ADS-B / APRS map, Transcript, …). Unset = leave alone.
    #[serde(default)]
    pub main_pane: Option<String>,
    /// Show/hide the channel-detail pane (the narrow FFT + waterfall
    /// column alongside whatever Main pane is up). Unset = leave alone.
    #[serde(default)]
    pub channel_detail_visible: Option<bool>,
    /// Reserved for future use (selecting the left-panel tab). Server
    /// currently routes this through to the browser but the browser
    /// hasn't been wired to act on it yet.
    #[serde(default)]
    pub left_tab: Option<String>,
    /// Optional "why" note surfaced in the UI's activity transcript.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ChatPostArgs {
    /// Which side of the conversation this turn is: `user` (mirror an
    /// operator/agent prompt), `assistant` (a reply — auto-closed so the
    /// streaming spinner resolves), or `tool` (a tool-call card inside
    /// the currently-open assistant turn).
    pub role: String,
    /// Message text. Required for `user` / `assistant`; ignored for
    /// `tool` (use the `tool_*` fields).
    #[serde(default)]
    pub text: Option<String>,
    /// `tool` role only: the tool name shown on the card (e.g. `tune`).
    #[serde(default)]
    pub tool_name: Option<String>,
    /// `tool` role only: the tool input, rendered under the card.
    #[serde(default)]
    pub tool_input: Option<String>,
    /// `tool` role only: the tool result text, if any.
    #[serde(default)]
    pub tool_result: Option<String>,
    /// The driving session's id (e.g. Claude Code's `session_id`), so
    /// every panel groups this conversation's turns together. Default:
    /// one shared `headless` session.
    #[serde(default)]
    pub session: Option<String>,
    /// Who is talking (shown on the panel's turns). Default `claude-code`.
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Clone)]
pub struct FerriteServer {
    http: Http,
    // `tool_router` is consumed by the `#[tool_handler]`-expanded impl
    // below; rustc's dead-code pass can't see through the macro.
    #[allow(dead_code)]
    tool_router: ToolRouter<FerriteServer>,
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Read the current source, merge a params delta (dropping `null`
/// keys), and PATCH it back — preserving `type` and untouched params.
async fn patch_source_merge(
    http: &Http,
    delta: &Value,
    cmd: &str,
    note: Option<&str>,
) -> Result<Value, McpError> {
    let current = http.get("/api/source").await?;
    let type_name = current
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("SoapySource")
        .to_string();
    let mut params = current
        .get("params")
        .cloned()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    if let Some(obj) = delta.as_object() {
        for (k, v) in obj {
            if v.is_null() {
                continue;
            }
            params.insert(k.clone(), v.clone());
        }
    }
    http.patch(
        "/api/source",
        json!({ "type": type_name, "params": Value::Object(params) }),
        cmd,
        note,
    )
    .await
}

// ─── operations layer ───────────────────────────────────────────────────
//
// One method per verb, the single source of truth shared by the MCP
// tool wrappers (below) and the human CLI (`main.rs`). Each returns the
// raw JSON the caller wants; the MCP wrappers `ok_json` it, the CLI
// pretty-prints it.

impl FerriteServer {
    pub(crate) fn from_http(http: Http) -> Self {
        Self {
            http,
            tool_router: Self::tool_router(),
        }
    }

    /// The transport, for CLI paths that stream (`await --follow`).
    pub(crate) fn http(&self) -> &Http {
        &self.http
    }

    /// One-shot world snapshot: pipeline status, source config, the full
    /// active flowgraph, its name, UI sinks, and a hoisted `ready`
    /// boolean (pipeline running and, per `health.ready`, samples flowing).
    pub(crate) async fn status_op(&self) -> Result<Value, McpError> {
        let pipeline = self.http.get("/api/pipeline").await?;
        let source = self.http.get("/api/source").await.unwrap_or(Value::Null);
        let flowgraph = self.http.get("/api/flowgraph").await.unwrap_or(Value::Null);
        let ui_sinks = self.http.get("/api/ui-sinks").await.unwrap_or(Value::Null);
        // The `health` record: radio liveness, requested-vs-actual LO /
        // rate, listen frequency, decoder/driver warnings, and `issues`
        // in plain words — check `health.ok` before trusting a result.
        let health = self
            .http
            .get("/api/state")
            .await
            .map(|snap| store_current(&snap, "health"))
            .unwrap_or(Value::Null);
        let preset = flowgraph.get("name").cloned().unwrap_or(Value::Null);
        // `health.ready` = running *and* samples flowing (a just-restarted
        // radio reads false until its first sample); fall back to the bare
        // pipeline status against an older daemon without it.
        let running = pipeline.get("status").and_then(Value::as_str) == Some("running");
        let ready = health
            .get("ready")
            .and_then(Value::as_bool)
            .map_or(running, |r| r && running);
        // Explicit hardware-vs-software flag so a blind caller can't mistake
        // a SineSource/File (a restart's fallback, or a `param src args=` that
        // never changed the type) for real RF. Authority is the server's
        // source.type; this just surfaces it at the top level.
        let source_kind = match source.get("type").and_then(Value::as_str) {
            Some("SoapySource") => "hardware",
            Some(_) => "software",
            None => "unknown",
        };
        Ok(json!({
            "pipeline": pipeline,
            "source": source,
            "source_kind": source_kind,
            "flowgraph": flowgraph,
            "preset": preset,
            "ui_sinks": ui_sinks,
            "health": health,
            "ready": ready,
        }))
    }

    pub(crate) async fn list_devices_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/devices").await
    }

    pub(crate) async fn source_capabilities_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/source/capabilities").await
    }

    /// The entire server store (`GET /api/state`) — every live kind in one
    /// payload: decoder output, `signals`, the `ai` transcript, plus the
    /// folded `readback` / `flowdiag` snapshots. The single read path; the
    /// per-domain verbs below slice the kind they need out of this.
    pub(crate) async fn state_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/state").await
    }

    /// Per-block flow diagnostics (sample throughput / process-time /
    /// ring fill) for the running pipeline, or `null` when stopped. The
    /// "are samples actually moving?" health check beyond `status.ready`.
    /// Sliced from the store's `flowdiag` kind.
    pub(crate) async fn flowdiag_op(&self) -> Result<Value, McpError> {
        Ok(store_current(&self.state_op().await?, "flowdiag"))
    }

    /// Latest driver readback (actual gain, AGC state, antenna, bandwidth
    /// as the hardware reports them) — confirms a `set_block_param` gain/AGC
    /// change really landed. `null` when stopped or non-Soapy. Sliced from
    /// the store's `readback` kind (folded by the pipeline diag tick).
    pub(crate) async fn source_readback_op(&self) -> Result<Value, McpError> {
        Ok(store_current(&self.state_op().await?, "readback"))
    }

    /// The curated `samples/` tree of replayable IQ/audio fixtures —
    /// path, kind, rate, centre freq, modulation. Feed a path to
    /// `replay_capture` to run a deterministic decode test without RF.
    pub(crate) async fn list_captures_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/captures").await
    }

    /// Point the source at a recorded sample via `ModulatedFileSource`
    /// (audio fixtures get re-modulated, IQ upmixed — so the full RX
    /// chain runs). Resolves kind/rate/centre/modulation from the
    /// matching `list_captures` entry; explicit args override. Triggers a
    /// source restart, same as `select_device`.
    pub(crate) async fn replay_capture_op(
        &self,
        args: ReplayCaptureArgs,
    ) -> Result<Value, McpError> {
        // Look up the fixture so we can default the metadata from its
        // sidecar (matches the web SamplePicker). Match on absolute path
        // or the samples-relative `rel` for convenience.
        let captures = self.http.get("/api/captures").await.unwrap_or(Value::Null);
        let entry = captures.as_array().and_then(|arr| {
            arr.iter().find(|e| {
                let p = e.get("path").and_then(Value::as_str);
                let rel = e.get("rel").and_then(Value::as_str);
                p == Some(args.path.as_str()) || rel == Some(args.path.as_str())
            })
        });
        let field = |k: &str| entry.and_then(|e| e.get(k)).cloned();
        let entry_str = |k: &str| field(k).and_then(|v| v.as_str().map(str::to_owned));
        let entry_f64 = |k: &str| field(k).and_then(|v| v.as_f64());

        // Prefer the entry's absolute path when matched by `rel`.
        let path = entry_str("path").unwrap_or_else(|| args.path.clone());
        let kind = args
            .kind
            .or_else(|| entry_str("kind"))
            .unwrap_or_else(|| "audio".into());
        let modulation = args
            .modulation
            .or_else(|| entry_str("modulation"))
            .unwrap_or_else(|| "fm".into());
        let rate = args
            .rate_hz_hint
            .or_else(|| entry_f64("sample_rate_hz"))
            .unwrap_or(0.0);
        let center = args
            .center_freq_hz
            .or_else(|| entry_f64("center_freq_hz"))
            .unwrap_or(0.0);
        let loop_playback = args.loop_playback.unwrap_or(true);
        // The block takes `ssb` + a `sideband`; the verb also accepts the
        // sideband itself (`usb` / `lsb`), which it used to pass through
        // verbatim and the block rejected at start.
        let (modulation, sideband) = match modulation.to_ascii_lowercase().as_str() {
            "usb" => ("ssb".to_string(), Some("usb")),
            "lsb" => ("ssb".to_string(), Some("lsb")),
            other => (other.to_string(), None),
        };

        // Only the keys we resolve; the rest (offset_hz, deviation_hz,
        // mod_depth, output_rate_hz) fall back to the block's
        // `#[serde(default)]`.
        let mut params = json!({
            "path": path,
            "kind": kind,
            "modulation": modulation,
            "rate_hz_hint": rate,
            "center_freq_hz": center,
            "loop_playback": loop_playback,
        });
        if let Some(sb) = sideband {
            params["sideband"] = json!(sb);
        }
        self.http
            .patch(
                "/api/source",
                json!({ "type": "ModulatedFileSource", "params": params }),
                "replay-capture",
                args.note.as_deref(),
            )
            .await
    }

    pub(crate) async fn list_blocks_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/pipeline/blocks").await
    }

    pub(crate) async fn list_block_types_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/blocks").await
    }

    pub(crate) async fn reload_drivers_op(&self) -> Result<Value, McpError> {
        self.http
            .post("/api/devices/reload", json!({}), "device-reload", None)
            .await
    }

    pub(crate) async fn select_device_op(&self, args: SelectDeviceArgs) -> Result<Value, McpError> {
        // Send only the args; the server fills driver-appropriate
        // defaults. Cross-source-type param carryover is a footgun (a
        // SineSource's 100 MHz centre would mis-tune the new SDR), so we
        // deliberately do NOT preserve the previous source's params.
        self.http
            .patch(
                "/api/source",
                json!({ "type": "SoapySource", "params": { "args": args.args } }),
                "device-select",
                args.note.as_deref(),
            )
            .await
    }

    pub(crate) async fn list_presets_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/presets").await
    }

    pub(crate) async fn preset_detail_op(&self, args: PresetDetailArgs) -> Result<Value, McpError> {
        self.http.get(&format!("/api/presets/{}", args.name)).await
    }

    pub(crate) async fn band_at_op(&self, args: BandAtArgs) -> Result<Value, McpError> {
        self.http
            .get(&format!("/api/band-plan/at?hz={}", args.freq_hz))
            .await
    }

    /// Current strongest-signal watchlist — the live ranked list from the
    /// `SignalList` block at the wideband FFT, not a history. Each row's
    /// `freq_hz` feeds straight into `tune`. Sliced from the store's
    /// `signals` kind; `max_age_ms` (default 2000) gates on the record's
    /// `at_ms` so a stale list (pipeline stopped / no `SignalList`) reports
    /// empty rather than frozen. `max_age_ms=0` disables the gate.
    pub(crate) async fn spectrum_op(&self, args: SpectrumArgs) -> Result<Value, McpError> {
        let secs = args.secs.unwrap_or(3.0).clamp(0.5, 15.0);
        self.http
            .post_slow(
                "/api/analysis/spectrum",
                json!({
                    "snapshot": args.snapshot,
                    "lo_hz": args.lo_hz,
                    "hi_hz": args.hi_hz,
                    "secs": secs,
                    "res_hz": args.res_hz,
                    "max_peaks": args.max_peaks,
                    "profile_points": args.profile_points,
                }),
                "spectrum",
                args.note.as_deref(),
                Duration::from_secs_f64(secs + 30.0),
            )
            .await
    }

    pub(crate) async fn snapshot_op(&self, args: SnapshotArgs) -> Result<Value, McpError> {
        let secs = args.secs.unwrap_or(3.0).clamp(0.5, 15.0);
        self.http
            .post_slow(
                "/api/analysis/snapshot",
                json!({ "secs": secs }),
                "snapshot",
                None,
                Duration::from_secs_f64(secs + 30.0),
            )
            .await
    }

    /// Spectrogram; the reply drops the raw pixels (`cells_b64` — the UI
    /// draws them from the `analysis` record) and keeps the text view.
    pub(crate) async fn spectrogram_op(&self, args: SpectrogramArgs) -> Result<Value, McpError> {
        let secs = args.secs.unwrap_or(3.0).clamp(0.5, 15.0);
        let mut v = self
            .http
            .post_slow(
                "/api/analysis/spectrogram",
                json!({
                    "snapshot": args.snapshot, "secs": secs,
                    "lo_hz": args.lo_hz, "hi_hz": args.hi_hz,
                    "res_hz": args.res_hz, "step_ms": args.step_ms,
                }),
                "spectrogram",
                None,
                Duration::from_secs_f64(secs + 60.0),
            )
            .await?;
        if let Some(o) = v.as_object_mut() {
            o.remove("cells_b64");
        }
        Ok(v)
    }

    /// Envelope; the reply drops the per-window power series (the UI
    /// draws it) and keeps segments + the text strip.
    pub(crate) async fn envelope_op(&self, args: EnvelopeArgs) -> Result<Value, McpError> {
        let secs = args.secs.unwrap_or(3.0).clamp(0.5, 15.0);
        let mut v = self
            .http
            .post_slow(
                "/api/analysis/envelope",
                json!({
                    "snapshot": args.snapshot, "secs": secs,
                    "freq_hz": args.freq_hz, "bw_hz": args.bw_hz, "window_ms": args.window_ms,
                }),
                "envelope",
                None,
                Duration::from_secs_f64(secs + 60.0),
            )
            .await?;
        if let Some(r) = v.get_mut("result").and_then(Value::as_object_mut) {
            r.remove("power_db");
            if let Some(Value::Array(seg)) = r.get_mut("segments") {
                seg.truncate(300);
            }
        }
        Ok(v)
    }

    pub(crate) async fn signals_op(&self, args: SignalsArgs) -> Result<Value, McpError> {
        let snapshot = self.state_op().await?;
        let rec = snapshot
            .get("kinds")
            .and_then(|k| k.get("signals"))
            .and_then(|k| k.get("current"))
            .and_then(|c| c.get(STORE_REPLACE_KEY));
        let max_age = args.max_age_ms.unwrap_or(2000);
        if let Some(rec) = rec {
            if max_age > 0 {
                let at = rec.get("at_ms").and_then(Value::as_u64).unwrap_or(0);
                if now_ms().saturating_sub(u128::from(at)) > u128::from(max_age) {
                    return Ok(json!({ "signals": [], "stale": true }));
                }
            }
            return Ok(rec.get("data").cloned().unwrap_or(Value::Null));
        }
        Ok(json!({ "signals": [] }))
    }

    /// The whole store, or one kind (`kinds.<kind>`) when `kind` is set.
    pub(crate) async fn decodes_op(&self, args: DecodesArgs) -> Result<Value, McpError> {
        let snapshot = self.state_op().await?;
        match args.kind {
            Some(k) => Ok(snapshot
                .get("kinds")
                .and_then(|m| m.get(&k))
                .cloned()
                .unwrap_or(Value::Null)),
            None => Ok(snapshot),
        }
    }

    /// Block until `max` records arrive on the event plane for `kind`
    /// (or the timeout). The subscription form of `decodes`: an agent
    /// says "wake me when the next ADS-B frame / pager message /
    /// transcript segment lands" instead of polling.
    pub(crate) async fn await_op(&self, args: AwaitArgs) -> Result<Value, McpError> {
        let max = args.max.unwrap_or(1).max(1);
        let timeout = Duration::from_secs_f64(args.timeout_s.unwrap_or(30.0).clamp(0.0, 300.0));
        let since = args.since_seq.unwrap_or(0);
        let include_retained = args.include_retained.unwrap_or(false);
        let filter = args.filter.clone().unwrap_or_default();
        let started = std::time::Instant::now();
        let mut records = Vec::new();
        let timed_out = self
            .http
            .subscribe_records(&args.kind, timeout, |rec| {
                if rec.retain && !include_retained {
                    return true;
                }
                if rec.seq.is_some_and(|s| s <= since) {
                    return true;
                }
                if !record_matches(&rec.value, &filter) {
                    return true;
                }
                records.push(
                    json!({ "topic": rec.topic, "retained": rec.retain, "record": rec.value }),
                );
                records.len() < max
            })
            .await?;
        Ok(json!({
            "kind": args.kind,
            "count": records.len(),
            "timed_out": timed_out,
            "waited_ms": started.elapsed().as_millis(),
            "records": records,
        }))
    }

    pub(crate) async fn decodes_reset_op(&self, args: DecodesResetArgs) -> Result<Value, McpError> {
        self.http
            .post(
                &format!("/api/state/{}/reset", args.kind),
                json!({}),
                "decodes-reset",
                args.note.as_deref(),
            )
            .await
    }

    /// The running preset doc (live params folded in).
    pub(crate) async fn get_flowgraph_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/flowgraph").await
    }

    /// Replace the running preset doc wholesale (validated server-side).
    pub(crate) async fn set_flowgraph_op(&self, args: SetFlowgraphArgs) -> Result<Value, McpError> {
        self.http
            .patch(
                "/api/flowgraph",
                args.doc,
                "flowgraph-set",
                args.note.as_deref(),
            )
            .await
    }

    /// Swap one block's type (+ params), keeping its id and wiring.
    pub(crate) async fn swap_block_op(&self, args: SwapBlockArgs) -> Result<Value, McpError> {
        let mut doc = self.get_flowgraph_op().await?;
        let block = doc
            .get_mut("blocks")
            .and_then(|b| b.get_mut(&args.block))
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                McpError::invalid_params(
                    format!("no block {:?} in the running preset", args.block),
                    None,
                )
            })?;
        let old = block.get("type").cloned().unwrap_or(Value::Null);
        block.insert("type".into(), Value::from(args.type_name.clone()));
        block.insert(
            "params".into(),
            Value::Object(args.params.clone().unwrap_or_default()),
        );
        let reply = self
            .http
            .patch(
                "/api/flowgraph",
                doc,
                "flowgraph-swap",
                Some(args.note.as_deref().unwrap_or(&format!(
                    "{}: {} → {}",
                    args.block,
                    old.as_str().unwrap_or("?"),
                    args.type_name
                ))),
            )
            .await?;
        Ok(json!({ "block": args.block, "from": old, "to": args.type_name, "reconfigure": reply }))
    }

    /// Save the running graph as a user preset.
    pub(crate) async fn save_preset_op(&self, args: SavePresetArgs) -> Result<Value, McpError> {
        self.http
            .post(
                &format!("/api/presets/{}", args.name),
                json!({ "label": args.label, "description": args.description, "overwrite": args.overwrite }),
                "preset-save",
                None,
            )
            .await
    }

    pub(crate) async fn load_preset_op(&self, args: LoadPresetArgs) -> Result<Value, McpError> {
        self.http
            .post(
                "/api/preset",
                json!({ "name": args.name }),
                "preset-load",
                args.note.as_deref(),
            )
            .await
    }

    /// Dodge-aware tune (POST /api/tune) plus, when supplied, a source
    /// PATCH folding in `gain_db` (+ `agc=false`), `bandwidth_hz`, and
    /// `sample_rate_hz`. The freq itself rides the dodge path, so the
    /// source PATCH never touches `center_freq_hz`.
    ///
    /// Bandwidth + SDRplay broadcast notches are now derived **server-side**
    /// at the `patch_source` choke point (see ferrited's `source_policy`),
    /// so every path — including a bare `--span` rate bump that never sets
    /// `sample_rate_hz` here — gets the right anti-alias BW and notch state.
    /// The client-side BW derive below is kept as harmless, idempotent
    /// belt-and-suspenders for the explicit `--sample-rate` path.
    pub(crate) async fn tune_op(&self, args: TuneArgs) -> Result<Value, McpError> {
        let note = args.note.as_deref();
        let tune = self
            .http
            .post(
                "/api/tune",
                json!({
                    "freq_hz": args.freq_hz,
                    "carrier_hz": args.carrier_hz,
                    "tone_hz": args.tone_hz,
                    "span_hz": args.span_hz,
                    // Pass the Option through: `null` lets the daemon apply
                    // the per-driver dodge default; `Some` overrides it.
                    "offset_ratio": args.offset_ratio,
                    "keep_lo": args.keep_lo,
                }),
                "tune",
                note,
            )
            .await?;

        if args.gain_db.is_none() && args.bandwidth_hz.is_none() && args.sample_rate_hz.is_none() {
            return Ok(json!({ "tune": tune, "source": Value::Null }));
        }

        // Build the source delta. The daemon derives the anti-alias
        // bandwidth from the rate (`server/src/source_policy.rs`) when we
        // send `sample_rate_hz` without an explicit `bandwidth_hz`, so the
        // CLI no longer carries an IF-filter ladder of its own.
        let mut delta = Map::new();
        if let Some(r) = args.sample_rate_hz {
            delta.insert("sample_rate_hz".into(), json!(r));
        }
        if let Some(b) = args.bandwidth_hz {
            delta.insert("bandwidth_hz".into(), json!(b));
        }
        if let Some(g) = args.gain_db {
            delta.insert("gain_db".into(), json!(g));
            // Manual gain intent — fold AGC off so SDRplay et al. stop
            // ignoring the IFGR write.
            delta.insert("agc".into(), json!(false));
        }
        let source = patch_source_merge(&self.http, &Value::Object(delta), "tune", note).await?;
        Ok(json!({ "tune": tune, "source": source }))
    }

    pub(crate) async fn set_block_param_op(
        &self,
        args: BlockParamsArgs,
    ) -> Result<Value, McpError> {
        self.http
            .post(
                &format!("/api/pipeline/blocks/{}/params", args.block),
                args.params,
                "param",
                args.note.as_deref(),
            )
            .await
    }

    pub(crate) async fn start_op(&self) -> Result<Value, McpError> {
        self.http
            .post("/api/pipeline/start", json!({}), "pipeline-start", None)
            .await
    }

    pub(crate) async fn stop_op(&self) -> Result<Value, McpError> {
        self.http
            .post("/api/pipeline/stop", json!({}), "pipeline-stop", None)
            .await
    }

    pub(crate) async fn transcribe_op(&self, args: TranscribeArgs) -> Result<Value, McpError> {
        // GET /api/profile, merge the transcribe flag (forcing audio on
        // when enabling — the tap rides the audio chain), set the
        // placement when asked, PATCH back.
        let current = self.http.get("/api/profile").await.unwrap_or(Value::Null);
        let mut profile = current.as_object().cloned().unwrap_or_default();
        profile.insert("transcribe".into(), Value::Bool(args.enabled));
        if args.enabled {
            profile.insert("audio".into(), Value::Bool(true));
        }
        // The audio split chooses headless (server) vs in-browser. Map
        // the verb's node/browser to the split's server/browser. Only
        // touch it when given, so we don't clobber a split the operator
        // already picked.
        if let Some(p) = args.placement.as_deref() {
            let split = match p {
                "node" => "server",
                "browser" => "browser",
                other => {
                    return Err(McpError::invalid_params(
                        format!("placement must be \"node\" or \"browser\", got {other:?}"),
                        None,
                    ))
                }
            };
            profile.insert("audio_split".into(), Value::String(split.to_string()));
        }
        self.http
            .patch(
                "/api/profile",
                Value::Object(profile),
                "transcribe",
                args.note.as_deref(),
            )
            .await
    }

    pub(crate) async fn recent_decodes_op(
        &self,
        args: RecentDecodesArgs,
    ) -> Result<Value, McpError> {
        let mut q = vec![format!(
            "category={}",
            args.category.as_deref().unwrap_or("decoder")
        )];
        if let Some(since) = args.since_ms {
            q.push(format!("since={since}"));
        }
        if let Some(lb) = args.lookback_secs {
            q.push(format!("lookback={lb}"));
        }
        if let Some(lim) = args.limit {
            if lim > 0 {
                q.push(format!("limit={lim}"));
            }
        }
        self.http
            .get(&format!("/api/decoder/recent?{}", q.join("&")))
            .await
    }

    /// Snapshot a UI canvas to `/tmp/ferrite-views/<pane>-<ms>.png` and
    /// return `{path, bytes, pane}`. Validates the pane locally.
    pub(crate) async fn view_snapshot_op(&self, args: ViewSnapshotArgs) -> Result<Value, McpError> {
        const KNOWN_PANES: &[&str] = &[
            "wide-spectrum",
            "wide-waterfall",
            "channel-spectrum",
            "channel-waterfall",
        ];
        if !KNOWN_PANES.contains(&args.pane.as_str()) {
            return Err(McpError::invalid_params(
                format!(
                    "unknown pane {:?}; expected one of {KNOWN_PANES:?}",
                    args.pane
                ),
                None,
            ));
        }
        let (bytes, _) = self.view_png(&args.pane).await?;
        let dir = std::path::PathBuf::from("/tmp/ferrite-views");
        std::fs::create_dir_all(&dir)
            .map_err(|e| http_err("mkdir /tmp/ferrite-views", e.to_string()))?;
        let out_path = dir.join(format!("{}-{}.png", args.pane, now_ms()));
        std::fs::write(&out_path, &bytes)
            .map_err(|e| http_err(&format!("write {}", out_path.display()), e.to_string()))?;
        Ok(json!({
            "path": out_path.display().to_string(),
            "bytes": bytes.len(),
            "pane": args.pane,
        }))
    }

    /// One UI canvas as PNG bytes (`GET /api/ui-views/<pane>`).
    async fn view_png(&self, pane: &str) -> Result<(Vec<u8>, String), McpError> {
        self.http.get_bytes(&format!("/api/ui-views/{pane}")).await
    }

    /// `GET /api/presets/<name>` refs + the reference image bytes.
    pub(crate) async fn reference_image_op(
        &self,
        args: ReferenceImageArgs,
    ) -> Result<(Value, Vec<u8>, String), McpError> {
        let doc = self
            .http
            .get(&format!("/api/presets/{}", args.preset))
            .await?;
        let (bytes, mime) = self
            .http
            .get_bytes(&format!("/api/presets/{}/reference-image", args.preset))
            .await?;
        let info = json!({
            "preset": args.preset,
            "label": doc.get("label"),
            "signal_wiki_url": doc.get("signal_wiki_url"),
            "image": doc.get("signal_wiki_image"),
            "bytes": bytes.len(),
        });
        Ok((info, bytes, mime))
    }

    pub(crate) async fn sigwiki_identify_op(
        &self,
        args: SigwikiIdentifyArgs,
    ) -> Result<Value, McpError> {
        let mut q = vec![format!("freq_hz={}", args.freq_hz)];
        if let Some(b) = args.bandwidth_hz {
            q.push(format!("bw_hz={b}"));
        }
        if let Some(m) = &args.mode {
            q.push(format!("mode={}", urlencode(m)));
        }
        if let Some(m) = &args.modulation {
            q.push(format!("modulation={}", urlencode(m)));
        }
        if let Some(n) = args.limit {
            q.push(format!("limit={n}"));
        }
        self.http
            .get(&format!("/api/sigwiki/identify?{}", q.join("&")))
            .await
    }

    pub(crate) async fn sigwiki_search_op(
        &self,
        args: SigwikiSearchArgs,
    ) -> Result<Value, McpError> {
        let limit = args.limit.unwrap_or(20);
        self.http
            .get(&format!(
                "/api/sigwiki/search?q={}&limit={limit}",
                urlencode(&args.query)
            ))
            .await
    }

    /// The page, plus its picture when it has one (bytes, mime).
    pub(crate) async fn sigwiki_page_op(
        &self,
        args: SigwikiPageArgs,
    ) -> Result<(Value, Option<(Vec<u8>, String)>), McpError> {
        let v = self
            .http
            .get(&format!(
                "/api/sigwiki/page?title={}",
                urlencode(&args.title)
            ))
            .await?;
        let picture = v
            .pointer("/result/signal/picture")
            .and_then(Value::as_str)
            .map(str::to_string);
        let image = match picture {
            Some(f) => self
                .http
                .get_bytes(&format!(
                    "/api/sigwiki/image?file={}&width=640",
                    urlencode(&f)
                ))
                .await
                .ok(),
            None => None,
        };
        Ok((v, image))
    }

    pub(crate) async fn spot_map_op(&self, args: SpotMapArgs) -> Result<Value, McpError> {
        self.http
            .post(
                "/api/analysis/spot_map",
                json!({ "kind": args.kind, "minutes": args.minutes, "rx_grid": args.rx_grid }),
                "spot-map",
                None,
            )
            .await
    }

    pub(crate) async fn view_state_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/view").await
    }

    pub(crate) async fn set_view_state_op(
        &self,
        args: SetViewStateArgs,
    ) -> Result<Value, McpError> {
        let mut body = Map::new();
        if let Some(p) = args.main_pane {
            body.insert("main_pane".into(), Value::String(p));
        }
        if let Some(b) = args.channel_detail_visible {
            body.insert("channel_detail_visible".into(), Value::Bool(b));
        }
        if let Some(t) = args.left_tab {
            body.insert("left_tab".into(), Value::String(t));
        }
        self.http
            .post(
                "/api/ui-view/set",
                Value::Object(body),
                "set-view-state",
                args.note.as_deref(),
            )
            .await
    }

    /// Publish a headless conversation turn onto the AI session bus
    /// (`ferrite/ingest/ai` → the `ai` store kind). Every AI panel —
    /// open now or later, the log persists — renders it grouped by
    /// `session`, alongside the in-browser assistant's sessions.
    pub(crate) async fn chat_post_op(&self, args: ChatPostArgs) -> Result<Value, McpError> {
        let t = now_ms();
        let session = args.session.unwrap_or_else(|| "headless".into());
        let source = args.source.unwrap_or_else(|| "claude-code".into());
        let rec = |extra: Value| {
            let mut base = json!({ "session": session, "source": source, "t": t });
            if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
                b.extend(e.clone());
            }
            base
        };
        // The `ai` session-bus vocabulary (see docs/02-protocol.md): one
        // record per user prompt / text block / tool use / tool result /
        // turn end, grouped by `session` on every panel.
        let events: Vec<Value> = match args.role.as_str() {
            "user" => vec![rec(
                json!({ "type": "user", "text": args.text.unwrap_or_default() }),
            )],
            "assistant" => vec![
                rec(json!({ "type": "text", "text": args.text.unwrap_or_default() })),
                rec(json!({ "type": "turn_end", "status": "done" })),
            ],
            "tool" => {
                let id = format!("inj-{t}");
                let mut evs = vec![rec(json!({
                    "type": "tool_use",
                    "tool_id": id,
                    "name": args.tool_name.unwrap_or_else(|| "tool".into()),
                    "input": args.tool_input.unwrap_or_default(),
                }))];
                if let Some(result) = args.tool_result {
                    evs.push(rec(json!({
                        "type": "tool_result",
                        "tool_id": id,
                        "text": result,
                        "is_error": false,
                    })));
                }
                evs
            }
            other => {
                return Err(McpError::invalid_params(
                    format!("unknown chat role {other:?}; expected user|assistant|tool"),
                    None,
                ));
            }
        };
        let published = self.http.publish_ingest("ai", &events).await?;
        Ok(json!({ "published": published }))
    }

    pub(crate) async fn start_capture_iq_op(
        &self,
        args: StartCaptureArgs,
    ) -> Result<Value, McpError> {
        self.post_capture("/api/capture/iq", "capture-iq", &args, args.note.as_deref())
            .await
    }

    pub(crate) async fn start_capture_fft_op(
        &self,
        args: StartCaptureArgs,
    ) -> Result<Value, McpError> {
        self.post_capture(
            "/api/capture/fft",
            "capture-fft",
            &args,
            args.note.as_deref(),
        )
        .await
    }

    pub(crate) async fn start_capture_audio_op(
        &self,
        args: StartCaptureAudioArgs,
    ) -> Result<Value, McpError> {
        self.post_capture(
            "/api/capture/audio",
            "capture-audio",
            &args,
            args.note.as_deref(),
        )
        .await
    }

    /// Serialize a capture request and POST it to ferrited, which owns the
    /// job registry + the capture state machines. The `note` rides the
    /// activity header (surfaced in the UI transcript); it is harmlessly
    /// present in the body too and ignored server-side.
    async fn post_capture<T: Serialize>(
        &self,
        path: &str,
        cmd: &str,
        args: &T,
        note: Option<&str>,
    ) -> Result<Value, McpError> {
        let body = serde_json::to_value(args)
            .map_err(|e| McpError::internal_error(format!("serialize capture args: {e}"), None))?;
        self.http.post(path, body, cmd, note).await
    }
}

// ─── MCP tool wrappers ──────────────────────────────────────────────────
//
// Each is a thin `ok_json(&self.<verb>_op(args).await?)` over the ops
// layer above. The `description` strings are the AI-facing docs.

#[tool_router]
impl FerriteServer {
    #[tool(
        description = "Publish a conversation turn onto the AI session bus (the `ai` \
                       store kind) so an agent driving ferrited headlessly (CLI / MCP) \
                       shows its prompts, tool calls and replies in every AI panel \
                       alongside the in-browser assistant — live, and in the persisted \
                       history. `role`: `user` (a prompt), `assistant` (a reply — ends \
                       the turn), or `tool` (a tool card; use `tool_name` / \
                       `tool_input` / `tool_result`). `session` groups the turns (pass \
                       your session id); `source` labels them (default `claude-code`). \
                       No browser tab needed."
    )]
    async fn chat_post(
        &self,
        Parameters(args): Parameters<ChatPostArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.chat_post_op(args).await?)
    }

    #[tool(
        description = "One-shot snapshot of the running ferrited: pipeline status (running/stopped) with a hoisted `ready` boolean (true only once samples flow — false for a few seconds after any source restart; wait for it before measuring), the active source config (driver/freq/rate/gain), the full active flowgraph + its `preset` name, the set of UI sinks, and `health` — `health.ok` / `health.issues` say in plain words whether you are getting what you asked for (radio lost/recovering, preset-declared vs configured vs actual sample rate, requested vs actual LO, decoder/driver warnings such as a rate mismatch), with `health.tuning.listen_hz` the real listen frequency. Check `health.ok` before trusting an empty decode result. Cheap; call this first."
    )]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.status_op().await?)
    }

    #[tool(
        description = "List every SoapySDR device the daemon currently sees. Returns the cached capability schema (driver, label, serial, sample-rate/freq/bandwidth ranges, gains, antennas) — same payload the source dialog uses."
    )]
    async fn list_devices(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.list_devices_op().await?)
    }

    #[tool(
        description = "Capability schema of the currently-bound source — antennas (port names), Soapy settings (filters, bias-T, AGC tuners — every key the driver lets you patch via `set_block_param(block='src', params={settings: {...}})`), gain ladder, sample-rate / bandwidth / frequency ranges. Use this before swapping antennas or flipping driver-specific knobs so you know what's actually settable on the bound SDR. Tagged response: `kind='hardware'|'software'|'unavailable'`."
    )]
    async fn source_capabilities(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.source_capabilities_op().await?)
    }

    #[tool(
        description = "Per-block flow diagnostics for the running pipeline — sample throughput, process-time, and ring-buffer fill for every block, or `null` when stopped. Per-block detail behind `status.ready` (which says whether the source delivers at all): after `start`, poll this to confirm the source is delivering frames downstream before you wait on a decode."
    )]
    async fn flowdiag(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.flowdiag_op().await?)
    }

    #[tool(
        description = "Latest 1 Hz driver readback — the actual gain, AGC state, antenna, and bandwidth as the *hardware* reports them (not the values you asked for). Use to confirm a `set_block_param`/`tune` gain or AGC change really landed (SDRplay silently ignores manual IFGR while AGC is on, so the readback is how you catch it). `null` when the pipeline is stopped or the source isn't a SoapySource."
    )]
    async fn source_readback(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.source_readback_op().await?)
    }

    #[tool(
        description = "List the curated `samples/` tree of replayable IQ/audio fixtures — each entry has the server `path`, `rel`, `kind` (iq/audio), `sample_rate_hz`, `center_freq_hz`, and `modulation`. Feed a `path` to `replay_capture` to run a deterministic decode/transcribe test against a known signal with no SDR attached."
    )]
    async fn list_captures(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.list_captures_op().await?)
    }

    #[tool(
        description = "Point the source at a recorded sample (from `list_captures`) so the full RX chain runs against a known signal — no SDR needed. Routes through `ModulatedFileSource`: `audio` fixtures are re-modulated onto a carrier (`modulation` fm/am/usb/lsb), `iq` captures are upmixed verbatim. kind/rate/centre/modulation default from the matching capture entry's sidecar; pass them explicitly for a raw clip. Loops by default. Triggers a source restart — follow with `start` (and `load_preset` for the matching demod), then `await_events` on the decoder's kind (or read `decodes` / `flowdiag`). The deterministic counterpart to `select_device` + `tune` for testing decoders."
    )]
    async fn replay_capture(
        &self,
        Parameters(args): Parameters<ReplayCaptureArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.replay_capture_op(args).await?)
    }

    #[tool(
        description = "Every block in the currently-composed preset, including the resolved source and any auto-inserted bridges. Each entry has the block's id, type_name, full BlockSpec (param keys, kinds, defaults, ranges, ai_notes), current values, and placement. Use to introspect the active pipeline's topology before guessing block ids — `chan` exists in some presets, `audio_nr` in voice-mode presets, `rds` in WBFM, etc. The `src` block now surfaces the full SDR knob set (gain, agc, antenna, bandwidth_hz, dc_offset_correction). Pairs with `set_block_param(block=<id>, params={...})`."
    )]
    async fn list_blocks(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.list_blocks_op().await?)
    }

    #[tool(
        description = "Every registered block type's schema (sorted by `type_name`). Static — doesn't depend on the active preset. Each entry has the type_name, the BlockSpec (param keys, kinds, defaults, ranges, ai_notes), and the reconfig_scope (`self` / `downstream` / `sourceRestart`) so you know whether a param hot-applies or rebuilds. Use to look up the exact param key for a block before patching it (saves a guess-then-retry round)."
    )]
    async fn list_block_types(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.list_block_types_op().await?)
    }

    #[tool(
        description = "Recover from an in-process SoapySDR driver wedge (external `SoapySDRUtil --find` works but our enumerate hangs / misses devices). Tears down + re-loads every Soapy driver module via the soapysdr-sys C ABI. Refuses with 409 when the pipeline is running — call `stop` first. Service-process drivers (notably SDRplay) usually wedge on the service side, not in the module; `systemctl restart sdrplay` is the right hammer there."
    )]
    async fn reload_drivers(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.reload_drivers_op().await?)
    }

    #[tool(
        description = "Switch the active source to the SDR identified by the supplied Soapy args string (e.g. `driver=hackrf,serial=…`). Source params reset to the driver's defaults — the previous source's centre freq / rate / gain do NOT carry over (a SineSource centre of 100 MHz would have been meaningless on the SDR anyway). Always follow up with `tune` to set the listen frequency and `set_block_param` for gain / antenna / AGC. Triggers a source restart."
    )]
    async fn select_device(
        &self,
        Parameters(args): Parameters<SelectDeviceArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.select_device_op(args).await?)
    }

    #[tool(
        description = "List every preset flowgraph the daemon can load. Returned entries carry name + label + description so the AI can pick the right one (e.g. `nwr` for NOAA weather, `adsb` for aircraft, `rtl433-433` for ISM telemetry)."
    )]
    async fn list_presets(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.list_presets_op().await?)
    }

    #[tool(
        description = "Full JSON of one preset by name: `label`, `description`, `ai_notes` (the operator notes the prompt-builder embeds for the *active* preset; this gets them for any other), the block graph (which decoders, channelizer width, audio chain), `signal_wiki_url`, `signal_wiki_image`, `sample_path`. Use this when picking between candidate presets — `list_presets` gives a one-liner each, this gives the gotchas + canonical frequencies."
    )]
    async fn preset_detail(
        &self,
        Parameters(args): Parameters<PresetDetailArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.preset_detail_op(args).await?)
    }

    #[tool(
        description = "What US frequency-allocation band(s) contain `freq_hz`. Returns every matching band in the embedded `bandplan-usa.json` (Arrin Clark KN1E's CC0 plan, same chart the spectrum's ribbon overlay uses). Multiple matches are common (a primary allocation + a sub-band carving inside it); the narrowest is usually the most informative name. Returns an empty list when `freq_hz` falls outside any allocation. Use to answer 'what band am I tuned to?' without hard-coding the boundaries in prompts (e.g. 144.39 → 'Amateur 2 m', 162.55 → 'NOAA Weather Radio', 433.92 → 'ISM Region 1')."
    )]
    async fn band_at(
        &self,
        Parameters(args): Parameters<BandAtArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.band_at_op(args).await?)
    }

    #[tool(
        description = "The current strongest-signal watchlist, auto-detected at the wideband FFT by the `SignalList` block. Returns a *snapshot* (not history): `{signals:[{id, freq_hz, power_db, bw_hz, snr_db, first_seen_frame, last_seen_frame}], center_freq_hz, span_hz, frame, at_ms, now}`, ranked strongest-first. The two-step survey workflow is `signals` → pick a row → `tune` its `freq_hz`. Empty `signals` means either no signal clears the detector or the active preset has no `SignalList` (only the wideband presets wire it — e.g. `wbfm`). `at_ms` is when the list was emitted; if `now - at_ms` is large the list is stale (detection stalled). `max_age_ms` gates that staleness (default 2000)."
    )]
    async fn signals(
        &self,
        Parameters(args): Parameters<SignalsArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.signals_op(args).await?)
    }

    #[tool(
        description = "The running preset document with every live param edit folded in: `{name, label, blocks:{id:{type, params}}, wires:[[\"a.out\",\"b.in\"]], ai_notes, …}`. Read it before `swap_block` / `set_flowgraph`."
    )]
    async fn get_flowgraph(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.get_flowgraph_op().await?)
    }

    #[tool(
        description = "Replace one block in the running preset — same id, same wires — with another type and params, e.g. swap the multimon `MorseDemod` for fldigi's `CwDemod`. Port names must match the old block's wiring (the daemon validates and rejects otherwise; a rate change upstream is a separate `set_block_param`). Live: the graph rebuilds in place. Follow with `save_preset` to keep it."
    )]
    async fn swap_block(
        &self,
        Parameters(args): Parameters<SwapBlockArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.swap_block_op(args).await?)
    }

    #[tool(
        description = "Replace the running preset with a complete flowgraph document (from `get_flowgraph`, edited) — for changes `swap_block` can't express (adding blocks, rewiring). Validated server-side before it goes live."
    )]
    async fn set_flowgraph(
        &self,
        Parameters(args): Parameters<SetFlowgraphArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.set_flowgraph_op(args).await?)
    }

    #[tool(
        description = "Save the running graph (live params included) as a user preset `name` — it then appears in `list_presets` (flagged `user`) and loads with `load_preset` like a shipped one. Won't shadow a shipped preset; `overwrite` replaces an earlier save."
    )]
    async fn save_preset(
        &self,
        Parameters(args): Parameters<SavePresetArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.save_preset_op(args).await?)
    }

    #[tool(
        description = "Record a few seconds of live IQ (no retune — a tap on the source, full span on hardware) and keep it by id. Then run `spectrum` / `spectrogram` / `envelope` on the same `snapshot` so every measurement looks at the same moment of radio. The last 4 are kept. Returns `{id, center_hz, rate_hz, start_ms (UTC ms), secs, tapped}`."
    )]
    async fn snapshot(
        &self,
        Parameters(args): Parameters<SnapshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.snapshot_op(args).await?)
    }

    #[tool(
        description = "Time × frequency picture of a range (≤ 400 kHz; for a single signal use a few kHz around it). The operator sees it drawn in the AI tab; you get `text` — a character-cell spectrogram (time downward, ' .:-=+*#%@' = weak→strong) — plus `result {f0_hz, df_hz, dt_s, rows, cols, floor_dbfs}`. Read modes off it: RTTY = two alternating vertical stripes ~170 Hz apart; Olivia/MFSK = one short dash at a time hopping among evenly spaced tones; CW = dashes on one line; PSK31 = one thin steady line; FT8 = 8 tones over ~50 Hz in 13-s bursts on 15-s boundaries. Use a `snapshot` id to look at the same moment as a `spectrum`."
    )]
    async fn spectrogram(
        &self,
        Parameters(args): Parameters<SpectrogramArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.spectrogram_op(args).await?)
    }

    #[tool(
        description = "On/off power timeline at one frequency (default 200 Hz bandwidth, 10 ms windows): `result.segments` [{start_s, end_s}] from the snapshot start (`snapshot.start_ms` is UTC ms), `duty`, `swing_db`, `keyed`, and `text` — a '#'/'.' strip, 100 windows per line. Morse by eye, beacon slot timing (NCDXF: 10-s slots in a 3-min cycle), burst detection. The operator sees it drawn in the AI tab."
    )]
    async fn envelope(
        &self,
        Parameters(args): Parameters<EnvelopeArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.envelope_op(args).await?)
    }

    #[tool(
        description = "A few-Hz-resolution look at a frequency range (≤ 400 kHz, inside the current span) over `secs` of live IQ — without retuning or interrupting the session. Returns `{res_hz, floor_dbfs, frames, peaks:[{freq_hz, avg_db, peak_db, duty, width_hz, keyed}], profile:[[freq_hz, maxhold_db]], tapped}`: `freq_hz` is interpolated to a fraction of a bin; `*_db` are over the range's noise floor; `keyed` marks on/off signals (CW, beacon slots) from a 20 ms envelope and `duty` their on-fraction. Use it where `signals` is too coarse (~146 Hz bins, fixed threshold): find CW / digital carriers, then `tune(carrier_hz=freq_hz)` to land one on the decoder's tone. For an intermittent signal give `secs` long enough to cover it (NCDXF beacons: 10-s slots every 3 min)."
    )]
    async fn spectrum(
        &self,
        Parameters(args): Parameters<SpectrumArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.spectrum_op(args).await?)
    }

    #[tool(
        description = "Structured decoder output from the server-side DecoderStore — the single home for every event-emitting demod (APRS, ADS-B, AIS, FT8, WSPR, pager, transcribe, signals, RDS, …). With `kind` returns that kind as `{recent:[…], current:{…}}`: `recent` is a time-ordered append log (ft8/wspr/pager/transcribe/…); `current` is a keyed table (adsb by icao, aprs by call, ais by mmsi) or the single latest snapshot (signals/rds/rssi). Omit `kind` for the whole store `{seq, kinds:{…}}`. This is the same structured state the advanced UI views paint — captured server-side, so it works headless. Records carry `{seq, epoch, at_ms, key?, data, ctx?}`: `data` is the decoder's JSON (pager/EAS/AIS add parsed fields — address/capcode + message, SAME event + locations, mmsi + position + name — next to the raw `text`), `ctx` is where the radio was listening (`listen_hz`, `lo_hz`, `preset`). Also reads bookkeeping kinds: `jobs` (captures), `activity`, `health`. Driver/decoder warnings live in `status().health.warnings`."
    )]
    async fn decodes(
        &self,
        Parameters(args): Parameters<DecodesArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.decodes_op(args).await?)
    }

    #[tool(
        description = "Wait for the next record(s) of a kind on the event plane instead of polling. Subscribes to `ferrite/state/<kind>` on the daemon's embedded MQTT broker and returns once `max` records arrived (default 1) or `timeout_s` elapsed (default 30; `timed_out: true` with whatever came, never an error). `kind` is any store kind (`adsb`, `aprs`, `pager`, `ft8`, `transcribe`, `signals`, `activity`, `jobs`, `graph`, `status`) or a log subtree (`log/decoder`, `log/decoder/pocsag`). Each record is the same `{kind, seq, epoch, op, record:{at_ms, key?, data}}` delta `decodes` is built from; log lines are `{target, level, message, at_ms}`. Pass `since_seq` (the last `seq` you saw) to skip anything already known; `include_retained: true` to also get the current value first. Typical loop: `tune` → `await_events(kind='pager', max=5, timeout_s=60)` → read `records[*].record.data`."
    )]
    async fn await_events(
        &self,
        Parameters(args): Parameters<AwaitArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.await_op(args).await?)
    }

    #[tool(
        description = "Clear one decoder kind in the DecoderStore — drops its accumulated records (the recent log + the keyed table) and bumps the store seq so browser mirrors clear too. Use after retuning to a new band/channel so stale ADS-B aircraft / APRS stations / decode lines don't linger, or to start a clean capture. Kinds reset themselves automatically on preset load / graph rebuild; this is the explicit on-demand path."
    )]
    async fn reset_decodes(
        &self,
        Parameters(args): Parameters<DecodesResetArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.decodes_reset_op(args).await?)
    }

    #[tool(
        description = "Load a preset flowgraph by name. The daemon preserves the live source's centre freq across the swap, so a load-then-tune is two distinct steps. Match a name from `list_presets`."
    )]
    async fn load_preset(
        &self,
        Parameters(args): Parameters<LoadPresetArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.load_preset_op(args).await?)
    }

    #[tool(
        description = "Tune the radio to listen at `freq_hz`. Routes through POST /api/tune so the per-driver DC-spike dodge applies (the operator-visible listen freq stays at the requested value; the source LO may move so the spike doesn't land in the demodulated channel). The daemon owns the dodge: it picks the right ratio for the bound SDR automatically, so leave `offset_ratio` unset (pass it only to override — `0` disables the dodge). `span_hz` raises the source rate if larger than current. Optionally fold in `gain_db` (manual gain — folds `agc=false` so it lands), `bandwidth_hz`, and `sample_rate_hz` (auto-derives BW from the driver ladder when set without `bandwidth_hz`) in the same call. Set `keep_lo:true` to hop *within the current span* by retuning the channelizer only — no LO move, no graph restart (so a running transcribe worker doesn't reload and the survey centre stays put); it falls back to a normal tune when the target is outside the span. Use it for the `signals` → hop → listen loop."
    )]
    async fn tune(
        &self,
        Parameters(args): Parameters<TuneArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.tune_op(args).await?)
    }

    #[tool(
        description = "PATCH a parameter delta onto one block in the running pipeline — the **advanced/direct** control surface. `block` is the block id (e.g. `src`, `chan`, `demod`, `audio_nr`); `params` is a JSON object of key→new_value pairs. Live-tunable keys hot-apply (no rebuild); others rebuild the block. The src block aliases to PATCH /api/source — its settable keys include gain_db, gain_mode, gain_elements, agc, antenna, bandwidth_hz, dc_offset_correction, and the driver-specific `settings` map (discover via `source_capabilities`; it merges per key — `null` removes a key, a value the radio already has is skipped without a restart, and the reply's `changes` / `notes` say what happened). Unknown src keys are rejected with the valid list. Same surface the UI's <BlockParams> component uses.\n\n⚠️ PREFER THE CONVENIENCE PATHS where one exists — they apply smart composition this raw PATCH skips, so a direct param can silently misbehave:\n- **Frequency:** use the `tune` tool, NOT `param src center_freq_hz` or `param chan freq_shift_hz`. `tune` applies the per-driver DC-spike dodge (offsets the LO so the carrier doesn't land on DC) and sets the channelizer shift together; setting center_freq_hz directly parks the carrier on the DC spike on a zero-IF radio (HackRF) and it won't decode.\n- **Gain:** use the single `gain_db` auto dial (it splits across stages footgun-free on HackRF), NOT `gain_mode=manual` + `gain_elements` — reach for manual per-stage only when you specifically need it.\nOnly drop to the raw param when there's no convenience equivalent (driver `settings`, decoder knobs, etc.) or you deliberately want the un-composed behaviour."
    )]
    async fn set_block_param(
        &self,
        Parameters(args): Parameters<BlockParamsArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.set_block_param_op(args).await?)
    }

    #[tool(
        description = "Start the pipeline (instantiates blocks if it was stopped). Idempotent — calling on an already-running pipeline is a no-op."
    )]
    async fn start(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.start_op().await?)
    }

    #[tool(
        description = "Stop the running pipeline. The source device is released so a subsequent `select_device` / `reload_drivers` / preset change can claim it cleanly."
    )]
    async fn stop(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.stop_op().await?)
    }

    #[tool(
        description = "Toggle the speech-to-text tap on a voice preset. Splices a VoiceTranscribe block before the AudioSink (`transcribe` implies `audio`). `placement: \"node\"` is **headless** — whisper.cpp runs in `ferrited`, no browser needed (sets the audio split to `server`, pulling the whole audio chain node-side); `\"browser\"` runs whisper in a connected UI tab. Either way read the transcript with `decodes(kind=\"transcribe\")` or wait for lines with `await_events(kind=\"transcribe\")`. Use a *listen* preset (`wbfm`, `nbfm`, `usb`, `lsb`, `wbam`), not a headless `*-record` preset."
    )]
    async fn transcribe(
        &self,
        Parameters(args): Parameters<TranscribeArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.transcribe_op(args).await?)
    }

    #[tool(
        description = "Snapshot one of the four spectrum/waterfall canvases the browser is currently rendering and return it as an image (you see it; so does the operator, on this tool's card) — band-plan overlay, VFO marker, contrast, pause state, and zoom are all baked in (no re-render from raw FFT). Also saved under `/tmp/ferrite-views/`. Requires a UI tab connected to /ws/ui-views; 503 when none. Note: `wide-*` panes are only rendered while `main_pane=wide`; `channel-*` panes only when the active preset has a Channelizer AND `channel_detail_visible=true`. Check via `view_state` if a snapshot returns 404 (canvas not mounted)."
    )]
    async fn view_snapshot(
        &self,
        Parameters(args): Parameters<ViewSnapshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        let pane = args.pane.clone();
        let info = self.view_snapshot_op(args).await?;
        let (bytes, mime) = self.view_png(&pane).await?;
        ok_json_image(&info, &bytes, &mime)
    }

    #[tool(
        description = "The daemon's recent log lines for a tracing-target prefix (default `decoder`; e.g. `decoder::ft8`, `decoder::cw`, `ferrited` for the daemon itself) — raw decoder output and diagnostics, oldest first: `{entries:[{at_ms, target, level, message}], now}`. The ring holds the last 4096 lines (the signal list logs several a second, so it reaches back minutes, not hours). `lookback_secs` for a recent window, `since_ms` (pass back `now`) for exactly what's new, `limit` for the last N, `min_level: \"WARN\"` for just the trouble. Structured decodes are better read with `decodes` / `await_events`; this is for what the daemon *said* — errors, warnings, raw decoder text."
    )]
    async fn logs(
        &self,
        Parameters(args): Parameters<LogsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let rank = |l: &str| match l.to_ascii_uppercase().as_str() {
            "ERROR" => 0,
            "WARN" => 1,
            "INFO" => 2,
            "DEBUG" => 3,
            _ => 4,
        };
        let floor = args.min_level.as_deref().map(rank);
        let mut v = self
            .recent_decodes_op(RecentDecodesArgs {
                category: args.category,
                since_ms: args.since_ms,
                lookback_secs: args.lookback_secs,
                // The level filter runs here, so cap after it.
                limit: if floor.is_some() { None } else { args.limit },
            })
            .await?;
        if let (Some(floor), Some(entries)) =
            (floor, v.get_mut("entries").and_then(Value::as_array_mut))
        {
            entries.retain(|e| rank(e.get("level").and_then(Value::as_str).unwrap_or("")) <= floor);
            if let Some(n) = args.limit.filter(|n| *n > 0 && entries.len() > *n) {
                entries.drain(..entries.len() - n);
            }
        }
        ok_json(&v)
    }

    #[tool(
        description = "A preset's reference picture — the sigidwiki waterfall of the signal it decodes (CW dashes, FT8 tone blocks, POCSAG bursts, RTTY's two tones…) — returned as an image (you see it; the operator sees it on this tool's card), with the sigidwiki page URL. Compare against `spectrogram` / `view_snapshot` to identify a signal by eye. Presets without one: 404."
    )]
    async fn reference_image(
        &self,
        Parameters(args): Parameters<ReferenceImageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (info, bytes, mime) = self.reference_image_op(args).await?;
        ok_json_image(&info, &bytes, &mime)
    }

    #[tool(
        description = "What could this signal be? Ranks the Signal Identification Guide (sigidwiki.com, ~600 signals: amateur modes, utilities, military modems, radars, beacons, numbers stations, jammers, satellites…) against what you measured: `freq_hz` gates (in the page's range, or on one of its listed channels), then `bandwidth_hz`, `mode` and `modulation` rank — pass everything you know (from `spectrum`: `freq_hz`, `width_hz`, `keyed` → mode `CW`/modulation `OOK`; from `spectrogram`: FSK / MFSK / PSK / chirp…), and the band plan adds context (amateur bands favour amateur modes). Returns `result.candidates` [{title, score, why, freq_min/max, bw_min/max, mode, modulation, description, picture, url}] and a `text` table; the operator sees the candidates as a card. Follow up with `sigwiki_page` on the best few and compare their pictures with your `spectrogram`."
    )]
    async fn sigwiki_identify(
        &self,
        Parameters(args): Parameters<SigwikiIdentifyArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.sigwiki_identify_op(args).await?)
    }

    #[tool(
        description = "Search the Signal Identification Guide (sigidwiki.com) by words — every term must appear in a signal's title, description, mode, modulation, location or categories (`olivia`, `russian radar`, `pager`, `weather balloon`). Returns titles with frequency range, bandwidth, mode/modulation and a short description; open one with `sigwiki_page`."
    )]
    async fn sigwiki_search(
        &self,
        Parameters(args): Parameters<SigwikiSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.sigwiki_search_op(args).await?)
    }

    #[tool(
        description = "One Signal Identification Guide entry: description, frequency range / listed channels, bandwidth, mode, modulation, location, categories, the page URL — and its waterfall picture returned as an image (you see it; the operator sees it on the card, with the page's sample audio to play). Compare the picture with your `spectrogram` / `view_snapshot` to confirm an identification."
    )]
    async fn sigwiki_page(
        &self,
        Parameters(args): Parameters<SigwikiPageArgs>,
    ) -> Result<CallToolResult, McpError> {
        let (v, image) = self.sigwiki_page_op(args).await?;
        match image {
            Some((bytes, mime)) => ok_json_image(&v, &bytes, &mime),
            None => ok_json(&v),
        }
    }

    #[tool(
        description = "Where the stations an FT8 / FT4 / WSPR decoder heard are: one spot per callsign over the last `minutes` (default 30) — grid → lat/lon, distance (km) and bearing from the receive grid (`rx_grid`, default the daemon's `FERRITE_RX_GRID`), latest SNR, decode count; farthest first. The operator sees it drawn as a world map in the AI tab; you get `result.spots` and a `text` table. Needs decodes with grids — run the `ft8` / `wspr` preset for a few slots first."
    )]
    async fn spot_map(
        &self,
        Parameters(args): Parameters<SpotMapArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.spot_map_op(args).await?)
    }

    #[tool(
        description = "Read the browser's authored UI chrome state — which Main tab is active (FFT/Waterfall or the per-preset advanced view), channel-detail visibility, per-pane zoom + pause. Lets the AI tailor responses to what the operator is actually looking at, without round-tripping through a PNG. 503 when no UI tab is connected."
    )]
    async fn view_state(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.view_state_op().await?)
    }

    #[tool(
        description = "Start an async IQ capture — both shapes are live tees, so the operator's UI session keeps running. Default (`wideband=false`) tees the post-channelizer cf32 stream (block `chan`): the narrow slice the demod sees. `wideband=true` tees the raw full-span cf32 straight off the SDR (block `src`, before DC block / channelizer) at the live source rate — 8 bytes × rate per second of disk (16 MB/s at 2 MS/s); the cap is exact in samples. Optional `freq_hz` retunes first (visible to the operator, like a `tune`). `sample_rate_hz`/`bandwidth_hz`/`gain_db` are NOT applied by the wideband tee — change the live source with `set_block_param(block='src', …)` first if you need a different rate. **Returns immediately** with a `job_id`; wait for it with `await_events(kind=\"jobs/<job_id>\", include_retained=true, filter={\"status\":[\"done\",\"failed\",\"interrupted\"]})` — the record carries `output_path` and the sidecar JSON (`sample_rate_hz`, `center_freq_hz`, `samples_written`). Defaults `out` to `/tmp/ferrite-captures/iq-<unix_ms>-<freq>mhz.cf32`. Wideband needs a hardware (SoapySDR) source; against a software/test source it falls back to the old preset-swap recording only with `force:true` + `freq_hz` (session restored afterwards). Start and completion both appear on the operator's AI activity transcript."
    )]
    async fn start_capture_iq(
        &self,
        Parameters(args): Parameters<StartCaptureArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.start_capture_iq_op(args).await?)
    }

    #[tool(
        description = "Start an async FFT-byte-stream capture against the post-`logmag` u8 spectrum (block `logmag`). Non-disruptive — the UI's waterfall keeps painting while the byte stream side-tees to `output_path`. **Returns immediately** with a `job_id`; wait for it with `await_events(kind=\"jobs/<job_id>\", include_retained=true, filter={\"status\":[\"done\",\"failed\",\"interrupted\"]})` — the record carries `output_path` and the sidecar. Same live args / defaults as `start_capture_iq` (no wideband) but writes raw `u8` frame bytes (`bin`) plus a sidecar JSON — a recording for offline tools, not something to read here: to *look* at the spectrum use `snapshot` + `spectrum` / `spectrogram` (drawn in the AI tab) or `view_snapshot`."
    )]
    async fn start_capture_fft(
        &self,
        Parameters(args): Parameters<StartCaptureArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.start_capture_fft_op(args).await?)
    }

    #[tool(
        description = "Record what the operator is hearing to a 48 kHz mono WAV — a live tee of the node→browser audio bridge, so the UI session keeps running untouched (no preset swap). Omit `freq_hz` to record the current session as-is; give it to retune first (the operator's waterfall + audio follow, same as a `tune`). **Returns immediately** with a `job_id`; wait for it with `await_events(kind=\"jobs/<job_id>\", include_retained=true, filter={\"status\":[\"done\",\"failed\",\"interrupted\"]})` — the record carries `output_path` and the sidecar (`sample_rate_hz` 48000, `samples_written`, `source_rate_hz`). `duration_s` defaults to 10. Defaults `out` to `/tmp/ferrite-captures/audio-<unix_ms>-<freq>mhz.wav`. Only when the running preset has no browser audio at all (a headless `*-record` graph) does it fall back to the old preset-swap recording — that path needs `freq_hz`, refuses against a software source unless `force:true`, and restores the session when done. Start and completion both appear on the operator's AI activity transcript."
    )]
    async fn start_capture_audio(
        &self,
        Parameters(args): Parameters<StartCaptureAudioArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.start_capture_audio_op(args).await?)
    }

    #[tool(
        description = "Push a chrome-state patch to the operator's browser. Use this to put them on the right pane for what's happening: `main_pane='advanced'` switches the main column to the per-preset view (FT8 map / ADS-B map / APRS map / Transcript / fldigi console, depending on the active preset); `main_pane='wide'` switches back to the FFT/Waterfall. `channel_detail_visible` flips the narrow channel-detail column on/off. Unset fields are left alone. 503 when no UI tab is connected — the patch needs a viewer to land on."
    )]
    async fn set_view_state(
        &self,
        Parameters(args): Parameters<SetViewStateArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.set_view_state_op(args).await?)
    }
}

/// What a client learns on connect (`initialize.instructions`): what
/// Ferrite is, how to work it, and every tool by purpose. A test keeps
/// it naming every registered tool.
const INSTRUCTIONS: &str = r#"Ferrite is a software-defined radio: a running `ferrited` daemon drives a real SDR (or a recorded/test source) through a preset flowgraph of DSP blocks — source → channelizer → demod → decoders → audio/UI. These tools are its whole control and measurement surface; they wrap ferrited's REST API, act on the live session the operator is watching, and show up in the operator's AI tab (activity lines, analysis cards, images on tool cards). Nothing here needs a shell.

Orient
- `status` — first call: pipeline state + `ready`, source (driver/freq/rate/gain/antenna), active preset, `health` (radio liveness, rate/LO mismatches, decoder warnings).
- `view_state` — what the operator is looking at; `view_snapshot` — a picture of a spectrum/waterfall canvas as they see it; `set_view_state` — put them on the right pane.
- `band_at` — which allocation (ham sub-band, broadcast, aviation, …) a frequency is in.

Set up the radio
- `list_devices` → `select_device`; `source_capabilities` (antennas, gain ladder, driver settings); `source_readback` (what the hardware actually applied); `reload_drivers` (recover a wedged SoapySDR).
- `list_presets` / `preset_detail` (ai_notes, block graph, canonical frequencies) → `load_preset`; `start` / `stop`.
- `tune` — move the listen frequency (dodge-aware; `carrier_hz` lands a CW/digital carrier on the decoder's tone; also gain/bw/rate).
- `set_block_param` — any live block parameter (`src` gain/antenna/AGC/settings, demod, filters, decoders).

Find and measure signals — every result is drawn in the AI tab
- `signals` — coarse strongest-signal watchlist over the whole span (live, ~146 Hz bins).
- `snapshot` — record a few seconds of live IQ, kept by id, so several measurements look at the same moment.
- `spectrum` — Hz-accurate peaks over a ≤400 kHz range: level over the floor, width, duty, `keyed`.
- `spectrogram` — time × frequency of a range: read the mode off its shape (CW dashes, RTTY's two tones, FT8 tone blocks, PSK31's thin line).
- `envelope` — on/off timeline at one frequency: Morse elements, beacon slots, bursts.
- `reference_image` — a preset's sigidwiki reference waterfall, to compare by eye.

Identify — the Signal Identification Guide (sigidwiki.com, ~600 signals), cached locally
- `sigwiki_identify` — rank candidates for what you measured (freq, bandwidth, mode, modulation); drawn as a card.
- `sigwiki_search` — find signals by words (name, description, location).
- `sigwiki_page` — one entry: description, frequencies, bandwidth, modes, its waterfall picture inline, sample audio on the card.

Decode
- `decodes` — structured decoder output by kind (ft8, wspr, aprs, adsb, ais, pager, cw, fldigi, rds, transcribe, …); `reset_decodes` after retuning.
- `await_events` — block until the next record of a kind lands (event plane; no polling or sleeping); also `jobs/<id>` for captures.
- `spot_map` — where FT8/FT4/WSPR stations heard are: map card + distance/bearing table.
- `transcribe` — speech-to-text on a voice preset.
- `logs` — the daemon's recent log lines (raw decoder text, warnings, errors) by target.

Change the graph
- `list_blocks` (the running blocks, specs, live values) / `list_block_types` (every block type) / `get_flowgraph` → `swap_block` (replace one block in place) or `set_flowgraph` (any edit); `save_preset` keeps the result as a user preset.
- `flowdiag` — per-block throughput and ring fill: are samples actually moving?

Record and replay
- `start_capture_iq` / `start_capture_fft` / `start_capture_audio` — live-tee recordings; wait with `await_events(kind="jobs/<job_id>")`.
- `list_captures` → `replay_capture` — run the full chain on a recorded fixture (deterministic; no SDR).

Conversation
- `chat_post` — mirror a headless agent's turn into the operator's AI panel.

Habits: `status` before acting; `snapshot` once, then measure it several ways; unknown signal → `spectrum` + `spectrogram` → `sigwiki_identify` with everything you measured → `sigwiki_page` on the best candidates; tune with `carrier_hz` from a `spectrum` peak; confirm hardware changes with `source_readback`; wait with `await_events`, never sleep. Source: tools/ferrite-ctl/src/mcp.rs; REST in docs/02-protocol.md."#;

#[tool_handler]
impl ServerHandler for FerriteServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2025_03_26)
            .with_instructions(INSTRUCTIONS.to_string())
    }
}

/// Entry point — wires stdio to the tool-router and runs until the
/// client closes. Logs go to stderr; stdout is reserved for the MCP
/// JSON-RPC stream and a stray print here would corrupt the protocol.
pub async fn serve(http: Http) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    tracing::info!(base = %http.base, "ferrite-ctl mcp: serving on stdio");
    let server = FerriteServer::from_http(http);
    let service = server
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!("mcp serve: {e}"))?;
    service.waiting().await.ok();
    Ok(())
}

#[cfg(test)]
mod instructions_tests {
    use super::*;

    /// The connect-time overview names every tool, so a new tool can't
    /// ship undiscoverable.
    #[test]
    fn instructions_name_every_tool() {
        let server =
            FerriteServer::from_http(Http::new(Client::new(), "http://127.0.0.1:1".into(), None));
        let missing: Vec<String> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .filter(|n| !INSTRUCTIONS.contains(&format!("`{n}`")))
            .collect();
        assert!(
            missing.is_empty(),
            "tools missing from INSTRUCTIONS: {missing:?}"
        );
    }

    /// Shell-only tooling the in-app agent must never be pointed at: it
    /// has no shell, and the offline analysis package
    /// (`tools/analysis`, `ferrite-analyze`) is for operators / CI /
    /// Claude Code — the agent gets the same numbers from `spectrum`.
    const SHELL_ONLY: &[&str] = &[
        "`Read`",
        "Read tool",
        "Bash",
        "python",
        "fft_to_png",
        "fft_peaks",
        "ferrite-analyze",
        "ferrite_analyze",
        "tools/analysis",
    ];

    /// The agent has no shell: no tool may tell it to use one.
    #[test]
    fn no_tool_points_at_a_shell() {
        let server =
            FerriteServer::from_http(Http::new(Client::new(), "http://127.0.0.1:1".into(), None));
        for t in server.tool_router.list_all() {
            let d = t.description.as_deref().unwrap_or_default();
            for bad in SHELL_ONLY {
                assert!(!d.contains(bad), "tool {} mentions {bad}", t.name);
            }
        }
    }

    /// Nor may the server instructions (the agent's tool primer).
    #[test]
    fn instructions_never_point_at_shell_tooling() {
        for bad in SHELL_ONLY {
            assert!(!INSTRUCTIONS.contains(bad), "INSTRUCTIONS mention {bad}");
        }
    }
}
