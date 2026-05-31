//! `ferrite-ctl` — CLI driver for a running ferrited.
//!
//! A thin human front-end over the MCP operations layer in
//! [`mcp`]. Every subcommand parses argv, builds the same typed args an
//! MCP tool would receive, calls the matching `FerriteServer::*_op`
//! method, and prints the JSON it returns. The MCP stdio server
//! (`ferrite-ctl mcp`) and this CLI therefore share one implementation
//! per verb — they cannot drift.
//!
//! Output is JSON: pretty-printed by default, one-line compact under
//! `--json` (friendly for `jq -c` / line consumers). Drives the same
//! running ferrited the browser UI uses (`--connect`, defaults to
//! `http://127.0.0.1:10001`).

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{Map, Value};

mod mcp;
mod sdr_tables;

use mcp::{
    BandAtArgs, BlockParamsArgs, CaptureStatusArgs, ChatPostArgs, FerriteServer, Http,
    LoadPresetArgs, PresetDetailArgs, RecentDecodesArgs, ReplayCaptureArgs, SelectDeviceArgs,
    SetViewStateArgs, StartCaptureArgs, StartCaptureAudioArgs, TranscribeArgs, TuneArgs,
    ViewSnapshotArgs,
};

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

    /// Emit compact one-line JSON instead of the pretty-printed
    /// default. Output is JSON either way.
    #[arg(long, global = true)]
    json: bool,

    /// HTTP timeout per request (seconds).
    #[arg(long, default_value_t = 10, global = true)]
    timeout: u64,

    /// Free-form annotation attached to every mutating request as
    /// `X-Ferrite-Note`. The daemon broadcasts it under target
    /// `ai::activity` so the UI's log panel surfaces what's happening
    /// and why ("checking APRS at 144.39 MHz", etc). Read-only polls
    /// don't log, so `tail` / `status` don't flood the stream.
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
    /// bandwidth / sample-rate / gain). Routes through the dodge-aware
    /// tune path.
    Tune {
        /// Centre frequency in Hz. Accepts plain integers or the
        /// `123.45M` / `915k` shorthand.
        freq: String,
        /// Optional bandwidth (Hz, with same shorthand).
        #[arg(long)]
        bw: Option<String>,
        /// Optional sample rate (Hz).
        #[arg(long)]
        rate: Option<String>,
        /// Optional gain (dB). Folds `agc=false` in so the manual
        /// value lands on AGC-defaulting drivers.
        #[arg(long)]
        gain: Option<f64>,
        /// Optional sample-rate floor (Hz) for the tune span.
        #[arg(long)]
        span: Option<String>,
        /// Per-driver DC-spike dodge ratio (HackRF ~0.7; DC-tracking
        /// drivers 0). Must exceed 0.5 when non-zero.
        #[arg(long)]
        offset: Option<f64>,
    },
    /// Patch parameter(s) on a pipeline block. `key=value` pairs;
    /// values parse as JSON when possible (numbers, booleans), else
    /// string. The `src` block accepts gain_db / agc / antenna /
    /// bandwidth_hz / dc_offset_correction / settings.
    Param {
        /// Block id, e.g. `demod`, `audio_nr`, `chan`, `src`.
        block: String,
        /// One or more `key=value` pairs.
        #[arg(value_parser = parse_kv, required = true)]
        kv: Vec<(String, Value)>,
    },
    /// Start the pipeline (instantiates blocks if stopped).
    Start,
    /// Stop the pipeline.
    Stop,
    /// Toggle speech-to-text (VoiceTranscribe tap). Read results with
    /// `decoder recent --category decoder::transcribe`.
    Transcribe {
        /// `on` to enable transcription, `off` to disable.
        #[arg(value_parser = ["on", "off"])]
        state: String,
        /// Where it runs: `node` is headless (whisper in `ferrited`, no
        /// browser); `browser` uses the in-browser worker. Omit to leave
        /// the current placement unchanged.
        #[arg(long, value_parser = ["node", "browser"])]
        placement: Option<String>,
    },
    /// One-shot world snapshot: pipeline status (+ `ready`), source
    /// config, active flowgraph, UI sinks.
    Status,
    /// Capability schema of the currently-bound source (antennas, gain
    /// ladder, rate/bw/freq ranges, driver settings).
    Caps,
    /// Per-block flow diagnostics (throughput / process-time / ring fill)
    /// — the "are samples actually moving?" health check. `null` stopped.
    Flowdiag,
    /// Latest driver readback (actual gain / AGC / antenna / bandwidth as
    /// the hardware reports them). `null` when stopped or non-Soapy.
    Readback,
    /// Every block in the active preset with its full spec + values.
    Blocks,
    /// Every registered block type's schema (static).
    BlockTypes,
    /// List the curated `samples/` tree of replayable IQ/audio fixtures.
    Captures,
    /// Replay a recorded sample as the source (full RX chain via
    /// ModulatedFileSource) — deterministic testing with no SDR.
    Replay {
        /// Sample path (absolute server `path` or `samples/`-relative
        /// `rel` from `captures`), or any server file path.
        path: String,
        /// `iq` or `audio`. Defaults from the capture entry, else audio.
        #[arg(long)]
        kind: Option<String>,
        /// Audio replay carrier: `fm` / `am` / `usb` / `lsb` / `ssb`.
        #[arg(long)]
        modulation: Option<String>,
        /// Sample-rate hint (Hz, SI shorthand). Defaults from the entry.
        #[arg(long)]
        rate: Option<String>,
        /// Centre frequency to claim (Hz, SI shorthand). Defaults entry.
        #[arg(long)]
        center: Option<String>,
    },
    /// What US allocation band(s) contain a frequency.
    BandAt {
        /// Frequency in Hz (SI shorthand OK).
        freq: String,
    },
    /// Capture a slice of signal to disk (IQ / audio / FFT). Blocks
    /// until the async job finishes, then prints the job record.
    #[command(subcommand)]
    Capture(CaptureCmd),
    /// Tail the decoder log buffer until SIGINT.
    Tail {
        /// Tracing-target prefix to filter on (e.g. `decoder::pocsag`,
        /// `decoder` for everything decoder-side).
        #[arg(default_value = "decoder")]
        category: String,
        /// Poll interval in seconds.
        #[arg(long, default_value_t = 1.0)]
        interval: f64,
        /// Pull entries from this many seconds in the past on the
        /// first poll.
        #[arg(long, default_value_t = 0.0)]
        lookback: f64,
    },
    /// Inspect the decoder log buffer (one-shot recall).
    #[command(subcommand)]
    Decoder(DecoderCmd),
    /// Snapshot a spectrum / waterfall canvas to a PNG, print the path.
    /// Requires a UI tab subscribed to `/ws/ui-views`.
    View {
        /// One of `wide-spectrum`, `wide-waterfall`, `channel-spectrum`,
        /// `channel-waterfall`.
        pane: String,
    },
    /// Print the browser's authored chrome state (active tab,
    /// channel-detail visibility, per-pane zoom + pause).
    ViewGet,
    /// Push a chrome-state patch to the operator's browser.
    ViewSet {
        /// `wide` (FFT/Waterfall) or `advanced` (per-preset view).
        #[arg(long)]
        main_pane: Option<String>,
        /// Show/hide the channel-detail pane.
        #[arg(long)]
        channel_detail: Option<bool>,
    },
    /// Mirror a conversation turn into the operator's browser AI panel
    /// (so a headless driver's prompts / replies / tool calls show in
    /// the UI alongside the in-browser assistant). Needs a UI tab open.
    ChatPost {
        /// `user` (a prompt), `assistant` (a reply, auto-closed), or
        /// `tool` (a tool-call card in the open assistant turn).
        #[arg(value_parser = ["user", "assistant", "tool"])]
        role: String,
        /// Message text (user / assistant). Optional for `tool`.
        #[arg(default_value = "")]
        text: String,
        /// `tool` role: the tool name shown on the card.
        #[arg(long)]
        tool_name: Option<String>,
        /// `tool` role: the tool input rendered under the card.
        #[arg(long)]
        tool_input: Option<String>,
        /// `tool` role: the tool result text, if any.
        #[arg(long)]
        tool_result: Option<String>,
    },
    /// Dev codegen: regenerate the if-filter-ladders artifact from the
    /// Rust source of truth. Purely local — no running ferrited.
    #[command(hide = true)]
    GenTables,
    /// Run as an MCP (Model Context Protocol) server over stdio. Exposes
    /// every ferrite-ctl verb as a discrete tool to any MCP-enabled
    /// client. See tools/ferrite-ctl/src/mcp.rs and docs/02-protocol.md.
    Mcp,
}

#[derive(Subcommand, Debug)]
enum CaptureCmd {
    /// Capture IQ. Default = non-disruptive live narrowband (tees the
    /// post-channelizer cf32 stream). `--wideband` = full-rate
    /// `Source → FileIqSink` (clobbers the live preset; needs a freq).
    Iq {
        /// Centre frequency (Hz, SI shorthand). Optional live; required
        /// with `--wideband`.
        freq: Option<String>,
        /// Capture duration in seconds.
        #[arg(long, default_value_t = 5.0)]
        duration: f64,
        /// Sample rate (Hz). `--wideband` only.
        #[arg(long, default_value = "2.0M")]
        rate: String,
        /// Bandwidth (Hz). `--wideband` only.
        #[arg(long)]
        bw: Option<String>,
        /// Front-end gain (dB). `--wideband` only.
        #[arg(long)]
        gain: Option<f64>,
        /// Output path. Defaults under `/tmp/ferrite-captures/`.
        #[arg(long)]
        out: Option<String>,
        /// `cf32` or `wav-s16`. `--wideband` only.
        #[arg(long, default_value = "cf32")]
        format: String,
        /// Opt into the preset-swap wideband capture.
        #[arg(long)]
        wideband: bool,
    },
    /// Capture demodulated audio to disk (preset-swap; disruptive).
    Audio {
        /// Centre frequency (Hz, SI shorthand).
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
        /// Output path. Defaults under `/tmp/ferrite-captures/`.
        #[arg(long)]
        out: Option<String>,
    },
    /// Capture the byte-quantised FFT waterfall (live tee on `logmag`).
    Fft {
        /// Centre frequency (Hz, SI shorthand). Optional.
        freq: Option<String>,
        /// Capture duration in seconds.
        #[arg(long, default_value_t = 5.0)]
        duration: f64,
        /// Output path. Defaults under `/tmp/ferrite-captures/`.
        #[arg(long)]
        out: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
enum DeviceCmd {
    /// List devices the daemon currently sees (warm cache + fresh
    /// enumerate), with full capability schemas.
    List,
    /// Set the source by Soapy-style args, e.g.
    /// `driver=sdrplay,serial=224001D748`. Resets source params to the
    /// driver's defaults; follow with `tune`. Triggers a source restart.
    Select {
        /// Soapy args string (`driver=…,serial=…`).
        args: String,
    },
    /// Recover from an in-process SoapySDR driver wedge — tear down and
    /// re-load every Soapy module. Refuses (409) while running.
    Reload,
}

#[derive(Subcommand, Debug)]
enum PresetCmd {
    /// List presets the daemon can load (name + label + description).
    List,
    /// Load a preset by name (matches `flowgraphs/<name>.json`).
    Load { name: String },
    /// Full JSON of one preset by name (ai_notes, block graph, sigwiki
    /// refs, sample path) — the gotchas + canonical frequencies.
    Detail { name: String },
}

#[derive(Subcommand, Debug)]
enum DecoderCmd {
    /// One-shot recall of recent decoder log entries.
    Recent {
        /// Tracing-target prefix to filter on.
        #[arg(long, default_value = "decoder")]
        category: String,
        /// Pull entries emitted in the last N seconds.
        #[arg(long, default_value_t = 30.0)]
        lookback: f64,
        /// Cap the number of entries printed (newest kept). 0 = no cap.
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

/// Parse 100M / 100k / 100.0 / 100000 — SI suffixes for freq / bw.
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

/// Optional SI parse — `None` stays `None`.
fn parse_si_opt(s: Option<&str>) -> Result<Option<f64>> {
    s.map(parse_si).transpose()
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.headless {
        eprintln!("ferrite-ctl: --headless not implemented yet (Phase 3 work — see plan)");
        return ExitCode::from(2);
    }
    let client = match build_client(cli.timeout) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ferrite-ctl: {e:#}");
            return ExitCode::from(2);
        }
    };
    let base = cli.connect.trim_end_matches('/').to_string();
    let http = Http::new(client, base, cli.note.clone());

    // The MCP server owns stdio for the JSON-RPC stream, so it's handled
    // before constructing the CLI-facing server (which would print).
    if matches!(cli.cmd, Cmd::Mcp) {
        return match mcp::serve(http).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ferrite-ctl: {e:#}");
                ExitCode::from(1)
            }
        };
    }
    if matches!(cli.cmd, Cmd::GenTables) {
        return match sdr_tables::write_web_ladders() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ferrite-ctl: {e:#}");
                ExitCode::from(1)
            }
        };
    }

    let server = FerriteServer::from_http(http);
    match run(cli.cmd, &server, cli.json).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ferrite-ctl: {e:#}");
            ExitCode::from(1)
        }
    }
}

fn build_client(timeout_s: u64) -> Result<reqwest::Client> {
    // Bare client — the ops layer stamps `x-ferrite-command` +
    // `x-ferrite-note` per mutating call (see mcp::Http), so no default
    // activity headers are baked in here.
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_s))
        .build()
        .context("build http client")
}

/// Print a Value: pretty by default, compact under `--json`.
fn emit(v: &Value, compact: bool) -> Result<()> {
    let s = if compact {
        serde_json::to_string(v)?
    } else {
        serde_json::to_string_pretty(v)?
    };
    println!("{s}");
    Ok(())
}

/// Adapt an op's `McpError` into the CLI's `anyhow` error channel.
fn op_err(e: rmcp::ErrorData) -> anyhow::Error {
    anyhow!("{e}")
}

async fn run(cmd: Cmd, server: &FerriteServer, json: bool) -> Result<()> {
    match cmd {
        Cmd::Device(DeviceCmd::List) => {
            emit(&server.list_devices_op().await.map_err(op_err)?, json)
        }
        Cmd::Device(DeviceCmd::Select { args }) => {
            let v = server
                .select_device_op(SelectDeviceArgs { args, note: None })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Device(DeviceCmd::Reload) => {
            emit(&server.reload_drivers_op().await.map_err(op_err)?, json)
        }
        Cmd::Preset(PresetCmd::List) => {
            emit(&server.list_presets_op().await.map_err(op_err)?, json)
        }
        Cmd::Preset(PresetCmd::Load { name }) => {
            let v = server
                .load_preset_op(LoadPresetArgs { name, note: None })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Preset(PresetCmd::Detail { name }) => {
            let v = server
                .preset_detail_op(PresetDetailArgs { name })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Tune {
            freq,
            bw,
            rate,
            gain,
            span,
            offset,
        } => {
            let args = TuneArgs {
                freq_hz: parse_si(&freq)?,
                span_hz: parse_si_opt(span.as_deref())?,
                offset_ratio: offset,
                gain_db: gain,
                bandwidth_hz: parse_si_opt(bw.as_deref())?,
                sample_rate_hz: parse_si_opt(rate.as_deref())?,
                note: None,
            };
            emit(&server.tune_op(args).await.map_err(op_err)?, json)
        }
        Cmd::Param { block, kv } => {
            let mut params = Map::new();
            for (k, v) in kv {
                params.insert(k, v);
            }
            let v = server
                .set_block_param_op(BlockParamsArgs {
                    block,
                    params: Value::Object(params),
                    note: None,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Start => emit(&server.start_op().await.map_err(op_err)?, json),
        Cmd::Stop => emit(&server.stop_op().await.map_err(op_err)?, json),
        Cmd::Transcribe { state, placement } => {
            let v = server
                .transcribe_op(TranscribeArgs {
                    enabled: state == "on",
                    placement,
                    note: None,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Status => emit(&server.status_op().await.map_err(op_err)?, json),
        Cmd::Caps => emit(
            &server.source_capabilities_op().await.map_err(op_err)?,
            json,
        ),
        Cmd::Flowdiag => emit(&server.flowdiag_op().await.map_err(op_err)?, json),
        Cmd::Readback => emit(&server.source_readback_op().await.map_err(op_err)?, json),
        Cmd::Blocks => emit(&server.list_blocks_op().await.map_err(op_err)?, json),
        Cmd::BlockTypes => emit(&server.list_block_types_op().await.map_err(op_err)?, json),
        Cmd::Captures => emit(&server.list_captures_op().await.map_err(op_err)?, json),
        Cmd::Replay {
            path,
            kind,
            modulation,
            rate,
            center,
        } => {
            let v = server
                .replay_capture_op(ReplayCaptureArgs {
                    path,
                    kind,
                    modulation,
                    rate_hz_hint: parse_si_opt(rate.as_deref())?,
                    center_freq_hz: parse_si_opt(center.as_deref())?,
                    loop_playback: None,
                    note: None,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::BandAt { freq } => {
            let v = server
                .band_at_op(BandAtArgs {
                    freq_hz: parse_si(&freq)?,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::Capture(c) => capture(c, server, json).await,
        Cmd::Tail {
            category,
            interval,
            lookback,
        } => tail(&category, interval, lookback, server, json).await,
        Cmd::Decoder(DecoderCmd::Recent {
            category,
            lookback,
            limit,
        }) => {
            let v = server
                .recent_decodes_op(RecentDecodesArgs {
                    category: Some(category),
                    lookback_secs: Some(lookback),
                    limit: if limit == 0 { None } else { Some(limit) },
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::View { pane } => {
            let v = server
                .view_snapshot_op(ViewSnapshotArgs { pane })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::ViewGet => emit(&server.view_state_op().await.map_err(op_err)?, json),
        Cmd::ViewSet {
            main_pane,
            channel_detail,
        } => {
            let v = server
                .set_view_state_op(SetViewStateArgs {
                    main_pane,
                    channel_detail_visible: channel_detail,
                    left_tab: None,
                    note: None,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        Cmd::ChatPost {
            role,
            text,
            tool_name,
            tool_input,
            tool_result,
        } => {
            let v = server
                .chat_post_op(ChatPostArgs {
                    role,
                    text: if text.is_empty() { None } else { Some(text) },
                    tool_name,
                    tool_input,
                    tool_result,
                })
                .await
                .map_err(op_err)?;
            emit(&v, json)
        }
        // Handled in main() before the server is built.
        Cmd::GenTables | Cmd::Mcp => unreachable!("dispatched in main()"),
    }
}

/// Kick an async capture op, then block (polling `capture_status`)
/// until the job leaves `running`, and print the final job record.
async fn capture(cmd: CaptureCmd, server: &FerriteServer, json: bool) -> Result<()> {
    let (job, duration) = match cmd {
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
            let args = StartCaptureArgs {
                duration_s: duration,
                freq_hz: parse_si_opt(freq.as_deref())?,
                out,
                wideband,
                sample_rate_hz: Some(parse_si(&rate)?),
                bandwidth_hz: parse_si_opt(bw.as_deref())?,
                gain_db: gain,
                format: Some(format),
                note: None,
            };
            (
                server.start_capture_iq_op(args).await.map_err(op_err)?,
                duration,
            )
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
            let args = StartCaptureAudioArgs {
                freq_hz: parse_si(&freq)?,
                duration_s: Some(duration),
                sample_rate_hz: Some(parse_si(&rate)?),
                bandwidth_hz: parse_si_opt(bw.as_deref())?,
                gain_db: gain,
                preset: Some(preset),
                out,
                note: None,
            };
            (
                server.start_capture_audio_op(args).await.map_err(op_err)?,
                duration,
            )
        }
        CaptureCmd::Fft {
            freq,
            duration,
            out,
        } => {
            let args = StartCaptureArgs {
                duration_s: duration,
                freq_hz: parse_si_opt(freq.as_deref())?,
                out,
                ..Default::default()
            };
            (
                server.start_capture_fft_op(args).await.map_err(op_err)?,
                duration,
            )
        }
    };

    let Some(job_id) = job.get("job_id").and_then(Value::as_str).map(str::to_owned) else {
        return emit(&job, json);
    };
    if !json {
        eprintln!("ferrite-ctl: capturing for ~{duration:.1}s (job {job_id})…");
    }
    // Poll until done/failed, with a safety ceiling past the duration.
    let deadline = Duration::from_secs_f64(duration + 15.0);
    let started = std::time::Instant::now();
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let st = server
            .capture_status_op(CaptureStatusArgs {
                job_id: Some(job_id.clone()),
            })
            .await
            .map_err(op_err)?;
        let running = st.get("status").and_then(Value::as_str) == Some("running");
        if !running {
            return emit(&st, json);
        }
        if started.elapsed() > deadline {
            return emit(&st, json);
        }
    }
}

/// Tail the decoder buffer: poll `recent_decodes` each interval, print
/// entries newer than the last seen `at_ms`, until SIGINT.
async fn tail(
    category: &str,
    interval: f64,
    lookback: f64,
    server: &FerriteServer,
    json: bool,
) -> Result<()> {
    let dur = Duration::from_secs_f64(interval.max(0.05));
    // First poll seeds the watermark; only emit history if lookback>0.
    let mut seen_at: u64 = 0;
    let mut first = true;

    let interrupt = tokio::signal::ctrl_c();
    let mut interrupt = std::pin::pin!(interrupt);
    loop {
        let lb = if first {
            lookback.max(0.0)
        } else {
            interval * 2.0 + 1.0
        };
        let v = server
            .recent_decodes_op(RecentDecodesArgs {
                category: Some(category.to_string()),
                lookback_secs: Some(lb),
                limit: None,
            })
            .await
            .map_err(op_err)?;
        if let Some(arr) = v.get("entries").and_then(Value::as_array) {
            for e in arr {
                let at = e.get("at_ms").and_then(Value::as_u64).unwrap_or(0);
                let emit_it = if first { lookback > 0.0 } else { at > seen_at };
                if emit_it {
                    print_entry(e, json);
                }
                seen_at = seen_at.max(at);
            }
        }
        first = false;
        tokio::select! {
            _ = &mut interrupt => return Ok(()),
            () = tokio::time::sleep(dur) => {}
        }
    }
}

fn print_entry(e: &Value, json: bool) {
    if json {
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

/// Pull the SoapySDR driver name out of an `args` string. Soapy accepts
/// plain `"sdrplay"` or a kv form `"driver=sdrplay,serial=xxx"`. Returns
/// `""` when neither shape parses. Shared with the tune op's rate→BW
/// auto-derive (`mcp::FerriteServer::tune_op`).
pub(crate) fn driver_key_from_args(args: &str) -> String {
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
    fn parse_si_handles_suffixes() {
        assert!((parse_si("100M").unwrap() - 100e6).abs() < f64::EPSILON);
        assert!((parse_si("915k").unwrap() - 915e3).abs() < f64::EPSILON);
        assert!((parse_si("2.4M").unwrap() - 2.4e6).abs() < 1.0);
        assert!((parse_si("144390000").unwrap() - 144_390_000.0).abs() < f64::EPSILON);
        assert!(parse_si("").is_err());
    }
}
