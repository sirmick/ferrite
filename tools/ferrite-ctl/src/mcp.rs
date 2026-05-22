//! MCP (Model Context Protocol) server surface for ferrite-ctl.
//!
//! `ferrite-ctl mcp` speaks JSON-RPC over stdio. Each ferrite-ctl verb
//! that makes sense as a discrete tool is exposed as an MCP tool;
//! schemas + descriptions are derived from the request structs via
//! `schemars` (re-exported through `rmcp`). External MCP-enabled
//! clients (Claude Desktop, Claude Code CLI, the bundled ferrite-ai
//! sidecar via the Agent SDK's `mcpServers` config) can drive a
//! running `ferrited` through this surface without going via the
//! human-facing CLI.
//!
//! Implementation note: stdin/stdout are reserved for the MCP
//! transport, so every message that isn't a protocol frame must go to
//! stderr (`tracing-subscriber` with `with_writer(stderr)` below). A
//! stray `println!` here corrupts the JSON-RPC stream.

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
use serde::Deserialize;
use serde_json::{json, Value};
use tracing_subscriber::EnvFilter;

/// Thin HTTP shim around the running `ferrited`. Same shape the CLI
/// `Driver` uses — kept private to this module so the MCP path can
/// evolve without touching the CLI surface (and vice versa). Re-uses
/// the same `reqwest::Client` lifecycle (single connection pool,
/// caller-supplied `X-Ferrite-Note` header baked in).
#[derive(Clone)]
struct Http {
    client: Client,
    base: String,
}

impl Http {
    async fn get(&self, path: &str) -> Result<Value, McpError> {
        let resp = self
            .client
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .map_err(|e| http_err(&format!("GET {path}"), e.to_string()))?;
        api_response(resp, &format!("GET {path}")).await
    }
    async fn post(&self, path: &str, body: Value) -> Result<Value, McpError> {
        let resp = self
            .client
            .post(format!("{}{}", self.base, path))
            .json(&body)
            .send()
            .await
            .map_err(|e| http_err(&format!("POST {path}"), e.to_string()))?;
        api_response(resp, &format!("POST {path}")).await
    }
    async fn patch(&self, path: &str, body: Value) -> Result<Value, McpError> {
        let resp = self
            .client
            .patch(format!("{}{}", self.base, path))
            .json(&body)
            .send()
            .await
            .map_err(|e| http_err(&format!("PATCH {path}"), e.to_string()))?;
        api_response(resp, &format!("PATCH {path}")).await
    }
}

fn http_err(ctx: &str, msg: String) -> McpError {
    McpError::internal_error(format!("{ctx}: {msg}"), None)
}

async fn api_response(resp: reqwest::Response, ctx: &str) -> Result<Value, McpError> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| http_err(ctx, format!("read body: {e}")))?;
    if !status.is_success() {
        return Err(http_err(ctx, format!("{status}: {text}")));
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

// ─── tool request schemas ───────────────────────────────────────────────

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SelectDeviceArgs {
    /// Soapy device-args string identifying which SDR to bind to,
    /// e.g. `driver=rtlsdr,serial=00000001` or
    /// `driver=hackrf,serial=0000000000000000b74865dc2b4f1bd7`.
    /// Use `list_devices` to see what's enumerable.
    pub args: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoadPresetArgs {
    /// Preset basename — matches `flowgraphs/<name>.json` on the
    /// daemon (e.g. `wbfm`, `nwr`, `adsb`, `rtl433-433`). Use
    /// `list_presets` to enumerate.
    pub name: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TuneArgs {
    /// Listen frequency in Hz (the operator-visible tune target). For
    /// zero-IF radios the server places the source LO off-target per
    /// the per-driver dodge ratio and pulls this freq back through
    /// the channelizer — see D29 in docs/09-decisions.md.
    pub freq_hz: f64,
    /// Optional sample-rate floor in Hz. When given and larger than
    /// the current source rate, the source rate is raised (the driver
    /// clamps to its rate ladder).
    #[serde(default)]
    pub span_hz: Option<f64>,
    /// Per-driver DC-spike dodge ratio (fraction of the channelizer's
    /// `output_rate_hz`). Defaults to 0 = no dodge. HackRF needs
    /// ~0.7; DC-tracking drivers (SDRplay, RTL-SDR, Airspy) leave it
    /// at 0. **Must exceed 0.5** when non-zero, or the spike sits
    /// inside the demodulated passband.
    #[serde(default)]
    pub offset_ratio: Option<f64>,
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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TranscribeArgs {
    /// `true` enables the in-browser VoiceTranscribe tap; `false`
    /// disables it. The pipeline rebuilds to splice the tap in/out.
    /// Needs a UI tab connected for the whisper.cpp worker to
    /// actually run; check decodes with `recent_decodes` filtered
    /// to `decoder::transcribe`.
    pub enabled: bool,
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RecentDecodesArgs {
    /// Tracing-target prefix to filter on. Defaults to `decoder`
    /// (everything decoder-side). Examples: `decoder::rtl_433`,
    /// `decoder::pocsag`, `decoder::transcribe`, `decoder::ft8`.
    #[serde(default)]
    pub category: Option<String>,
    /// How far back to pull entries (seconds). Default 30. Buffer
    /// caps at ~4096 entries, so very long lookbacks are bounded.
    #[serde(default)]
    pub lookback_secs: Option<f64>,
    /// Cap the number of entries returned (newest kept). 0 / unset
    /// = no cap.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ViewSnapshotArgs {
    /// Which canvas to snapshot: `wide-spectrum`, `wide-waterfall`,
    /// `channel-spectrum`, or `channel-waterfall`. The `channel-*`
    /// panes are only meaningful when the active preset has a
    /// Channelizer.
    pub pane: String,
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
}

// ─── server ─────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct FerriteServer {
    http: Http,
    // `tool_router` is consumed by the `#[tool_handler]`-expanded
    // impl below; rustc's dead-code pass can't see through the macro
    // and warns spuriously.
    #[allow(dead_code)]
    tool_router: ToolRouter<FerriteServer>,
}

#[tool_router]
impl FerriteServer {
    fn new(http: Http) -> Self {
        Self {
            http,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "One-shot snapshot of the running ferrited: pipeline status (running/stopped), the active source config (driver/freq/rate/gain), the loaded flowgraph name, and the set of UI sinks. Cheap; the AI calls this first to learn what the world looks like."
    )]
    async fn status(&self) -> Result<CallToolResult, McpError> {
        let status = self.http.get("/api/pipeline").await?;
        let source = self.http.get("/api/source").await.unwrap_or(Value::Null);
        let flowgraph = self.http.get("/api/flowgraph").await.unwrap_or(Value::Null);
        let ui_sinks = self.http.get("/api/ui-sinks").await.unwrap_or(Value::Null);
        let preset_name = flowgraph.get("name").cloned().unwrap_or(Value::Null);
        ok_json(&json!({
            "pipeline": status,
            "source": source,
            "preset": preset_name,
            "ui_sinks": ui_sinks,
        }))
    }

    #[tool(
        description = "List every SoapySDR device the daemon currently sees. Returns the cached capability schema (driver, label, serial, sample-rate/freq/bandwidth ranges, gains, antennas) — same payload the source dialog uses."
    )]
    async fn list_devices(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.http.get("/api/devices").await?)
    }

    #[tool(
        description = "Capability schema of the currently-bound source — antennas (port names), Soapy settings (filters, bias-T, AGC tuners — every key the driver lets you patch via `set_block_param(block='src', params={settings: {...}})`), gain ladder, sample-rate / bandwidth / frequency ranges. Use this before swapping antennas or flipping driver-specific knobs so you know what's actually settable on the bound SDR. Tagged response: `kind='hardware'|'software'|'unavailable'`."
    )]
    async fn source_capabilities(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.http.get("/api/source/capabilities").await?)
    }

    #[tool(
        description = "Every block in the currently-composed preset, including the resolved source and any auto-inserted bridges. Each entry has the block's id, type_name, full BlockSpec, current values, and (when published) ai_notes. Use to introspect the active pipeline's topology before guessing block ids — `chan` exists in some presets, `audio_nr` in voice-mode presets, `rds` in WBFM, etc. Pairs with `set_block_param(block=<id>, params={...})`."
    )]
    async fn list_blocks(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.http.get("/api/pipeline/blocks").await?)
    }

    #[tool(
        description = "Every registered block type's schema (sorted by `type_name`). Static — doesn't depend on the active preset. Each entry has the type_name, the BlockSpec (param keys, kinds, defaults, ranges, ai_notes), and the reconfig_scope (`self` / `downstream` / `sourceRestart`) so you know whether a param hot-applies or rebuilds. Use to look up the exact param key for a block before patching it (saves a guess-then-retry round)."
    )]
    async fn list_block_types(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.http.get("/api/blocks").await?)
    }

    #[tool(
        description = "Recover from an in-process SoapySDR driver wedge (external `SoapySDRUtil --find` works but our enumerate hangs / misses devices). Tears down + re-loads every Soapy driver module via the soapysdr-sys C ABI. Refuses with 409 when the pipeline is running — call `stop` first. Service-process drivers (notably SDRplay) usually wedge on the service side, not in the module; `systemctl restart sdrplay` is the right hammer there."
    )]
    async fn reload_drivers(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.http.post("/api/devices/reload", json!({})).await?)
    }

    #[tool(
        description = "Switch the active source to the SDR identified by the supplied Soapy args string (e.g. `driver=hackrf,serial=…`). Source params reset to the driver's defaults — the previous source's centre freq / rate / gain do NOT carry over (a SineSource centre of 100 MHz would have been meaningless on the SDR anyway). Always follow up with `tune` to set the listen frequency and `set_block_param` for gain / antenna / AGC. Triggers a source restart."
    )]
    async fn select_device(
        &self,
        Parameters(args): Parameters<SelectDeviceArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Cross-source-type param carryover is a footgun: a previous
        // SineSource's `tone_freq_abs_hz` makes no sense on a real
        // SDR, and its default `center_freq_hz=100_000_000` Hz would
        // land the new SDR at 100 MHz regardless of band. Send only
        // the args; the server fills the driver-appropriate defaults
        // (sample rate, bandwidth, gain) from the device's
        // capability schema.
        ok_json(
            &self
                .http
                .patch(
                    "/api/source",
                    json!({ "type": "SoapySource", "params": { "args": args.args } }),
                )
                .await?,
        )
    }

    #[tool(
        description = "List every preset flowgraph the daemon can load. Returned entries carry name + label + description so the AI can pick the right one (e.g. `nwr` for NOAA weather, `adsb` for aircraft, `rtl433-433` for ISM telemetry)."
    )]
    async fn list_presets(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.http.get("/api/presets").await?)
    }

    #[tool(
        description = "Load a preset flowgraph by name. The daemon preserves the live source's centre freq across the swap, so a load-then-tune is two distinct steps. Match a name from `list_presets`."
    )]
    async fn load_preset(
        &self,
        Parameters(args): Parameters<LoadPresetArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(
            &self
                .http
                .post("/api/preset", json!({ "name": args.name }))
                .await?,
        )
    }

    #[tool(
        description = "Tune the radio to listen at `freq_hz`. Routes through POST /api/tune so the per-driver DC-spike dodge applies (the operator-visible listen freq stays at the requested value; the source LO may move so the spike doesn't land in the demodulated channel). Supply `offset_ratio` from the per-driver SDR-preset JSON (HackRF ~0.7, DC-tracking drivers leave at 0). `span_hz` raises the source rate if larger than current."
    )]
    async fn tune(
        &self,
        Parameters(args): Parameters<TuneArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = json!({
            "freq_hz": args.freq_hz,
            "span_hz": args.span_hz,
            "offset_ratio": args.offset_ratio.unwrap_or(0.0),
        });
        ok_json(&self.http.post("/api/tune", body).await?)
    }

    #[tool(
        description = "PATCH a parameter delta onto one block in the running pipeline. `block` is the block id (e.g. `src`, `chan`, `demod`, `audio_nr`); `params` is a JSON object of key→new_value pairs. Live-tunable keys hot-apply (no rebuild); others rebuild the block. The src block aliases to PATCH /api/source. Same surface the UI's <BlockParams> component uses."
    )]
    async fn set_block_param(
        &self,
        Parameters(args): Parameters<BlockParamsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let path = format!("/api/pipeline/blocks/{}/params", args.block);
        ok_json(&self.http.post(&path, args.params).await?)
    }

    #[tool(
        description = "Start the pipeline (instantiates blocks if it was stopped). Idempotent — calling on an already-running pipeline is a no-op."
    )]
    async fn start(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.http.post("/api/pipeline/start", json!({})).await?)
    }

    #[tool(
        description = "Stop the running pipeline. The source device is released so a subsequent `select_device` / `reload_drivers` / preset change can claim it cleanly."
    )]
    async fn stop(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.http.post("/api/pipeline/stop", json!({})).await?)
    }

    #[tool(
        description = "Toggle the in-browser speech-to-text tap on a voice preset. Splices a VoiceTranscribe block before the AudioSink (`transcribe` implies `audio`). Whisper.cpp runs in the operator's browser — a UI tab must be connected for decodes to flow. Read the result back with `recent_decodes` filtered to `decoder::transcribe`. Use a *listen* preset (`wbfm`, `nbfm`, `usb`, `lsb`, `wbam`), not a headless `*-record` preset."
    )]
    async fn transcribe(
        &self,
        Parameters(args): Parameters<TranscribeArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Mirror the CLI: GET /api/profile, merge transcribe flag,
        // PATCH it back. Server enforces `transcribe → audio`.
        let current = self.http.get("/api/profile").await.unwrap_or(Value::Null);
        let mut profile = current.as_object().cloned().unwrap_or_default();
        profile.insert("transcribe".into(), Value::Bool(args.enabled));
        if args.enabled {
            profile.insert("audio".into(), Value::Bool(true));
        }
        ok_json(
            &self
                .http
                .patch("/api/profile", Value::Object(profile))
                .await?,
        )
    }

    #[tool(
        description = "Recent decoder-log entries — the AI's 'did any decode land?' check. `category` filters on a tracing target prefix (defaults to `decoder` = everything decoder-side); examples: `decoder::rtl_433`, `decoder::pocsag`, `decoder::transcribe`, `decoder::ft8`, `decoder::ais`. `lookback_secs` (default 30) bounds how far back to scan; `limit` (default 0 = no cap) trims to the newest N entries."
    )]
    async fn recent_decodes(
        &self,
        Parameters(args): Parameters<RecentDecodesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut q = vec![format!(
            "category={}",
            args.category.as_deref().unwrap_or("decoder")
        )];
        if let Some(lb) = args.lookback_secs {
            q.push(format!("lookback={lb}"));
        }
        if let Some(lim) = args.limit {
            if lim > 0 {
                q.push(format!("limit={lim}"));
            }
        }
        let path = format!("/api/decoder/recent?{}", q.join("&"));
        ok_json(&self.http.get(&path).await?)
    }

    #[tool(
        description = "Snapshot one of the four spectrum/waterfall canvases the browser is currently rendering. Writes the PNG to `/tmp/ferrite-views/<pane>-<unix_ms>.png` and returns the path + byte count — band-plan overlay, VFO marker, contrast, pause state, and zoom are all baked in (no re-render from raw FFT). Read the returned path with the `Read` tool to see the image. Requires a UI tab connected to /ws/ui-views; 503 when none. Note: `wide-*` panes are only rendered while `main_pane=wide`; `channel-*` panes only when the active preset has a Channelizer AND `channel_detail_visible=true`. Check via `view_state` if a snapshot returns 404 (canvas not mounted)."
    )]
    async fn view_snapshot(
        &self,
        Parameters(args): Parameters<ViewSnapshotArgs>,
    ) -> Result<CallToolResult, McpError> {
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
        // The server returns raw image/png bytes (not JSON), so the
        // generic `Http::get` text-then-parse path doesn't fit — go
        // through the bare reqwest client. Path is `/api/ui-views/:pane`
        // (plural, no `snapshot/` prefix); the earlier wrong path
        // `/api/ui-view/snapshot/<pane>` 404'd every call.
        let url = format!("{}/api/ui-views/{}", self.http.base, args.pane);
        let resp = self
            .http
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| http_err(&format!("GET {url}"), e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(http_err(&format!("GET {url}"), format!("{status}: {body}")));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| http_err(&format!("GET {url}"), format!("read body: {e}")))?;
        // Mirror the CLI: write to `/tmp/ferrite-views/<pane>-<ms>.png`
        // and return the path. AI clients can `Read` the file as an
        // image content block.
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let dir = std::path::PathBuf::from("/tmp/ferrite-views");
        std::fs::create_dir_all(&dir)
            .map_err(|e| http_err("mkdir /tmp/ferrite-views", e.to_string()))?;
        let out_path = dir.join(format!("{}-{ms}.png", args.pane));
        std::fs::write(&out_path, &bytes)
            .map_err(|e| http_err(&format!("write {}", out_path.display()), e.to_string()))?;
        ok_json(&json!({
            "path": out_path.display().to_string(),
            "bytes": bytes.len(),
            "pane": args.pane,
        }))
    }

    #[tool(
        description = "Read the browser's authored UI chrome state — which Main tab is active (FFT/Waterfall or the per-preset advanced view), channel-detail visibility, per-pane zoom + pause. Lets the AI tailor responses to what the operator is actually looking at, without round-tripping through a PNG. 503 when no UI tab is connected."
    )]
    async fn view_state(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.http.get("/api/view").await?)
    }

    #[tool(
        description = "Push a chrome-state patch to the operator's browser. Use this to put them on the right pane for what's happening: `main_pane='advanced'` switches the main column to the per-preset view (FT8 map / ADS-B map / APRS map / Transcript / fldigi console, depending on the active preset); `main_pane='wide'` switches back to the FFT/Waterfall. `channel_detail_visible` flips the narrow channel-detail column on/off. Unset fields are left alone. 503 when no UI tab is connected — the patch needs a viewer to land on."
    )]
    async fn set_view_state(
        &self,
        Parameters(args): Parameters<SetViewStateArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut body = serde_json::Map::new();
        if let Some(p) = args.main_pane {
            body.insert("main_pane".into(), Value::String(p));
        }
        if let Some(b) = args.channel_detail_visible {
            body.insert("channel_detail_visible".into(), Value::Bool(b));
        }
        if let Some(t) = args.left_tab {
            body.insert("left_tab".into(), Value::String(t));
        }
        ok_json(
            &self
                .http
                .post("/api/ui-view/set", Value::Object(body))
                .await?,
        )
    }
}

#[tool_handler]
impl ServerHandler for FerriteServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2025_03_26)
            .with_instructions(
                "Ferrite SDR control via MCP. Tools wrap the running ferrited's REST API: \
             `status` first (cheap snapshot), `list_devices` / `list_presets` for inventory, \
             `select_device` + `load_preset` to set up, `start`/`stop` for lifecycle, \
             `tune` for the dodge-aware listen-frequency change, `set_block_param` for \
             everything else, `recent_decodes` to read decoder output, `view_snapshot` / \
             `view_state` to see what the operator is looking at. Source code lives under \
             `tools/ferrite-ctl/src/mcp.rs`; per-tool behaviour matches the REST endpoints \
             documented in `docs/02-protocol.md`."
                    .to_string(),
            )
    }
}

/// Entry point — wires stdio to the tool-router and runs until the
/// client closes. Logs go to stderr; stdout is reserved for the MCP
/// JSON-RPC stream and a stray print here would corrupt the protocol.
pub async fn serve(client: Client, base: String) -> Result<()> {
    // Tracing → stderr. `RUST_LOG=debug` (or `RUST_LOG=rmcp=debug`)
    // surfaces per-frame protocol detail when debugging a misbehaving
    // client; default level is `info` for boot lines only.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    tracing::info!(base = %base, "ferrite-ctl mcp: serving on stdio");
    let http = Http { client, base };
    let server = FerriteServer::new(http);
    let service = server
        .serve(stdio())
        .await
        .map_err(|e| anyhow::anyhow!("mcp serve: {e}"))?;
    service.waiting().await.ok();
    Ok(())
}
