//! `ferrite-ctl` — CLI driver for a running ferrited.
//!
//! Hits the same REST API the browser UI uses. A human can reach for
//! it to script repetitive setup; an AI agent can drive Ferrite via
//! these subcommands as tool calls.
//!
//! Two modes:
//! - default: connect to a ferrited the user already has running
//!   (`--connect <addr>`, defaults to `http://127.0.0.1:10001`).
//! - `--headless`: not implemented yet (Phase 3 will add the spawn-
//!   our-own-ferrited path); listed in the CLI so the shape is
//!   visible.
//!
//! Output is human-readable by default. `--json` flips every command
//! to one-shot JSON on stdout — the format an AI tool wrapper would
//! parse.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

const DEFAULT_CONNECT: &str = "http://127.0.0.1:10001";

#[derive(Parser, Debug)]
#[command(
    name = "ferrite-ctl",
    version,
    about = "CLI driver for a running ferrited",
    long_about = None,
)]
struct Cli {
    /// Base URL of the ferrited to drive.
    #[arg(long, default_value = DEFAULT_CONNECT, global = true)]
    connect: String,

    /// Spawn an isolated ferrited and drive that — placeholder for
    /// Phase 3; not implemented yet.
    #[arg(long, global = true)]
    headless: bool,

    /// Emit one JSON document per subcommand on stdout. Default is a
    /// human-readable view; the AI uses --json.
    #[arg(long, global = true)]
    json: bool,

    /// HTTP timeout per request (seconds).
    #[arg(long, default_value_t = 10, global = true)]
    timeout: u64,

    /// Free-form annotation attached to every API request as
    /// `X-Ferrite-Note`. The daemon broadcasts it under target
    /// `ai::activity` so the UI's log panel surfaces what the AI is
    /// doing and why ("checking APRS at 144.39 MHz", etc). Mutating
    /// requests log; read-only polls don't, so `tail` and `status`
    /// don't flood the stream.
    #[arg(long, global = true)]
    note: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Manage / inspect SDR devices known to the daemon.
    #[command(subcommand)]
    Device(DeviceCmd),
    /// Browse + load preset flowgraphs.
    #[command(subcommand)]
    Preset(PresetCmd),
    /// Tune the live source's centre frequency (and optionally
    /// bandwidth / sample-rate / gain).
    Tune {
        /// Centre frequency in Hz. Accepts plain integers or the
        /// `123.45M` / `915k` shorthand for human convenience.
        freq: String,
        /// Optional bandwidth (Hz, with same shorthand).
        #[arg(long)]
        bw: Option<String>,
        /// Optional sample rate (Hz).
        #[arg(long)]
        rate: Option<String>,
        /// Optional gain (dB).
        #[arg(long)]
        gain: Option<f64>,
    },
    /// Patch a parameter on a pipeline block. `--key` may be given
    /// multiple times. Values are interpreted as JSON when possible
    /// (numbers, booleans), else as a string.
    Param {
        /// Block id, e.g. `demod`, `audio_nr`, `chan`.
        block: String,
        /// One or more `key=value` pairs.
        #[arg(value_parser = parse_kv, required = true)]
        kv: Vec<(String, Value)>,
    },
    /// Start the pipeline (instantiates blocks if stopped).
    Start,
    /// Stop the pipeline.
    Stop,
    /// Toggle in-browser speech-to-text. Splices a `VoiceTranscribe`
    /// tap before the AudioSink via the runtime profile (`transcribe`
    /// implies `audio`). Whisper runs in the browser, so a UI tab must
    /// be connected for text to flow — set it up here, the browser
    /// produces it. Read the result back with
    /// `decoder recent --category decoder::transcribe`. Use a *listen*
    /// preset with an audio chain (`nbfm`, `usb`, `lsb`, `wbam`), not a
    /// headless `*-record` preset.
    Transcribe {
        /// `on` to enable transcription, `off` to disable.
        #[arg(value_parser = ["on", "off"])]
        state: String,
    },
    /// One-shot snapshot: pipeline status, source config, ui sinks,
    /// active flowgraph name. Useful for the AI's "what's the world
    /// look like" tool.
    Status,
    /// Capture a slice of signal to disk. Uses one of the shipped
    /// capture presets (`capture_fm` for IQ, `capture-aprs` /
    /// `capture-pager` / `am-audio-record` / `fm-audio-record` for
    /// demodulated audio, `capture_fft` for the spectrum byte stream)
    /// and orchestrates: load preset, tune, patch sink params, start,
    /// wait, stop. Pipeline must be stopped going in (the SDR can only
    /// be open in one place); `capture` leaves it stopped on exit so
    /// the user can inspect the file.
    #[command(subcommand)]
    Capture(CaptureCmd),
    /// Tail the decoder log buffer. Polls `/api/decoder/recent` every
    /// `--interval` and prints (or JSON-emits) new entries until
    /// SIGINT. With no `--category`, follows everything decoder-side.
    Tail {
        /// Tracing-target prefix to filter on (e.g. `decoder::pocsag`,
        /// `decoder` to follow every decoder, `ferrited` for daemon
        /// chatter). Defaults to `decoder` since this command's job is
        /// "what is the pipeline decoding."
        #[arg(default_value = "decoder")]
        category: String,
        /// Poll interval in seconds. Smaller = lower latency but more
        /// HTTP traffic; the server keeps ~4096 entries of history so
        /// 1–2 s is comfortable.
        #[arg(long, default_value_t = 1.0)]
        interval: f64,
        /// Pull entries from this many seconds in the past on the
        /// first poll. Default 0 = only new entries from "now" onward.
        #[arg(long, default_value_t = 0.0)]
        lookback: f64,
    },
    /// Inspect the decoder log buffer. `recent` for a one-shot recall
    /// (the AI's "did any decode land?" check), `tail` for streaming.
    #[command(subcommand)]
    Decoder(DecoderCmd),
    /// Snapshot one of the four spectrum / waterfall canvases the
    /// browser is currently rendering, save it as a PNG, and print the
    /// path. Lets the AI "see what the operator sees" — band-plan
    /// overlay, VFO marker, contrast, pause state, zoom, the works —
    /// without re-rendering from raw FFT bins.
    ///
    /// Requires a browser tab subscribed to `/ws/ui-views` (i.e. the
    /// UI is open). Returns 503 if no tab is connected, 504 if the
    /// browser doesn't respond within 3 s.
    View {
        /// Which pane to snapshot. One of `wide-spectrum`,
        /// `wide-waterfall`, `channel-spectrum`, `channel-waterfall`.
        /// `channel-*` panes are only meaningful when the active
        /// preset has a Channelizer (i.e. the channel-detail toggle is
        /// available in the RfQuickBar).
        pane: String,
        /// Output path. Defaults to
        /// `/tmp/ferrite-views/<pane>-<unix_ms>.png`.
        #[arg(long)]
        out: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum CaptureCmd {
    /// Capture IQ to disk. Default mode is **live narrowband**: flips
    /// `chan.record_path` on the running pipeline so the post-channelizer
    /// cf32 stream side-tees to disk while the user's UI session keeps
    /// running. `--wideband` swaps in the `capture_fm` preset for a
    /// full-rate `Source → FileIqSink` slice (clobbers the live preset,
    /// useful for explicit headless captures). The narrowband path
    /// requires the running preset to have a `chan` block.
    Iq {
        /// Centre frequency (Hz, SI shorthand OK). Optional in live
        /// mode (no value = record at the current source freq).
        /// Required with `--wideband` since the wideband preset boots
        /// fresh and needs a freq to tune to.
        freq: Option<String>,
        /// Capture duration in seconds. Drives both the wall-clock
        /// `record_max_seconds` cap on the block and the CLI's
        /// stop-after-sleep nudge.
        #[arg(long, default_value_t = 5.0)]
        duration: f64,
        /// Sample rate (Hz). `--wideband` only — narrowband uses the
        /// running pipeline's rate.
        #[arg(long, default_value = "2.0M")]
        rate: String,
        /// Bandwidth (Hz). `--wideband` only.
        #[arg(long)]
        bw: Option<String>,
        /// Front-end gain (dB). `--wideband` only; live narrowband
        /// inherits the running preset's gain.
        #[arg(long)]
        gain: Option<f64>,
        /// Output path. Defaults to `/tmp/ferrite-captures/iq-<ts>-<freq>.<ext>`.
        #[arg(long)]
        out: Option<String>,
        /// `cf32` or `wav-s16`. `--wideband` only — narrowband always
        /// writes cf32 (the live channelizer output is already cf32).
        #[arg(long, default_value = "cf32")]
        format: String,
        /// Opt into the preset-swap wideband capture. Without this
        /// flag, capture iq is a non-disruptive live-record — flips a
        /// param on the running channelizer, leaves the rest of the
        /// pipeline alone. Use --wideband when you want full-rate
        /// SDR-direct IQ and don't mind clobbering the UI session.
        #[arg(long)]
        wideband: bool,
    },
    /// Capture demodulated audio to disk. Picks a preset that ends in
    /// `FileAudioSink` (`am-audio-record`, `fm-audio-record`,
    /// `capture-aprs`, `capture-pager`). Default `fm-audio-record`.
    Audio {
        /// Centre frequency (Hz, SI shorthand OK).
        freq: String,
        /// Capture duration in seconds.
        #[arg(long, default_value_t = 10.0)]
        duration: f64,
        /// Source rate (Hz).
        #[arg(long, default_value = "2.4M")]
        rate: String,
        /// Bandwidth (Hz). Defaults to the rate.
        #[arg(long)]
        bw: Option<String>,
        /// Gain (dB).
        #[arg(long)]
        gain: Option<f64>,
        /// Preset to use (must end in a FileAudioSink-shaped block).
        #[arg(long, default_value = "fm-audio-record")]
        preset: String,
        /// Output path. Defaults to the preset's built-in path.
        #[arg(long)]
        out: Option<String>,
    },
    /// Capture the byte-quantised FFT waterfall to disk by flipping
    /// `logmag.record_path` on the running pipeline. The user's UI
    /// session keeps moving — the byte stream is teed, not stolen.
    /// Each frame on disk is `frame_size` bytes (one dBFS-mapped
    /// window, fft-shifted, `[0, 255]`); the sidecar records frame
    /// size, sample rate, and centre freq so post-processing can label
    /// the axes. Requires the running preset to have a `logmag` block
    /// (every preset that drives a waterfall has one).
    Fft {
        /// Centre frequency (Hz, SI shorthand OK). Optional — without
        /// it, capture records at the current source freq.
        freq: Option<String>,
        /// Capture duration in seconds.
        #[arg(long, default_value_t = 5.0)]
        duration: f64,
        /// Output path. Defaults to
        /// `/tmp/ferrite-captures/fft-<ts>-<freq>.bin`. Sidecar lands
        /// at the same stem with `.json`.
        #[arg(long)]
        out: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum DeviceCmd {
    /// List devices the daemon currently sees (includes warm cache
    /// entries plus a fresh enumerate).
    List,
    /// Set the source by Soapy-style args, e.g.
    /// `driver=sdrplay,serial=224001D748`. Triggers a source restart.
    Select {
        /// Soapy args string (`driver=…,serial=…`).
        args: String,
    },
}

#[derive(Subcommand, Debug)]
enum PresetCmd {
    /// List presets the daemon can load.
    List,
    /// Load a preset by name (matches `flowgraphs/<name>.json`).
    Load { name: String },
}

#[derive(Subcommand, Debug)]
enum DecoderCmd {
    /// One-shot recall of recent decoder log entries. Lower-friction
    /// substitute for `tail` when you want "what's in the buffer now"
    /// without committing to a streaming tail. Prints what matches
    /// and exits — same output format as `tail`, no SIGINT needed.
    ///
    /// Use this instead of `curl /api/decoder/recent` so the call
    /// flows through `--note`-aware logging and shows up in the
    /// activity panel like every other ferrite-ctl move.
    Recent {
        /// Tracing-target prefix to filter on (e.g.
        /// `decoder::rtl_433`, `decoder::pocsag`, or just `decoder`
        /// for everything decoder-side).
        #[arg(long, default_value = "decoder")]
        category: String,
        /// Pull entries emitted in the last N seconds. Buffer caps at
        /// ~4096 entries so very long lookbacks are bounded by ring
        /// size anyway. Default 30 s.
        #[arg(long, default_value_t = 30.0)]
        lookback: f64,
        /// Cap the number of entries printed (newest kept). 0 = no
        /// cap (print every match). Useful for the AI's "did any
        /// decode land in the last 30 s?" check — `--limit 5` keeps
        /// the output short.
        #[arg(long, default_value_t = 0_usize)]
        limit: usize,
    },
}

fn parse_kv(s: &str) -> Result<(String, Value), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got {s:?}"))?;
    let parsed = serde_json::from_str::<Value>(v).unwrap_or_else(|_| Value::String(v.to_string()));
    Ok((k.to_string(), parsed))
}

/// Parse 100M / 100k / 100.0 / 100000 — accepts SI suffixes for
/// frequency and bandwidth shorthand.
fn parse_si(s: &str) -> Result<f64> {
    let s = s.trim();
    if s.is_empty() {
        bail!("empty number");
    }
    let (num, mul) = match s.chars().last().unwrap() {
        'k' | 'K' => (&s[..s.len() - 1], 1e3),
        'M' => (&s[..s.len() - 1], 1e6),
        'G' => (&s[..s.len() - 1], 1e9),
        _ => (s, 1.0),
    };
    let v: f64 = num.parse().with_context(|| format!("parse {s:?}"))?;
    Ok(v * mul)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.headless {
        eprintln!("ferrite-ctl: --headless not implemented yet (Phase 3 work — see plan)");
        return ExitCode::from(2);
    }
    // Tag every API call with the subcommand we're running + the
    // user-supplied --note. Daemon middleware turns these into
    // `ai::activity` log lines that the UI's log panel surfaces.
    let command = command_summary(&cli.cmd);
    let client = match build_client(cli.timeout, cli.note.as_deref(), command) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ferrite-ctl: {e:#}");
            return ExitCode::from(2);
        }
    };
    let driver = Driver {
        base: cli.connect.trim_end_matches('/').to_string(),
        client,
        json: cli.json,
    };
    match driver.run(cli.cmd).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ferrite-ctl: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn build_client(
    timeout_s: u64,
    note: Option<&str>,
    command: &'static str,
) -> Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(v) = reqwest::header::HeaderValue::from_str(command) {
        headers.insert("x-ferrite-command", v);
    }
    if let Some(n) = note {
        // Header values reject CRLF + non-ASCII control chars; just
        // skip the header silently if the note isn't a clean string
        // rather than crashing the CLI on UTF-8 multibyte that the
        // user thought was OK. (HeaderValue accepts visible ASCII.)
        if let Ok(v) = reqwest::header::HeaderValue::from_str(n) {
            headers.insert("x-ferrite-note", v);
        }
    }
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_s))
        .default_headers(headers)
        .build()
        .context("build http client")
}

/// Map a subcommand to a stable kebab-case identifier the daemon logs
/// as `ai::activity command=<this>`. Lets a UI filter to "just the
/// AI's tunes" or "just its captures" without parsing the path.
fn command_summary(cmd: &Cmd) -> &'static str {
    match cmd {
        Cmd::Device(DeviceCmd::List) => "device-list",
        Cmd::Device(DeviceCmd::Select { .. }) => "device-select",
        Cmd::Preset(PresetCmd::List) => "preset-list",
        Cmd::Preset(PresetCmd::Load { .. }) => "preset-load",
        Cmd::Tune { .. } => "tune",
        Cmd::Param { .. } => "param",
        Cmd::Start => "pipeline-start",
        Cmd::Stop => "pipeline-stop",
        Cmd::Transcribe { .. } => "transcribe",
        Cmd::Status => "status",
        Cmd::Capture(CaptureCmd::Iq { .. }) => "capture-iq",
        Cmd::Capture(CaptureCmd::Audio { .. }) => "capture-audio",
        Cmd::Capture(CaptureCmd::Fft { .. }) => "capture-fft",
        Cmd::Tail { .. } => "tail",
        Cmd::Decoder(DecoderCmd::Recent { .. }) => "decoder-recent",
        Cmd::View { .. } => "view",
    }
}

struct Driver {
    base: String,
    client: reqwest::Client,
    json: bool,
}

impl Driver {
    async fn run(&self, cmd: Cmd) -> Result<()> {
        match cmd {
            Cmd::Device(DeviceCmd::List) => self.devices_list().await,
            Cmd::Device(DeviceCmd::Select { args }) => self.devices_select(&args).await,
            Cmd::Preset(PresetCmd::List) => self.presets_list().await,
            Cmd::Preset(PresetCmd::Load { name }) => self.preset_load(&name).await,
            Cmd::Tune {
                freq,
                bw,
                rate,
                gain,
            } => self.tune(&freq, bw.as_deref(), rate.as_deref(), gain).await,
            Cmd::Param { block, kv } => self.param(&block, kv).await,
            Cmd::Start => self.pipeline_start().await,
            Cmd::Stop => self.pipeline_stop().await,
            Cmd::Transcribe { state } => self.transcribe(&state).await,
            Cmd::Status => self.status().await,
            Cmd::Capture(c) => self.capture(c).await,
            Cmd::Tail {
                category,
                interval,
                lookback,
            } => self.tail(&category, interval, lookback).await,
            Cmd::Decoder(DecoderCmd::Recent {
                category,
                lookback,
                limit,
            }) => self.decoder_recent(&category, lookback, limit).await,
            Cmd::View { pane, out } => self.view(&pane, out.as_deref()).await,
        }
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let resp = self
            .client
            .get(format!("{}{}", self.base, path))
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        api_response(resp).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{}{}", self.base, path))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;
        api_response(resp).await
    }

    async fn patch(&self, path: &str, body: Value) -> Result<Value> {
        let resp = self
            .client
            .patch(format!("{}{}", self.base, path))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("PATCH {path}"))?;
        api_response(resp).await
    }

    async fn devices_list(&self) -> Result<()> {
        let v = self.get("/api/devices").await?;
        if self.json {
            print_json(&v);
        } else if let Some(arr) = v.as_array() {
            println!("{} device(s):", arr.len());
            for d in arr {
                // /api/devices returns the daemon's DeviceCache view —
                // info.args is the SoapySDRKwargs map (driver/label/serial),
                // hardware_key + driver_key are the user-facing identifiers.
                let info_args = d.get("info").and_then(|i| i.get("args"));
                let driver = info_args
                    .and_then(|a| a.get("driver"))
                    .and_then(Value::as_str)
                    .unwrap_or(d.get("driver_key").and_then(Value::as_str).unwrap_or("?"));
                let label = info_args
                    .and_then(|a| a.get("label"))
                    .and_then(Value::as_str)
                    .or_else(|| d.get("hardware_key").and_then(Value::as_str))
                    .unwrap_or("?");
                let serial = info_args
                    .and_then(|a| a.get("serial"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let serial_part = if serial.is_empty() {
                    String::new()
                } else {
                    format!("  serial={serial}")
                };
                println!("  {driver:10} {label}{serial_part}");
            }
        } else {
            print_json(&v);
        }
        Ok(())
    }

    async fn devices_select(&self, args: &str) -> Result<()> {
        // Fetch the current source to preserve params we don't intend
        // to change (centre freq, gain, etc.); only swap the args
        // string + the type to SoapySource.
        let current = self.get("/api/source").await.unwrap_or(Value::Null);
        let mut params = current
            .get("params")
            .cloned()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        params.insert("args".into(), Value::String(args.to_string()));
        let body = json!({ "type": "SoapySource", "params": Value::Object(params) });
        let v = self.patch("/api/source", body).await?;
        if self.json {
            print_json(&v);
        } else {
            println!("source set: SoapySource args={args}");
        }
        Ok(())
    }

    async fn presets_list(&self) -> Result<()> {
        let v = self.get("/api/presets").await?;
        if self.json {
            print_json(&v);
        } else if let Some(arr) = v.as_array() {
            println!("{} preset(s):", arr.len());
            for p in arr {
                let name = p.get("name").and_then(Value::as_str).unwrap_or("?");
                let label = p.get("label").and_then(Value::as_str).unwrap_or(name);
                println!("  {name:24} {label}");
            }
        } else {
            print_json(&v);
        }
        Ok(())
    }

    async fn preset_load(&self, name: &str) -> Result<()> {
        let v = self.post("/api/preset", json!({ "name": name })).await?;
        let status = self.pipeline_status().await;
        if self.json {
            print_json(&v);
        } else {
            println!("preset loaded: {name}");
            self.warn_if_stopped(&status);
        }
        Ok(())
    }

    async fn tune(
        &self,
        freq: &str,
        bw: Option<&str>,
        rate: Option<&str>,
        gain: Option<f64>,
    ) -> Result<()> {
        // PATCH /api/source replaces the whole `SourceConfig {type, params}`,
        // so we read the current one, merge in the new param values,
        // and write it back. That way `tune 144.39M` doesn't drop the
        // device args / antenna / whatever else is set.
        let current = self.get("/api/source").await?;
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
        params.insert("center_freq_hz".into(), json!(parse_si(freq)?));
        if let Some(b) = bw {
            params.insert("bandwidth_hz".into(), json!(parse_si(b)?));
        }
        if let Some(r) = rate {
            let rate_hz = parse_si(r)?;
            params.insert("sample_rate_hz".into(), json!(rate_hz));
            // Auto-derive bandwidth when the caller didn't pin it and
            // the rate changed. Otherwise the previous BW (often a
            // preset hint set for a different rate) sticks — the
            // hardware IF filter brick-walls the spectrum well below
            // the new sample rate. The web UI does the same via
            // `bandwidthForRate(caps, rate)` in optionsModel.ts; we
            // mirror its driver-keyed ladder here.
            if bw.is_none() {
                let driver = params
                    .get("args")
                    .and_then(Value::as_str)
                    .map(driver_key_from_args)
                    .unwrap_or_default();
                if let Some(bw_hz) = recommended_bandwidth_for(&driver, rate_hz) {
                    params.insert("bandwidth_hz".into(), json!(bw_hz));
                }
            }
        }
        if let Some(g) = gain {
            params.insert("gain_db".into(), json!(g));
            // `--gain N` is a manual-gain intent; SDRplay (and a few
            // others) silently ignore manual IFGR while AGC is on and
            // log `Not updating IFGR gain because AGC is enabled`. Fold
            // `agc_enable=false` into the same PATCH so the manual
            // value actually lands — atomic from the API's POV, no
            // operator dance to remember. If the caller really wants
            // AGC on, they pass no `--gain` flag (and the rest of the
            // tune still works fine).
            params.insert("agc_enable".into(), json!(false));
        }
        let body = json!({ "type": type_name, "params": Value::Object(params) });
        let v = self.patch("/api/source", body).await?;
        let status = self.pipeline_status().await;
        if self.json {
            print_json(&v);
        } else {
            println!(
                "tuned: freq={} Hz{}{}{}",
                parse_si(freq)?,
                bw.map(|b| format!(" bw={b}")).unwrap_or_default(),
                rate.map(|r| format!(" rate={r}")).unwrap_or_default(),
                gain.map(|g| format!(" gain={g}dB")).unwrap_or_default(),
            );
            self.warn_if_stopped(&status);
        }
        Ok(())
    }

    async fn param(&self, block: &str, kv: Vec<(String, Value)>) -> Result<()> {
        let mut params = serde_json::Map::new();
        for (k, v) in kv {
            params.insert(k, v);
        }
        let path = format!("/api/pipeline/blocks/{block}/params");
        let v = self.post(&path, Value::Object(params)).await?;
        if self.json {
            print_json(&v);
        } else {
            println!(
                "patched {block}: {} key(s)",
                v.as_object().map_or(0, serde_json::Map::len)
            );
        }
        Ok(())
    }

    async fn transcribe(&self, state: &str) -> Result<()> {
        // PATCH /api/profile replaces the whole profile, so read the
        // current one and flip just `transcribe` (forcing `audio` on
        // when enabling — the tap rides the audio chain). `off` leaves
        // `audio` / `demod_placement` as they were.
        let on = state == "on";
        let cur = self.get("/api/profile").await?;
        let audio = if on {
            true
        } else {
            cur.get("audio").and_then(Value::as_bool).unwrap_or(true)
        };
        let demod = cur.get("demod_placement").cloned().unwrap_or(Value::Null);
        let body = json!({
            "audio": audio,
            "transcribe": on,
            "demod_placement": demod,
        });
        let v = self.patch("/api/profile", body).await?;
        let status = self.pipeline_status().await;
        if self.json {
            print_json(&v);
        } else {
            println!(
                "transcribe {}: profile audio={audio} transcribe={on} \u{2014} \
                 whisper runs in the browser (a UI tab must be connected); \
                 read it back with `decoder recent --category decoder::transcribe`",
                if on { "ON" } else { "OFF" },
            );
            self.warn_if_stopped(&status);
        }
        Ok(())
    }

    async fn pipeline_start(&self) -> Result<()> {
        let v = self.post("/api/pipeline/start", json!({})).await?;
        if self.json {
            print_json(&v);
        } else {
            println!("pipeline: start ok");
        }
        Ok(())
    }

    async fn pipeline_stop(&self) -> Result<()> {
        let v = self.post("/api/pipeline/stop", json!({})).await?;
        if self.json {
            print_json(&v);
        } else {
            println!("pipeline: stop ok");
        }
        Ok(())
    }

    async fn capture(&self, cmd: CaptureCmd) -> Result<()> {
        match cmd {
            CaptureCmd::Iq {
                freq,
                duration,
                rate,
                bw,
                gain,
                out,
                format,
                wideband,
            } => {
                if wideband {
                    self.capture_iq_wideband(freq, duration, rate, bw, gain, out, format)
                        .await
                } else {
                    self.capture_iq_live(freq, duration, out).await
                }
            }
            CaptureCmd::Audio {
                freq,
                duration,
                rate,
                bw,
                gain,
                preset,
                out,
            } => {
                let freq_hz = parse_si(&freq)?;
                let rate_hz = parse_si(&rate)?;
                let bw_hz = match bw {
                    Some(s) => parse_si(&s)?,
                    None => rate_hz,
                };
                let path = out
                    .clone()
                    .unwrap_or_else(|| default_capture_path("audio", freq_hz, "wav"));
                ensure_parent_dir(&path)?;

                self.preset_load_quiet(&preset).await?;
                self.patch_source_params(json!({
                    "center_freq_hz": freq_hz,
                    "sample_rate_hz": rate_hz,
                    "bandwidth_hz": bw_hz,
                    "gain_db": gain,
                }))
                .await?;
                self.patch_flowgraph_block_params(
                    "audio",
                    json!({
                        "path": &path,
                        "max_seconds": duration,
                    }),
                )
                .await?;
                self.run_capture(duration, "audio", &path).await
            }
            CaptureCmd::Fft {
                freq,
                duration,
                out,
            } => self.capture_fft_live(freq, duration, out).await,
        }
    }

    /// Wideband IQ capture via the `capture_fm` preset's
    /// `Source → FileIqSink` chain. Freq is required because this path
    /// boots a fresh preset and the source needs a tune target. Stomps
    /// whatever was running — useful for explicit headless captures.
    #[allow(clippy::too_many_arguments)] // CLI flag fan-out; refactoring into a struct would add ceremony without clarifying anything.
    async fn capture_iq_wideband(
        &self,
        freq: Option<String>,
        duration: f64,
        rate: String,
        bw: Option<String>,
        gain: Option<f64>,
        out: Option<String>,
        format: String,
    ) -> Result<()> {
        let freq = freq.ok_or_else(|| {
            anyhow!("--wideband requires a freq argument (the preset boots fresh)")
        })?;
        let freq_hz = parse_si(&freq)?;
        let rate_hz = parse_si(&rate)?;
        let bw_hz = match bw {
            Some(s) => parse_si(&s)?,
            None => rate_hz,
        };
        let ext = if format == "wav-s16" { "wav" } else { "cf32" };
        let path = out.unwrap_or_else(|| default_capture_path("iq", freq_hz, ext));
        ensure_parent_dir(&path)?;

        self.preset_load_quiet("capture_fm").await?;
        self.patch_source_params(json!({
            "center_freq_hz": freq_hz,
            "sample_rate_hz": rate_hz,
            "bandwidth_hz": bw_hz,
            "gain_db": gain,
        }))
        .await?;
        self.patch_flowgraph_block_params(
            "cap",
            json!({
                "path": &path,
                "format": &format,
                "rate_hz": rate_hz,
                "center_freq_hz": freq_hz,
                "max_seconds": duration,
                "write_sidecar": true,
            }),
        )
        .await?;
        self.run_capture(duration, "iq", &path).await
    }

    /// Live narrowband IQ capture — flips `chan.record_path` on the
    /// running pipeline. Non-disruptive: the user's UI session keeps
    /// driving audio + waterfall while the post-channelizer cf32
    /// stream side-tees to disk.
    async fn capture_iq_live(
        &self,
        freq: Option<String>,
        duration: f64,
        out: Option<String>,
    ) -> Result<()> {
        if let Some(f) = freq.as_ref() {
            self.tune(f, None, None, None).await?;
        }
        let center_hz = self.current_source_freq_hz().await?;
        let path = out.unwrap_or_else(|| default_capture_path("iq", center_hz, "cf32"));
        ensure_parent_dir(&path)?;
        self.live_record("chan", "iq", &path, duration).await
    }

    /// Live FFT-byte-stream capture — flips `logmag.record_path` on
    /// the running pipeline. The waterfall keeps moving in the UI; the
    /// AI gets the same byte stream a tee away.
    async fn capture_fft_live(
        &self,
        freq: Option<String>,
        duration: f64,
        out: Option<String>,
    ) -> Result<()> {
        if let Some(f) = freq.as_ref() {
            self.tune(f, None, None, None).await?;
        }
        let center_hz = self.current_source_freq_hz().await?;
        let path = out.unwrap_or_else(|| default_capture_path("fft", center_hz, "bin"));
        ensure_parent_dir(&path)?;
        self.live_record("logmag", "fft", &path, duration).await
    }

    /// Common live-record shape: patch the block's `record_path` and
    /// `record_max_seconds`, wait, then patch `record_path=""` to
    /// finalise. The block's wall-clock cap inside ferrited will
    /// usually fire first; the empty-path patch is the belt-and-braces
    /// stop in case the block missed a tick or the cap was 0.
    async fn live_record(
        &self,
        block_id: &str,
        kind: &str,
        path: &str,
        duration: f64,
    ) -> Result<()> {
        // Live captures need a running pipeline — the live-param patch
        // hits a block instance, which only exists while sampling. If
        // the user / AI calls capture against a stopped pipeline, just
        // start it ourselves and log the auto-start so they know what
        // happened. Avoids the "AI forgot to start the SDR" footgun.
        let initial_status = self.pipeline_status().await;
        let auto_started = if initial_status == "running" {
            false
        } else {
            if !self.json {
                eprintln!(
                    "ferrite-ctl: pipeline was {} — auto-starting before {} capture",
                    initial_status.to_uppercase(),
                    kind,
                );
            }
            self.post("/api/pipeline/start", json!({}))
                .await
                .with_context(|| format!("auto-start pipeline before {kind} capture"))?;
            // Give the source a beat to begin streaming so the first
            // recorded frame isn't mid-init garbage.
            tokio::time::sleep(Duration::from_millis(300)).await;
            true
        };

        let start = json!({
            "record_path": path,
            "record_max_seconds": duration,
        });
        self.post(
            &format!("/api/pipeline/blocks/{block_id}/params"),
            start,
        )
        .await
        .with_context(|| {
            format!(
                "start recording on `{block_id}` (block missing from current preset? expected on a flowgraph with `{block_id}` declared)"
            )
        })?;

        if !self.json {
            let suffix = if auto_started {
                " (pipeline auto-started)"
            } else {
                ""
            };
            eprintln!("ferrite-ctl: live-recording {kind} for {duration:.1} s -> {path}{suffix}");
        }
        tokio::time::sleep(Duration::from_secs_f64(duration + 0.5)).await;

        // Stop is idempotent — even if the block's own cap fired and
        // already cleared its state, sending record_path="" is a no-op
        // on the daemon side.
        let _ = self
            .post(
                &format!("/api/pipeline/blocks/{block_id}/params"),
                json!({ "record_path": "" }),
            )
            .await;

        if self.json {
            print_json(&json!({
                "kind": kind,
                "path": path,
                "duration_s": duration,
                "mode": "live",
            }));
        } else {
            println!("captured {kind} (live): {path}");
        }
        Ok(())
    }

    async fn current_source_freq_hz(&self) -> Result<f64> {
        let v = self.get("/api/source").await?;
        v.get("params")
            .and_then(|p| p.get("center_freq_hz"))
            .and_then(Value::as_f64)
            .ok_or_else(|| anyhow!("source params missing `center_freq_hz`"))
    }

    async fn run_capture(&self, duration: f64, kind: &str, path: &str) -> Result<()> {
        // Start, sleep duration + a buffer for the source to actually
        // begin streaming, then stop. We respect the FileIqSink /
        // FileAudioSink `max_seconds` so the file caps even if our
        // sleep overshoots.
        self.post("/api/pipeline/start", json!({})).await?;
        let total = Duration::from_secs_f64(duration + 0.5);
        if !self.json {
            eprintln!("ferrite-ctl: capturing {kind} for {duration:.1} s -> {path}");
        }
        tokio::time::sleep(total).await;
        // Best-effort stop. If the pipeline has already torn itself
        // down (pre-Phase 3 it doesn't, but the FileIqSink emits a
        // stop request when max_seconds fires once that lands), a
        // 200 idempotent reply is fine.
        let _ = self.post("/api/pipeline/stop", json!({})).await;
        if self.json {
            print_json(&json!({
                "kind": kind,
                "path": path,
                "duration_s": duration,
            }));
        } else {
            println!("captured {kind}: {path}");
        }
        Ok(())
    }

    async fn preset_load_quiet(&self, name: &str) -> Result<()> {
        self.post("/api/preset", json!({ "name": name }))
            .await
            .map(|_| ())
    }

    /// Apply a delta to the current source's params, preserving the
    /// existing `type` and any params not in the delta. Drops `null`
    /// keys so callers can pass `gain_db: None` without nulling.
    async fn patch_source_params(&self, delta: Value) -> Result<()> {
        let current = self.get("/api/source").await?;
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
        if let Some(delta_obj) = delta.as_object() {
            for (k, v) in delta_obj {
                if v.is_null() {
                    continue;
                }
                params.insert(k.clone(), v.clone());
            }
        }
        let body = json!({ "type": type_name, "params": Value::Object(params) });
        self.patch("/api/source", body).await.map(|_| ())
    }

    /// Edit a single block's params in the active flowgraph doc (read-
    /// modify-write via PATCH /api/flowgraph) — applies before
    /// `pipeline start`, so the very first instantiation sees the new
    /// values. Different from POST /api/pipeline/blocks/:id/params,
    /// which requires the pipeline to be running.
    async fn patch_flowgraph_block_params(&self, block_id: &str, delta: Value) -> Result<()> {
        let mut doc = self.get("/api/flowgraph").await?;
        let blocks = doc
            .get_mut("blocks")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow!("flowgraph has no `blocks`"))?;
        let block = blocks
            .get_mut(block_id)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow!("flowgraph has no block `{block_id}`"))?;
        let params = block
            .entry("params".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()))
            .as_object_mut()
            .ok_or_else(|| anyhow!("`{block_id}.params` is not an object"))?;
        if let Some(delta_obj) = delta.as_object() {
            for (k, v) in delta_obj {
                params.insert(k.clone(), v.clone());
            }
        }
        self.patch("/api/flowgraph", doc).await.map(|_| ())
    }

    async fn tail(&self, category: &str, interval: f64, lookback: f64) -> Result<()> {
        // Watermark advances strictly forward — initialise from the
        // server's `now` (minus optional lookback) to avoid replaying
        // the entire ring on the first poll.
        let initial = self
            .get(&format!("/api/decoder/recent?since=0&category={category}"))
            .await?;
        let server_now = initial.get("now").and_then(Value::as_u64).unwrap_or(0);
        // CLI-supplied lookback in seconds; the * 1000 fits easily in u64
        // for any sane window. Negative inputs are clamped to 0 by max().
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lookback_ms = (lookback.max(0.0) * 1000.0) as u64;
        let mut since = server_now.saturating_sub(lookback_ms);

        // If lookback was nonzero, seed the output with the entries we
        // just pulled that fall inside the window.
        if lookback > 0.0 {
            if let Some(arr) = initial.get("entries").and_then(Value::as_array) {
                for e in arr {
                    self.emit_log_entry(e);
                }
            }
        }

        // Honour SIGINT cleanly so the AI / scripts can stop the tail
        // without leaving a dangling HTTP connection.
        let interrupt = tokio::signal::ctrl_c();
        let mut interrupt = std::pin::pin!(interrupt);

        let dur = Duration::from_secs_f64(interval.max(0.05));
        loop {
            tokio::select! {
                _ = &mut interrupt => return Ok(()),
                () = tokio::time::sleep(dur) => {}
            }
            let path = format!("/api/decoder/recent?since={since}&category={category}");
            let v = self.get(&path).await?;
            if let Some(now) = v.get("now").and_then(Value::as_u64) {
                since = now;
            }
            if let Some(arr) = v.get("entries").and_then(Value::as_array) {
                for e in arr {
                    if let Some(at) = e.get("at_ms").and_then(Value::as_u64) {
                        since = since.max(at);
                    }
                    self.emit_log_entry(e);
                }
            }
        }
    }

    fn emit_log_entry(&self, e: &Value) {
        if self.json {
            // One JSON object per line — friendly for `jq -c` and AI
            // streaming consumers.
            if let Ok(s) = serde_json::to_string(e) {
                println!("{s}");
            }
        } else {
            let target = e.get("target").and_then(Value::as_str).unwrap_or("?");
            let level = e.get("level").and_then(Value::as_str).unwrap_or("?");
            let msg = e.get("message").and_then(Value::as_str).unwrap_or("");
            println!("[{level}] {target}: {msg}");
        }
    }

    async fn decoder_recent(&self, category: &str, lookback: f64, limit: usize) -> Result<()> {
        // Two-step server polling so the `since` watermark is anchored
        // to server-side time, not client-side. Same shape `tail` uses
        // for its first poll.
        let initial = self
            .get(&format!("/api/decoder/recent?since=0&category={category}"))
            .await?;
        let server_now = initial.get("now").and_then(Value::as_u64).unwrap_or(0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lookback_ms = (lookback.max(0.0) * 1000.0) as u64;
        let since = server_now.saturating_sub(lookback_ms);
        let v = self
            .get(&format!(
                "/api/decoder/recent?since={since}&category={category}"
            ))
            .await?;
        let entries = v
            .get("entries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let slice: Vec<&Value> = if limit == 0 {
            entries.iter().collect()
        } else {
            // Newest-kept: take the last `limit` items (the server
            // emits entries in oldest-first order).
            let start = entries.len().saturating_sub(limit);
            entries[start..].iter().collect()
        };
        for e in slice {
            self.emit_log_entry(e);
        }
        Ok(())
    }

    async fn view(&self, pane: &str, out: Option<&str>) -> Result<()> {
        // Validate pane locally so a typo doesn't round-trip to the
        // server. The list mirrors `KNOWN_PANES` in routes.rs.
        const KNOWN_PANES: &[&str] = &[
            "wide-spectrum",
            "wide-waterfall",
            "channel-spectrum",
            "channel-waterfall",
        ];
        if !KNOWN_PANES.contains(&pane) {
            anyhow::bail!("unknown pane {pane:?}; expected one of {KNOWN_PANES:?}");
        }

        let resp = self
            .client
            .get(format!("{}/api/ui-views/{pane}", self.base))
            .send()
            .await
            .with_context(|| format!("GET /api/ui-views/{pane}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("ferrited returned {status}: {body}");
        }
        let bytes = resp
            .bytes()
            .await
            .context("reading PNG body from ferrited")?;

        // Resolve output path. Default mirrors capture-fft: a
        // dedicated /tmp dir, file named per-pane with a millisecond
        // timestamp so two views taken back-to-back don't clobber
        // each other.
        let out_path = match out {
            Some(p) => std::path::PathBuf::from(p),
            None => {
                let dir = std::path::PathBuf::from("/tmp/ferrite-views");
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("mkdir {}", dir.display()))?;
                let ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0);
                dir.join(format!("{pane}-{ms}.png"))
            }
        };
        std::fs::write(&out_path, &bytes)
            .with_context(|| format!("write {}", out_path.display()))?;
        println!("{}", out_path.display());
        Ok(())
    }

    async fn status(&self) -> Result<()> {
        // Compose from a few endpoints so the AI gets one digest.
        let pipeline = self.get("/api/pipeline").await.unwrap_or(Value::Null);
        let source = self.get("/api/source").await.unwrap_or(Value::Null);
        let flowgraph = self.get("/api/flowgraph").await.unwrap_or(Value::Null);
        let ui_sinks = self.get("/api/ui-sinks").await.unwrap_or(Value::Null);
        let status_str = pipeline
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let ready = status_str == "running";
        let snap = json!({
            "pipeline": pipeline,
            "source": source,
            "flowgraph": flowgraph,
            "ui_sinks": ui_sinks,
            // Single boolean for AI / scripts to gate "is the SDR
            // actually sampling right now?" without re-checking the
            // string. `pipeline.status == "running"` is the source of
            // truth; this just hoists it.
            "ready": ready,
        });
        if self.json {
            print_json(&snap);
        } else {
            let preset = flowgraph
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            // /api/source returns `{type, params: {...}}` — the
            // human-interesting fields live on params.
            let source_type = source
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let params = source.get("params");
            let freq = params
                .and_then(|p| p.get("center_freq_hz"))
                .and_then(Value::as_f64);
            let rate = params
                .and_then(|p| p.get("sample_rate_hz"))
                .and_then(Value::as_f64);
            // Prominent state line — uppercase + a hint when stopped
            // so an autonomous AI doesn't have to parse "running"
            // out of a quiet line.
            if ready {
                println!("pipeline:  RUNNING");
            } else {
                println!(
                    "pipeline:  {} — run `ferrite-ctl start` to begin sampling",
                    status_str.to_uppercase(),
                );
            }
            println!("preset:    {preset}");
            print!("source:    {source_type}");
            if let (Some(f), Some(r)) = (freq, rate) {
                println!(" — {:.4} MHz @ {:.3} MS/s", f / 1e6, r / 1e6);
            } else {
                println!();
            }
        }
        Ok(())
    }

    /// Read the pipeline's current status string. Used by the
    /// auto-start guards on `tune`, `preset load`, and the live
    /// captures so the CLI can warn (or auto-start) when the user
    /// drives a command that needs a running pipeline against a
    /// stopped one. Treats network errors as "unknown" rather than
    /// failing the outer command — the underlying action is what
    /// surfaces the real error.
    async fn pipeline_status(&self) -> String {
        self.get("/api/pipeline")
            .await
            .ok()
            .and_then(|v| v.get("status").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Append a one-line state hint to a successful action's output
    /// when the pipeline isn't running. Skipped under `--json` so the
    /// machine-readable shape stays clean — JSON consumers should call
    /// `status` if they care about ready state.
    fn warn_if_stopped(&self, status: &str) {
        if self.json || status == "running" {
            return;
        }
        eprintln!(
            "ferrite-ctl: heads up — pipeline is {} (no samples flowing); run `ferrite-ctl start` when ready",
            status.to_uppercase(),
        );
    }
}

async fn api_response(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let text = resp.text().await.context("read response body as text")?;
    if status.is_success() {
        if text.is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(&text).context("parse response JSON");
    }
    // ferrited returns `{"error":{"code":"...", "message":"..."}}` on
    // 4xx/5xx. Surface both pieces.
    let parsed: Result<Value> = serde_json::from_str(&text).context("parse error body JSON");
    if let Ok(v) = parsed {
        if let Some(err) = v.get("error") {
            let code = err.get("code").and_then(Value::as_str).unwrap_or("UNKNOWN");
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or(text.as_str());
            return Err(anyhow!("HTTP {status} [{code}]: {msg}"));
        }
    }
    Err(anyhow!("HTTP {status}: {text}"))
}

fn print_json(v: &Value) {
    match serde_json::to_string_pretty(v) {
        Ok(s) => println!("{s}"),
        Err(e) => eprintln!("ferrite-ctl: json print error: {e}"),
    }
}

/// Mint a default capture filename under `/tmp/ferrite-captures/` —
/// timestamp + signal kind + frequency in MHz, with the requested
/// extension. Lets the AI re-run captures without colliding paths.
fn default_capture_path(kind: &str, freq_hz: f64, ext: &str) -> String {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mhz = freq_hz / 1e6;
    format!("/tmp/ferrite-captures/{kind}-{ts}-{mhz:.4}MHz.{ext}")
}

fn ensure_parent_dir(path: &str) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create parent dir {}", parent.display()))?;
        }
    }
    Ok(())
}

/// Pull the SoapySDR driver name out of an `args` string. Soapy
/// accepts plain `"sdrplay"` (the driver name alone) or a kv form
/// like `"driver=sdrplay,serial=xxx"`. Returns `""` when neither
/// shape parses.
fn driver_key_from_args(args: &str) -> String {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some((_, rest)) = trimmed.split_once("driver=") {
        return rest.split(&[',', ' ']).next().unwrap_or("").to_string();
    }
    if !trimmed.contains('=') && !trimmed.contains(',') {
        return trimmed.to_string();
    }
    String::new()
}

/// Largest IF-filter ladder entry ≤ `rate_hz` for the given driver,
/// matching the rule the web UI's `bandwidthForRate` uses. Returns
/// `None` when the driver has no known ladder — the web side falls
/// back to the device probe in that case; we just leave bandwidth
/// alone so the driver keeps whatever it already had.
///
/// **DUPLICATE-OF**: `web/src/lib/controls/sdr-presets/<driver>.json`
/// `if_filter_ladder_hz`. TODO: move the per-driver ladder data into
/// a shared file the server, the CLI, and the web all read, instead
/// of three copies in three languages.
fn recommended_bandwidth_for(driver: &str, rate_hz: f64) -> Option<f64> {
    let ladder: &[f64] = match driver {
        "sdrplay" => &[
            200_000.0,
            300_000.0,
            600_000.0,
            1_536_000.0,
            5_000_000.0,
            6_000_000.0,
            7_000_000.0,
            8_000_000.0,
        ],
        _ => return None,
    };
    ladder.iter().rev().find(|&&x| x <= rate_hz).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_key_from_args_handles_both_shapes() {
        assert_eq!(driver_key_from_args("sdrplay"), "sdrplay");
        assert_eq!(driver_key_from_args("driver=sdrplay"), "sdrplay");
        assert_eq!(
            driver_key_from_args("driver=sdrplay,serial=224001D748"),
            "sdrplay"
        );
        assert_eq!(driver_key_from_args(""), "");
        assert_eq!(driver_key_from_args("   "), "");
    }

    #[test]
    fn recommended_bandwidth_picks_largest_le_rate() {
        // Mirrors the web side's `bandwidthForRate` for the SDRplay
        // ladder. Anything below the smallest ladder entry returns
        // None (no entry ≤ rate).
        assert_eq!(
            recommended_bandwidth_for("sdrplay", 10_000_000.0),
            Some(8_000_000.0)
        );
        assert_eq!(
            recommended_bandwidth_for("sdrplay", 8_000_000.0),
            Some(8_000_000.0)
        );
        assert_eq!(
            recommended_bandwidth_for("sdrplay", 6_000_000.0),
            Some(6_000_000.0)
        );
        assert_eq!(
            recommended_bandwidth_for("sdrplay", 2_000_000.0),
            Some(1_536_000.0)
        );
        assert_eq!(recommended_bandwidth_for("sdrplay", 100_000.0), None);
        assert_eq!(recommended_bandwidth_for("rtlsdr", 2_000_000.0), None);
    }
}
