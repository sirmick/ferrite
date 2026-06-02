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

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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
use tokio::sync::RwLock;
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
}

impl Http {
    pub(crate) fn new(client: Client, base: String, default_note: Option<String>) -> Self {
        Self {
            client,
            base,
            default_note,
        }
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

    /// Mutating POST that is deliberately *not* tagged for the activity
    /// transcript (no `x-ferrite-command` / `x-ferrite-note`). Used by
    /// `chat_post`: mirroring the headless conversation into the UI's AI
    /// panel shouldn't also spam the `ai::activity` command log.
    async fn post_raw(&self, path: &str, body: Value) -> Result<Value, McpError> {
        let resp = self
            .client
            .post(format!("{}{}", self.base, path))
            .json(&body)
            .send()
            .await
            .map_err(|e| http_err(&format!("POST {path}"), e.to_string()))?;
        api_response(resp, &format!("POST {path}")).await
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
        let mut req = req.header("x-ferrite-command", cmd);
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
    /// runs with no browser — read results with `recent_decodes` filtered
    /// to `decoder::transcribe`. `"browser"` sets the split to `browser`
    /// (in-browser whisper, needs a UI tab). Omit to leave the current
    /// split unchanged.
    #[serde(default)]
    pub placement: Option<String>,
    /// Optional "why" note surfaced in the UI's activity transcript.
    #[serde(default)]
    pub note: Option<String>,
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

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StartCaptureAudioArgs {
    /// Centre frequency in Hz to tune for the capture.
    pub freq_hz: f64,
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

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CaptureStatusArgs {
    /// Job id from a previous `start_capture_*` reply. Omit to get a
    /// list of every job this MCP session knows about (most recent
    /// first), capped to ~20 entries.
    #[serde(default)]
    pub job_id: Option<String>,
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
}

// ─── async-capture registry ─────────────────────────────────────────────

/// What a capture job is recording. `Iq`/`Fft` use the non-disruptive
/// live-record tee (`chan`/`logmag`); `Audio` and wideband IQ swap in a
/// recording preset and run a `Source → FileSink` slice.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "kebab-case")]
enum CaptureKind {
    Iq,
    Fft,
    Audio,
}

impl CaptureKind {
    /// Live-record block id for the tee path. Only meaningful for the
    /// live `Iq`/`Fft` kinds.
    fn live_block_id(self) -> &'static str {
        match self {
            Self::Iq => "chan",
            Self::Fft => "logmag",
            Self::Audio => "audio",
        }
    }
    fn ext(self) -> &'static str {
        match self {
            Self::Iq => "cf32",
            Self::Fft => "bin",
            Self::Audio => "wav",
        }
    }
}

/// State of one async capture. The `start_capture_*` tools return
/// immediately with a `Running` entry; the spawned task transitions it
/// to `Done` (path + sidecar ready) or `Failed` when the duration
/// elapses.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum JobStatus {
    Running,
    Done,
    Failed { error: String },
}

#[derive(Debug, Clone)]
struct CaptureJob {
    job_id: String,
    kind: CaptureKind,
    status: JobStatus,
    output_path: String,
    duration_s: f64,
    /// Unix ms — set when the start tool returns.
    started_at: u128,
    /// Unix ms — set when the task transitions out of `Running`.
    finished_at: Option<u128>,
    /// Sidecar JSON the recording block wrote next to `output_path`.
    /// Populated on success.
    sidecar: Option<Value>,
}

// Manual `Serialize` so `status` is a flat string ("running"/"done"/
// "failed") with a sibling `error`, matching what `capture_status` docs
// promise and what the CLI poll loop reads. The derived form (with
// `JobStatus`'s `#[serde(tag = "status")]`) double-wrapped it as
// `status: { "status": "running" }`, so the poll loop's `as_str()` check
// always saw `None`, declared the job "not running", and returned a
// partial file after one poll instead of waiting for the capture.
impl Serialize for CaptureJob {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let (status, error) = match &self.status {
            JobStatus::Running => ("running", None),
            JobStatus::Done => ("done", None),
            JobStatus::Failed { error } => ("failed", Some(error.as_str())),
        };
        let mut st = s.serialize_struct("CaptureJob", 9)?;
        st.serialize_field("job_id", &self.job_id)?;
        st.serialize_field("kind", &self.kind)?;
        st.serialize_field("status", status)?;
        st.serialize_field("error", &error)?;
        st.serialize_field("output_path", &self.output_path)?;
        st.serialize_field("duration_s", &self.duration_s)?;
        st.serialize_field("started_at", &self.started_at)?;
        st.serialize_field("finished_at", &self.finished_at)?;
        st.serialize_field("sidecar", &self.sidecar)?;
        st.end()
    }
}

#[derive(Clone)]
pub struct FerriteServer {
    http: Http,
    /// Async-capture registry. Indexed by job_id. Cap is ~20 entries
    /// (LRU eviction on insert).
    jobs: Arc<RwLock<Vec<CaptureJob>>>,
    /// Monotonic counter for job ids. Per MCP-server lifetime.
    next_job: Arc<AtomicU64>,
    // `tool_router` is consumed by the `#[tool_handler]`-expanded impl
    // below; rustc's dead-code pass can't see through the macro.
    #[allow(dead_code)]
    tool_router: ToolRouter<FerriteServer>,
}

const JOBS_CAP: usize = 20;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn default_capture_path(kind: CaptureKind, freq_hz: f64) -> PathBuf {
    let mhz = (freq_hz / 1e6).round();
    let stem = format!("{:?}-{}-{:.0}mhz", kind, now_ms(), mhz).to_lowercase();
    PathBuf::from("/tmp/ferrite-captures").join(format!("{stem}.{}", kind.ext()))
}

async fn upsert_job(jobs: &Arc<RwLock<Vec<CaptureJob>>>, job: CaptureJob) {
    let mut guard = jobs.write().await;
    if let Some(pos) = guard.iter().position(|j| j.job_id == job.job_id) {
        guard[pos] = job;
    } else {
        guard.insert(0, job);
        guard.truncate(JOBS_CAP);
    }
}

/// Mark a job `Done`/`Failed`, attach the sidecar, and stamp
/// `finished_at`. Shared tail of every capture task.
async fn finish_job(
    jobs: &Arc<RwLock<Vec<CaptureJob>>>,
    job_id: &str,
    result: Result<(), String>,
    sidecar: Option<Value>,
) {
    let status = match &result {
        Ok(()) => JobStatus::Done,
        Err(e) => JobStatus::Failed { error: e.clone() },
    };
    let job = {
        let g = jobs.read().await;
        g.iter().find(|j| j.job_id == job_id).cloned()
    };
    if let Some(mut j) = job {
        j.status = status;
        j.finished_at = Some(now_ms());
        j.sidecar = sidecar;
        upsert_job(jobs, j).await;
    }
}

/// Read the sidecar JSON a recording block wrote next to `out_path`
/// (`<stem>.<ext>.json`). Best-effort — `None` if absent / unparseable.
async fn read_sidecar(out_path: &std::path::Path, kind: CaptureKind) -> Option<Value> {
    let side = out_path.with_extension(format!("{}.json", kind.ext()));
    match tokio::fs::read(&side).await {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).ok(),
        Err(_) => None,
    }
}

/// Live-record tee orchestration (`Iq`/`Fft`): patch the block's
/// `record_path` + `record_max_seconds`, sleep, clear `record_path`.
/// Non-disruptive — the user's UI session keeps running. Auto-starts a
/// stopped pipeline first (the live patch hits a block instance, which
/// only exists while sampling).
#[allow(clippy::too_many_arguments)] // capture-task fan-out; a struct would add ceremony without clarifying.
async fn run_live_capture_task(
    http: Http,
    jobs: Arc<RwLock<Vec<CaptureJob>>>,
    job_id: String,
    kind: CaptureKind,
    duration_s: f64,
    freq_hz: Option<f64>,
    out_path: PathBuf,
    note: Option<String>,
) {
    let block = kind.live_block_id();
    let path_str = out_path.display().to_string();
    let cmd = match kind {
        CaptureKind::Iq => "capture-iq",
        CaptureKind::Fft => "capture-fft",
        CaptureKind::Audio => "capture-audio",
    };
    let note = note.as_deref();
    let result: Result<(), String> = async {
        if let Some(f) = freq_hz {
            http.post(
                "/api/tune",
                json!({ "freq_hz": f, "offset_ratio": 0.0 }),
                "tune",
                note,
            )
            .await
            .map_err(|e| format!("retune: {}", display_mcp_err(&e)))?;
        }
        // The live patch needs a running pipeline — auto-start if stopped.
        let running = http
            .get("/api/pipeline")
            .await
            .ok()
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_owned))
            .map(|s| s == "running")
            .unwrap_or(false);
        if !running {
            http.post("/api/pipeline/start", json!({}), "pipeline-start", note)
                .await
                .map_err(|e| format!("auto-start pipeline: {}", display_mcp_err(&e)))?;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        http.post(
            &format!("/api/pipeline/blocks/{block}/params"),
            json!({ "record_path": path_str, "record_max_seconds": duration_s }),
            cmd,
            note,
        )
        .await
        .map_err(|e| format!("start recording on `{block}`: {}", display_mcp_err(&e)))?;
        tokio::time::sleep(Duration::from_secs_f64(duration_s + 0.5)).await;
        let _ = http
            .post(
                &format!("/api/pipeline/blocks/{block}/params"),
                json!({ "record_path": "" }),
                cmd,
                note,
            )
            .await;
        Ok(())
    }
    .await;
    let sidecar = if result.is_ok() {
        read_sidecar(&out_path, kind).await
    } else {
        None
    };
    finish_job(&jobs, &job_id, result, sidecar).await;
}

/// Preset-swap capture orchestration (`Audio`, wideband `Iq`): load a
/// recording preset, patch the source + the sink block on the
/// (stopped) flowgraph, start, sleep, stop. Disruptive — clobbers the
/// live session. Mirrors the old CLI `capture audio` / `--wideband`
/// paths.
#[allow(clippy::too_many_arguments)]
async fn run_preset_capture_task(
    http: Http,
    jobs: Arc<RwLock<Vec<CaptureJob>>>,
    job_id: String,
    kind: CaptureKind,
    preset: String,
    source_patch: Value,
    block_id: String,
    block_patch: Value,
    duration_s: f64,
    out_path: PathBuf,
    note: Option<String>,
) {
    let cmd = match kind {
        CaptureKind::Audio => "capture-audio",
        _ => "capture-iq",
    };
    let note = note.as_deref();
    let result: Result<(), String> = async {
        http.post(
            "/api/preset",
            json!({ "name": preset }),
            "preset-load",
            note,
        )
        .await
        .map_err(|e| format!("load preset `{preset}`: {}", display_mcp_err(&e)))?;
        patch_source_merge(&http, &source_patch, cmd, note)
            .await
            .map_err(|e| format!("patch source: {}", display_mcp_err(&e)))?;
        patch_flowgraph_block(&http, &block_id, &block_patch, cmd, note)
            .await
            .map_err(|e| format!("patch `{block_id}`: {}", display_mcp_err(&e)))?;
        http.post("/api/pipeline/start", json!({}), "pipeline-start", note)
            .await
            .map_err(|e| format!("start pipeline: {}", display_mcp_err(&e)))?;
        tokio::time::sleep(Duration::from_secs_f64(duration_s + 0.5)).await;
        let _ = http
            .post("/api/pipeline/stop", json!({}), "pipeline-stop", note)
            .await;
        Ok(())
    }
    .await;
    let sidecar = if result.is_ok() {
        read_sidecar(&out_path, kind).await
    } else {
        None
    };
    finish_job(&jobs, &job_id, result, sidecar).await;
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

/// Edit a single block's params in the active flowgraph doc
/// (read-modify-write via `PATCH /api/flowgraph`) — applies before
/// `pipeline start` so the first instantiation sees the new values.
async fn patch_flowgraph_block(
    http: &Http,
    block_id: &str,
    delta: &Value,
    cmd: &str,
    note: Option<&str>,
) -> Result<Value, McpError> {
    let mut doc = http.get("/api/flowgraph").await?;
    let blocks = doc
        .get_mut("blocks")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| McpError::internal_error("flowgraph has no `blocks`", None))?;
    let block = blocks
        .get_mut(block_id)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            McpError::invalid_params(format!("flowgraph has no block `{block_id}`"), None)
        })?;
    let params = block
        .entry("params".to_string())
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| {
            McpError::internal_error(format!("`{block_id}.params` not an object"), None)
        })?;
    if let Some(obj) = delta.as_object() {
        for (k, v) in obj {
            params.insert(k.clone(), v.clone());
        }
    }
    http.patch("/api/flowgraph", doc, cmd, note).await
}

/// Best-effort error-string extraction from an `McpError` (its fields
/// aren't `pub` in the rmcp version we pin, so go through Display).
fn display_mcp_err(e: &McpError) -> String {
    format!("{e}")
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
            jobs: Arc::new(RwLock::new(Vec::new())),
            next_job: Arc::new(AtomicU64::new(1)),
            tool_router: Self::tool_router(),
        }
    }

    /// One-shot world snapshot: pipeline status, source config, the full
    /// active flowgraph, its name, UI sinks, and a hoisted `ready`
    /// boolean (`pipeline.status == "running"`).
    pub(crate) async fn status_op(&self) -> Result<Value, McpError> {
        let pipeline = self.http.get("/api/pipeline").await?;
        let source = self.http.get("/api/source").await.unwrap_or(Value::Null);
        let flowgraph = self.http.get("/api/flowgraph").await.unwrap_or(Value::Null);
        let ui_sinks = self.http.get("/api/ui-sinks").await.unwrap_or(Value::Null);
        let preset = flowgraph.get("name").cloned().unwrap_or(Value::Null);
        let ready = pipeline.get("status").and_then(Value::as_str) == Some("running");
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
            "ready": ready,
        }))
    }

    pub(crate) async fn list_devices_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/devices").await
    }

    pub(crate) async fn source_capabilities_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/source/capabilities").await
    }

    /// Per-block flow diagnostics (sample throughput / process-time /
    /// ring fill) for the running pipeline, or `null` when stopped. The
    /// "are samples actually moving?" health check beyond `status.ready`.
    pub(crate) async fn flowdiag_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/flowdiag").await
    }

    /// Latest 1 Hz driver readback (actual gain, AGC state, antenna,
    /// bandwidth as the hardware reports them) — confirms a `set_block_param`
    /// gain/AGC change really landed. `null` when stopped or non-Soapy.
    pub(crate) async fn source_readback_op(&self) -> Result<Value, McpError> {
        self.http.get("/api/source/readback").await
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

        // Only the keys we resolve; the rest (sideband, offset_hz,
        // deviation_hz, mod_depth, output_rate_hz) fall back to the
        // block's `#[serde(default)]`.
        let params = json!({
            "path": path,
            "kind": kind,
            "modulation": modulation,
            "rate_hz_hint": rate,
            "center_freq_hz": center,
            "loop_playback": loop_playback,
        });
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

    /// Current strongest-signal watchlist (GET /api/signals) — the live
    /// ranked list from the `SignalList` block at the wideband FFT, not a
    /// history. Each row's `freq_hz` feeds straight into `tune`.
    pub(crate) async fn signals_op(&self, args: SignalsArgs) -> Result<Value, McpError> {
        let path = match args.max_age_ms {
            Some(ms) => format!("/api/signals?max_age_ms={ms}"),
            None => "/api/signals".to_string(),
        };
        self.http.get(&path).await
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
                    "span_hz": args.span_hz,
                    "offset_ratio": args.offset_ratio.unwrap_or(0.0),
                    "keep_lo": args.keep_lo,
                }),
                "tune",
                note,
            )
            .await?;

        if args.gain_db.is_none() && args.bandwidth_hz.is_none() && args.sample_rate_hz.is_none() {
            return Ok(json!({ "tune": tune, "source": Value::Null }));
        }

        // Build the source delta. Read current params so the rate→BW
        // auto-derive can key off the bound driver.
        let current = self.http.get("/api/source").await?;
        let mut delta = Map::new();
        if let Some(r) = args.sample_rate_hz {
            delta.insert("sample_rate_hz".into(), json!(r));
            if args.bandwidth_hz.is_none() {
                let driver = current
                    .get("params")
                    .and_then(|p| p.get("args"))
                    .and_then(Value::as_str)
                    .map(crate::driver_key_from_args)
                    .unwrap_or_default();
                if let Some(bw) = crate::sdr_tables::recommended_bandwidth_for(&driver, r) {
                    delta.insert("bandwidth_hz".into(), json!(bw));
                }
            }
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
        // The server returns raw image/png bytes, so go through the bare
        // client rather than the JSON-parsing `Http::get`.
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

    /// Mirror a headless conversation turn into the operator's browser
    /// AI panel. Builds the exact event frames the chat store already
    /// renders (`ferrite_ai_user_turn` / `stream_event` text-delta +
    /// `ferrite_ai_done` / tool cards) and fans them out via
    /// `POST /api/ai/chat-inject`; ferrited's `/ws/chat` proxy relays
    /// them to every live tab. Lets an agent driving ferrited from
    /// outside the browser show its prompts, tool calls, and replies in
    /// the UI. Needs a UI tab open; returns the daemon's delivery counts.
    pub(crate) async fn chat_post_op(&self, args: ChatPostArgs) -> Result<Value, McpError> {
        let t = now_ms();
        let events: Vec<Value> = match args.role.as_str() {
            "user" => {
                let text = args.text.unwrap_or_default();
                vec![json!({ "type": "ferrite_ai_user_turn", "text": text, "t": t })]
            }
            "assistant" => {
                let text = args.text.unwrap_or_default();
                vec![
                    json!({
                        "type": "stream_event",
                        "event": {
                            "type": "content_block_delta",
                            "delta": { "type": "text_delta", "text": text }
                        }
                    }),
                    json!({ "type": "ferrite_ai_done" }),
                ]
            }
            "tool" => {
                let id = format!("inj-{t}");
                let name = args.tool_name.unwrap_or_else(|| "tool".into());
                let input = args.tool_input.unwrap_or_default();
                let mut evs = vec![
                    json!({
                        "type": "stream_event",
                        "event": {
                            "type": "content_block_start",
                            "content_block": { "type": "tool_use", "id": id, "name": name }
                        }
                    }),
                    json!({
                        "type": "stream_event",
                        "event": {
                            "type": "content_block_delta",
                            "delta": { "type": "input_json_delta", "partial_json": input }
                        }
                    }),
                ];
                if let Some(result) = args.tool_result {
                    evs.push(json!({
                        "type": "user",
                        "message": { "content": [
                            { "type": "tool_result", "tool_use_id": id, "content": result }
                        ]}
                    }));
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
        self.http
            .post_raw("/api/ai/chat-inject", json!({ "events": events }))
            .await
    }

    pub(crate) async fn capture_status_op(
        &self,
        args: CaptureStatusArgs,
    ) -> Result<Value, McpError> {
        let guard = self.jobs.read().await;
        match args.job_id {
            Some(id) => {
                let Some(job) = guard.iter().find(|j| j.job_id == id) else {
                    return Err(McpError::invalid_params(
                        format!(
                            "no capture job with id {id:?}; this session has {} known",
                            guard.len()
                        ),
                        None,
                    ));
                };
                serde_json::to_value(job)
                    .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None))
            }
            None => serde_json::to_value(&*guard)
                .map_err(|e| McpError::internal_error(format!("serialize: {e}"), None)),
        }
    }

    pub(crate) async fn start_capture_iq_op(
        &self,
        args: StartCaptureArgs,
    ) -> Result<Value, McpError> {
        validate_duration(args.duration_s)?;
        if args.wideband {
            self.spawn_wideband_iq(args).await
        } else {
            let out = self
                .resolve_out(args.out.clone(), CaptureKind::Iq, args.freq_hz)
                .await;
            let job = self
                .register_job(CaptureKind::Iq, out.clone(), args.duration_s)
                .await?;
            let (http, jobs, job_id) = (self.http.clone(), self.jobs.clone(), job.job_id.clone());
            let note = args.note.clone();
            tokio::spawn(async move {
                run_live_capture_task(
                    http,
                    jobs,
                    job_id,
                    CaptureKind::Iq,
                    args.duration_s,
                    args.freq_hz,
                    out,
                    note,
                )
                .await;
            });
            serialize_job(&job)
        }
    }

    pub(crate) async fn start_capture_fft_op(
        &self,
        args: StartCaptureArgs,
    ) -> Result<Value, McpError> {
        validate_duration(args.duration_s)?;
        let out = self
            .resolve_out(args.out.clone(), CaptureKind::Fft, args.freq_hz)
            .await;
        let job = self
            .register_job(CaptureKind::Fft, out.clone(), args.duration_s)
            .await?;
        let (http, jobs, job_id) = (self.http.clone(), self.jobs.clone(), job.job_id.clone());
        let note = args.note.clone();
        tokio::spawn(async move {
            run_live_capture_task(
                http,
                jobs,
                job_id,
                CaptureKind::Fft,
                args.duration_s,
                args.freq_hz,
                out,
                note,
            )
            .await;
        });
        serialize_job(&job)
    }

    /// Refuse a preset-swapping capture when it would silently tear down
    /// an active transcription (or other audio-profile) session. The
    /// `start_capture_audio` / wideband-IQ paths load a `*-record` preset,
    /// which replaces the whole running graph — including an injected
    /// `VoiceTranscribe` tap. Without this guard the tap just vanishes and
    /// decodes stop with no error (the trap that bit a live session). The
    /// caller can pass `force: true` to proceed anyway.
    async fn guard_active_audio_profile(&self, force: bool, what: &str) -> Result<(), McpError> {
        if force {
            return Ok(());
        }
        let profile = self.http.get("/api/profile").await.unwrap_or(Value::Null);
        let transcribing = profile
            .get("transcribe")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if transcribing {
            return Err(McpError::invalid_params(
                format!(
                    "{what} loads a recording preset, which would tear down the active \
                     transcription session (the running graph is replaced). Stop transcription \
                     first (`transcribe` enabled=false) or pass `force: true` to proceed anyway."
                ),
                None,
            ));
        }
        Ok(())
    }

    /// Refuse a capture that would run against a software/test source. A
    /// preset-swap capture inherits the live source; if that's a SineSource
    /// (or a restart's fallback) you capture plausible-looking noise, not
    /// RF — the worst kind of wrong data. `force` overrides (e.g. you really
    /// do want to record the synthetic source).
    async fn guard_hardware_source(&self, force: bool, what: &str) -> Result<(), McpError> {
        if force {
            return Ok(());
        }
        let source = self.http.get("/api/source").await.unwrap_or(Value::Null);
        let type_name = source
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if type_name != "SoapySource" {
            let what_src = if type_name.is_empty() {
                "a non-hardware source".to_string()
            } else {
                format!("a {type_name}")
            };
            return Err(McpError::invalid_params(
                format!(
                    "{what} would run against {what_src} — a software/test source, not real RF, \
                     so you'd record noise that looks like a signal. Select a device first \
                     (`device select <args>`) or pass `force: true` to capture the synthetic source."
                ),
                None,
            ));
        }
        Ok(())
    }

    pub(crate) async fn start_capture_audio_op(
        &self,
        args: StartCaptureAudioArgs,
    ) -> Result<Value, McpError> {
        let duration = args.duration_s.unwrap_or(10.0);
        validate_duration(duration)?;
        self.guard_active_audio_profile(args.force, "start_capture_audio")
            .await?;
        self.guard_hardware_source(args.force, "audio capture")
            .await?;
        let rate = args.sample_rate_hz.unwrap_or(2_400_000.0);
        let bw = args.bandwidth_hz.unwrap_or(rate);
        let preset = args
            .preset
            .clone()
            .unwrap_or_else(|| "fm-audio-record".into());
        let out = match args.out.clone() {
            Some(s) => PathBuf::from(s),
            None => default_capture_path(CaptureKind::Audio, args.freq_hz),
        };
        ensure_parent(&out).await?;
        let job = self
            .register_job(CaptureKind::Audio, out.clone(), duration)
            .await?;
        let source_patch = json!({
            "center_freq_hz": args.freq_hz,
            "sample_rate_hz": rate,
            "bandwidth_hz": bw,
            "gain_db": args.gain_db,
        });
        let block_patch = json!({ "path": out.display().to_string(), "max_seconds": duration });
        let (http, jobs, job_id) = (self.http.clone(), self.jobs.clone(), job.job_id.clone());
        let note = args.note.clone();
        tokio::spawn(async move {
            run_preset_capture_task(
                http,
                jobs,
                job_id,
                CaptureKind::Audio,
                preset,
                source_patch,
                "audio".into(),
                block_patch,
                duration,
                out,
                note,
            )
            .await;
        });
        serialize_job(&job)
    }

    /// Wideband IQ: swap in `capture_fm`, patch the source + `cap` sink,
    /// then run a `Source → FileIqSink` slice. Needs `freq_hz`.
    async fn spawn_wideband_iq(&self, args: StartCaptureArgs) -> Result<Value, McpError> {
        let freq = args.freq_hz.ok_or_else(|| {
            McpError::invalid_params("wideband IQ capture requires `freq_hz`", None)
        })?;
        self.guard_active_audio_profile(args.force, "wideband IQ capture")
            .await?;
        self.guard_hardware_source(args.force, "wideband IQ capture")
            .await?;
        let rate = args.sample_rate_hz.unwrap_or(2_000_000.0);
        let bw = args.bandwidth_hz.unwrap_or(rate);
        let format = args.format.clone().unwrap_or_else(|| "cf32".into());
        let out = match args.out.clone() {
            Some(s) => PathBuf::from(s),
            None => default_capture_path(CaptureKind::Iq, freq),
        };
        ensure_parent(&out).await?;
        let job = self
            .register_job(CaptureKind::Iq, out.clone(), args.duration_s)
            .await?;
        let source_patch = json!({
            "center_freq_hz": freq,
            "sample_rate_hz": rate,
            "bandwidth_hz": bw,
            "gain_db": args.gain_db,
        });
        let block_patch = json!({
            "path": out.display().to_string(),
            "format": format,
            "rate_hz": rate,
            "center_freq_hz": freq,
            "max_seconds": args.duration_s,
            "write_sidecar": true,
        });
        let (http, jobs, job_id) = (self.http.clone(), self.jobs.clone(), job.job_id.clone());
        let note = args.note.clone();
        let duration = args.duration_s;
        tokio::spawn(async move {
            run_preset_capture_task(
                http,
                jobs,
                job_id,
                CaptureKind::Iq,
                "capture_fm".into(),
                source_patch,
                "cap".into(),
                block_patch,
                duration,
                out,
                note,
            )
            .await;
        });
        serialize_job(&job)
    }

    /// Resolve a capture output path, defaulting to the live source freq
    /// (so the filename carries a useful hint) when none is supplied.
    async fn resolve_out(
        &self,
        out: Option<String>,
        kind: CaptureKind,
        freq_hz: Option<f64>,
    ) -> PathBuf {
        if let Some(s) = out {
            return PathBuf::from(s);
        }
        let freq = if let Some(f) = freq_hz {
            f
        } else {
            self.http
                .get("/api/source")
                .await
                .ok()
                .and_then(|src| src.get("params")?.get("center_freq_hz")?.as_f64())
                .unwrap_or(0.0)
        };
        default_capture_path(kind, freq)
    }

    /// Register a `Running` job in the LRU registry and return it.
    async fn register_job(
        &self,
        kind: CaptureKind,
        out: PathBuf,
        duration_s: f64,
    ) -> Result<CaptureJob, McpError> {
        ensure_parent(&out).await?;
        let job = CaptureJob {
            job_id: format!("cap-{}", self.next_job.fetch_add(1, Ordering::SeqCst)),
            kind,
            status: JobStatus::Running,
            output_path: out.display().to_string(),
            duration_s,
            started_at: now_ms(),
            finished_at: None,
            sidecar: None,
        };
        upsert_job(&self.jobs, job.clone()).await;
        Ok(job)
    }
}

fn validate_duration(d: f64) -> Result<(), McpError> {
    if d.is_finite() && d > 0.0 {
        Ok(())
    } else {
        Err(McpError::invalid_params(
            format!("duration_s must be a finite positive number, got {d}"),
            None,
        ))
    }
}

fn serialize_job(job: &CaptureJob) -> Result<Value, McpError> {
    serde_json::to_value(job)
        .map_err(|e| McpError::internal_error(format!("serialize capture job: {e}"), None))
}

async fn ensure_parent(out: &std::path::Path) -> Result<(), McpError> {
    if let Some(parent) = out.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| http_err(&format!("mkdir {}", parent.display()), e.to_string()))?;
    }
    Ok(())
}

// ─── MCP tool wrappers ──────────────────────────────────────────────────
//
// Each is a thin `ok_json(&self.<verb>_op(args).await?)` over the ops
// layer above. The `description` strings are the AI-facing docs.

#[tool_router]
impl FerriteServer {
    #[tool(
        description = "Mirror a conversation turn into the operator's browser AI \
                       panel so an agent driving ferrited headlessly (CLI / MCP) \
                       shows its prompts, tool calls, and replies in the UI alongside \
                       the in-browser assistant. `role`: `user` (a prompt), \
                       `assistant` (a reply — auto-closed so the spinner resolves), or \
                       `tool` (a tool-call card in the open assistant turn; use \
                       `tool_name` / `tool_input` / `tool_result`). Needs a UI tab \
                       connected to /ws/chat; frames with no viewer are dropped. Not \
                       logged to the `ai::activity` transcript."
    )]
    async fn chat_post(
        &self,
        Parameters(args): Parameters<ChatPostArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.chat_post_op(args).await?)
    }

    #[tool(
        description = "One-shot snapshot of the running ferrited: pipeline status (running/stopped) with a hoisted `ready` boolean, the active source config (driver/freq/rate/gain), the full active flowgraph + its `preset` name, and the set of UI sinks. Cheap; the AI calls this first to learn what the world looks like."
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
        description = "Per-block flow diagnostics for the running pipeline — sample throughput, process-time, and ring-buffer fill for every block, or `null` when stopped. The 'are samples actually moving?' health check that `status.ready` (just 'the pipeline is up') can't answer: after `start`, poll this to confirm the source is delivering frames downstream before you wait on a decode."
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
        description = "Point the source at a recorded sample (from `list_captures`) so the full RX chain runs against a known signal — no SDR needed. Routes through `ModulatedFileSource`: `audio` fixtures are re-modulated onto a carrier (`modulation` fm/am/usb/lsb), `iq` captures are upmixed verbatim. kind/rate/centre/modulation default from the matching capture entry's sidecar; pass them explicitly for a raw clip. Loops by default. Triggers a source restart — follow with `start` (and `load_preset` for the matching demod), then poll `recent_decodes` / `flowdiag`. The deterministic counterpart to `select_device` + `tune` for testing decoders."
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
        description = "Load a preset flowgraph by name. The daemon preserves the live source's centre freq across the swap, so a load-then-tune is two distinct steps. Match a name from `list_presets`."
    )]
    async fn load_preset(
        &self,
        Parameters(args): Parameters<LoadPresetArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.load_preset_op(args).await?)
    }

    #[tool(
        description = "Tune the radio to listen at `freq_hz`. Routes through POST /api/tune so the per-driver DC-spike dodge applies (the operator-visible listen freq stays at the requested value; the source LO may move so the spike doesn't land in the demodulated channel). Supply `offset_ratio` from the per-driver SDR-preset JSON (HackRF ~0.7, DC-tracking drivers leave at 0); `span_hz` raises the source rate if larger than current. Optionally fold in `gain_db` (manual gain — folds `agc=false` so it lands), `bandwidth_hz`, and `sample_rate_hz` (auto-derives BW from the driver ladder when set without `bandwidth_hz`) in the same call. Set `keep_lo:true` to hop *within the current span* by retuning the channelizer only — no LO move, no graph restart (so a running transcribe worker doesn't reload and the survey centre stays put); it falls back to a normal tune when the target is outside the span. Use it for the `signals` → hop → listen loop."
    )]
    async fn tune(
        &self,
        Parameters(args): Parameters<TuneArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.tune_op(args).await?)
    }

    #[tool(
        description = "PATCH a parameter delta onto one block in the running pipeline — the **advanced/direct** control surface. `block` is the block id (e.g. `src`, `chan`, `demod`, `audio_nr`); `params` is a JSON object of key→new_value pairs. Live-tunable keys hot-apply (no rebuild); others rebuild the block. The src block aliases to PATCH /api/source — its settable keys include gain_db, gain_mode, gain_elements, agc, antenna, bandwidth_hz, dc_offset_correction, and the driver-specific `settings` map (discover via `source_capabilities`). Same surface the UI's <BlockParams> component uses.\n\n⚠️ PREFER THE CONVENIENCE PATHS where one exists — they apply smart composition this raw PATCH skips, so a direct param can silently misbehave:\n- **Frequency:** use the `tune` tool, NOT `param src center_freq_hz` or `param chan freq_shift_hz`. `tune` applies the per-driver DC-spike dodge (offsets the LO so the carrier doesn't land on DC) and sets the channelizer shift together; setting center_freq_hz directly parks the carrier on the DC spike on a zero-IF radio (HackRF) and it won't decode.\n- **Gain:** use the single `gain_db` auto dial (it splits across stages footgun-free on HackRF), NOT `gain_mode=manual` + `gain_elements` — reach for manual per-stage only when you specifically need it.\nOnly drop to the raw param when there's no convenience equivalent (driver `settings`, decoder knobs, etc.) or you deliberately want the un-composed behaviour."
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
        description = "Toggle the speech-to-text tap on a voice preset. Splices a VoiceTranscribe block before the AudioSink (`transcribe` implies `audio`). `placement: \"node\"` is **headless** — whisper.cpp runs in `ferrited`, no browser needed (sets the audio split to `server`, pulling the whole audio chain node-side); `\"browser\"` runs whisper in a connected UI tab. Either way read decodes with `recent_decodes` filtered to `decoder::transcribe`. Use a *listen* preset (`wbfm`, `nbfm`, `usb`, `lsb`, `wbam`), not a headless `*-record` preset."
    )]
    async fn transcribe(
        &self,
        Parameters(args): Parameters<TranscribeArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.transcribe_op(args).await?)
    }

    #[tool(
        description = "Recent decoder-log entries — the AI's 'did any decode land?' check. `category` filters on a tracing target prefix (defaults to `decoder` = everything decoder-side); examples: `decoder::rtl_433`, `decoder::pocsag`, `decoder::transcribe`, `decoder::ft8`, `decoder::ais`. **For incremental polling pass `since_ms` = the previous response's `now` field** to get only what's new (the response always echoes a `now` watermark that advances even through quiet stretches). One-shot: `lookback_secs` is a relative window; `limit` is `tail` (newest N). The decoder ring retains ~4096 entries, so without `since_ms`/`lookback_secs`/`limit` you get the whole buffer — set one of them."
    )]
    async fn recent_decodes(
        &self,
        Parameters(args): Parameters<RecentDecodesArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.recent_decodes_op(args).await?)
    }

    #[tool(
        description = "Snapshot one of the four spectrum/waterfall canvases the browser is currently rendering. Writes the PNG to `/tmp/ferrite-views/<pane>-<unix_ms>.png` and returns the path + byte count — band-plan overlay, VFO marker, contrast, pause state, and zoom are all baked in (no re-render from raw FFT). Read the returned path with the `Read` tool to see the image. Requires a UI tab connected to /ws/ui-views; 503 when none. Note: `wide-*` panes are only rendered while `main_pane=wide`; `channel-*` panes only when the active preset has a Channelizer AND `channel_detail_visible=true`. Check via `view_state` if a snapshot returns 404 (canvas not mounted)."
    )]
    async fn view_snapshot(
        &self,
        Parameters(args): Parameters<ViewSnapshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.view_snapshot_op(args).await?)
    }

    #[tool(
        description = "Read the browser's authored UI chrome state — which Main tab is active (FFT/Waterfall or the per-preset advanced view), channel-detail visibility, per-pane zoom + pause. Lets the AI tailor responses to what the operator is actually looking at, without round-tripping through a PNG. 503 when no UI tab is connected."
    )]
    async fn view_state(&self) -> Result<CallToolResult, McpError> {
        ok_json(&self.view_state_op().await?)
    }

    #[tool(
        description = "Start an async IQ capture. Default (`wideband=false`) tees the post-channelizer cf32 stream (block `chan`) non-disruptively — the user's UI session keeps running. Set `wideband=true` for a full-rate `Source → FileIqSink` slice straight off the SDR via the `capture_fm` preset (clobbers the live session; requires `freq_hz`, honours `sample_rate_hz`/`bandwidth_hz`/`gain_db`/`format`). **Returns immediately** with a `job_id`; poll `capture_status(job_id)` for `status=done` and the sidecar JSON. `duration_s` is the wall-clock cap (block enforces it server-side). Optional `freq_hz` retunes first. Defaults `out` to `/tmp/ferrite-captures/iq-<unix_ms>-<freq>mhz.cf32`. The wideband path inherits the live source (antenna/notch/gain), so it refuses against a software/test source (SineSource) — `device select` first, or pass `force:true` to record the synthetic source."
    )]
    async fn start_capture_iq(
        &self,
        Parameters(args): Parameters<StartCaptureArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.start_capture_iq_op(args).await?)
    }

    #[tool(
        description = "Start an async FFT-byte-stream capture against the post-`logmag` u8 spectrum (block `logmag`). Non-disruptive — the UI's waterfall keeps painting while the byte stream side-tees to `output_path`. **Returns immediately** with a `job_id`; poll `capture_status(job_id)` for completion + sidecar. Same live args / defaults as `start_capture_iq` (no wideband) but writes raw `u8` frame bytes (`bin`) plus a sidecar JSON the AI's `find_fft_peaks` / `fft_to_png` python wrappers consume."
    )]
    async fn start_capture_fft(
        &self,
        Parameters(args): Parameters<StartCaptureArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.start_capture_fft_op(args).await?)
    }

    #[tool(
        description = "Start an async demodulated-audio capture to a WAV on disk. Disruptive — swaps in a recording preset (default `fm-audio-record`; also `am-audio-record`, `capture-aprs`, `capture-pager`), tunes to `freq_hz`, and runs a `Source → … → FileAudioSink` slice, so it clobbers the live UI session. **Returns immediately** with a `job_id`; poll `capture_status(job_id)` for `status=done` and the sidecar. `duration_s` defaults to 10; `sample_rate_hz` to 2.4e6; `bandwidth_hz` to the rate. Defaults `out` to `/tmp/ferrite-captures/audio-<unix_ms>-<freq>mhz.wav`. Inherits the live source, so it refuses against a software/test source (SineSource) — `device select` first, or `force:true`."
    )]
    async fn start_capture_audio(
        &self,
        Parameters(args): Parameters<StartCaptureAudioArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.start_capture_audio_op(args).await?)
    }

    #[tool(
        description = "Poll one async capture job by `job_id`, or list every job this MCP session has tracked (most recent first, capped to ~20 entries) when `job_id` is omitted. The reply's `status` is `running` (still recording), `done` (file + sidecar ready on disk; `sidecar` field carries the JSON), or `failed` (with an `error` string). Cheap; safe to poll once a second while a capture is in flight."
    )]
    async fn capture_status(
        &self,
        Parameters(args): Parameters<CaptureStatusArgs>,
    ) -> Result<CallToolResult, McpError> {
        ok_json(&self.capture_status_op(args).await?)
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

#[tool_handler]
impl ServerHandler for FerriteServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_protocol_version(ProtocolVersion::V_2025_03_26)
            .with_instructions(
                "Ferrite SDR control via MCP. Tools wrap the running ferrited's REST API: \
             `status` first (cheap snapshot with `ready`), `list_devices` / `list_presets` for \
             inventory, `source_capabilities` / `list_blocks` to introspect the bound SDR + \
             active pipeline, `select_device` + `load_preset` to set up, `start`/`stop` for \
             lifecycle, `tune` for the dodge-aware listen-frequency change (now also folds in \
             gain/bw/rate), `set_block_param` for everything else, `flowdiag` + \
             `source_readback` to confirm samples are flowing and a gain/AGC change landed, \
             `recent_decodes` to read decoder output, `start_capture_{iq,fft,audio}` + \
             `capture_status` for recordings, `list_captures` + `replay_capture` to drive the \
             full RX chain from a recorded fixture (deterministic testing, no SDR), \
             `view_snapshot` / `view_state` / `set_view_state` to see and steer what the \
             operator is looking at. Source: tools/ferrite-ctl/src/mcp.rs; behaviour matches \
             the REST endpoints in docs/02-protocol.md."
                    .to_string(),
            )
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
