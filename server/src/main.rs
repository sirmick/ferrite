//! `ferrited` — Ferrite SDR daemon.
//!
//! Binds to `0.0.0.0:8088`, serves the REST API under `/api`, the
//! preset WebSocket stream under `/ws/preset`, and the SvelteKit
//! static bundle from `FERRITE_STATIC_ROOT` (defaults to `./web-dist`).
//! Every response carries COOP/COEP so the browser grants
//! `SharedArrayBuffer` access.
//!
//! Startup contract: `--flowgraph <path>` is required and loads a
//! preset doc. `--source <path>` or the inline `--source-type` /
//! `--source-args` / `--antenna` / `--gain-db` / `--agc` /
//! `--source-bandwidth-hz` flags build the initial `SourceConfig`
//! (defaults to a harmless SineSource). `--start` spawns the pipeline
//! at boot; otherwise the UI flips it on via `POST /api/pipeline/start`.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use axum::{
    routing::{get, post},
    Router,
};
use clap::Parser;
use ferrite_runtime::{FlowgraphDoc, SourceConfig};
use http::{HeaderName, HeaderValue};
use serde_json::{json, Map, Value};
use tower_http::{
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod app_state;
mod band_plan;
mod block_schema;
mod bridge_sink;
mod decoder_store;
mod device;
mod device_cache;
mod frame_bus;
mod log_stream;
mod preset_pipeline;
mod routes;
mod soapy_log;
mod source_policy;
mod view_bridge;

/// Ferrite SDR daemon.
#[derive(Parser, Debug)]
#[command(name = "ferrited", version)]
struct Args {
    /// Address to bind the HTTP/WS server to.
    #[arg(long, default_value = "0.0.0.0:8088")]
    bind: String,

    /// Flowgraph preset JSON. Required for the server/headless modes;
    /// not used by `--list-devices` or `--probe-device`. The `src` block
    /// must be a `Source` placeholder (see `runtime::compose`); the
    /// concrete source is supplied separately via `--source` or inline
    /// flags.
    #[arg(long = "flowgraph", value_name = "PATH")]
    flowgraph: Option<PathBuf>,

    /// Optional `SourceConfig` JSON — overrides any inline `--source-*`
    /// flags. The file body is deserialised as `SourceConfig`.
    #[arg(long = "source", value_name = "PATH")]
    source: Option<PathBuf>,

    /// Source block type name (e.g. `SoapySource`, `SineSource`,
    /// `FileSource`). Ignored when `--source` is set. Defaults to
    /// `SineSource` so a source-less invocation still composes into
    /// a runnable graph.
    #[arg(long = "source-type", default_value = "SineSource")]
    source_type: String,

    /// Soapy `args` string (e.g. `driver=rtlsdr,serial=0001`). Written
    /// into `SourceConfig.params.args` when supplied.
    #[arg(long = "source-args")]
    source_args: Option<String>,

    /// Soapy antenna label. Written into `SourceConfig.params.antenna`
    /// when supplied.
    #[arg(long = "antenna")]
    antenna: Option<String>,

    /// Soapy manual gain, dB. Written into `SourceConfig.params.gain_db`.
    #[arg(long = "gain-db")]
    gain_db: Option<f64>,

    /// Soapy AGC toggle.
    #[arg(long = "agc")]
    agc: Option<bool>,

    /// Soapy RF bandwidth, Hz.
    #[arg(long = "source-bandwidth-hz")]
    source_bandwidth_hz: Option<f64>,

    /// File path for `FileSource` / Soapy capture replay.
    #[arg(long = "source-path")]
    source_path: Option<PathBuf>,

    /// Loop the file on EOF (file source only).
    #[arg(long = "loop", default_value_t = false)]
    loop_playback: bool,

    /// Auto-start the pipeline on boot instead of waiting for the UI
    /// to `POST /api/pipeline/start`.
    #[arg(long = "start", default_value_t = false)]
    auto_start: bool,

    /// Directory scanned by `GET /api/presets` and resolved by
    /// `POST /api/preset`. When unset the browse endpoint returns an
    /// empty list and the swap endpoint returns an error. Defaults to
    /// the parent directory of `--flowgraph` when that's a `.json` file
    /// inside a sibling `flowgraphs/` dir — the common dev layout.
    #[arg(long = "presets-dir", value_name = "PATH")]
    presets_dir: Option<PathBuf>,

    /// Run the corpus cross-reference validator over the presets
    /// directory at boot and `warn!` every finding (same checks as the
    /// `flowgraph_corpus` PR gate, minus asset-file existence which
    /// needs the repo `samples/` tree). Opt-in; off by default so a
    /// deployed binary doesn't scan on every start.
    #[arg(long = "validate-presets", default_value_t = false)]
    validate_presets: bool,

    /// Runtime tick period, in microseconds. Defaults to 400µs
    /// (2.5kHz) which is fine for any source at 2MHz or below given
    /// `DEFAULT_FRAMES_HINT` of 1024 samples per tick.
    #[arg(long = "tick-period-us", default_value_t = 400)]
    tick_period_us: u64,

    /// Enumerate `SoapySDR` devices and exit. Requires the `soapysdr`
    /// feature at build time.
    #[arg(long = "list-devices", default_value_t = false)]
    list_devices: bool,

    /// Run the pipeline for this many seconds and exit — no HTTP/WS
    /// server is started. Implies `--start`. Intended for headless
    /// recording presets (`fm-audio-record`, `capture_fm`, …) driven
    /// straight from the CLI. `Ctrl-C` before the timer elapses also
    /// triggers a graceful stop.
    #[arg(long = "run-for-secs", value_name = "SECS")]
    run_for_secs: Option<f64>,

    /// Probe one device's capabilities and exit.
    #[arg(long = "probe-device", value_name = "ARGS")]
    probe_device: Option<String>,

    /// Enumerate every `SoapySDR` device, probe each one, and print the
    /// full capability schema (including driver settings) to stdout
    /// before exiting. Same per-device output as `--probe-device <args>`,
    /// looped over the enumeration. Intended as a CLI debug tool —
    /// confirms what the server-side capability cache would see for
    /// every attached device.
    #[arg(long = "probe-all", default_value_t = false)]
    probe_all: bool,

    /// Enumerate every `SoapySDR` device, probe it, open a short Rx
    /// stream, and print sample-count / magnitude stats for each one.
    /// Intended as a CLI smoke test — confirms the device not only
    /// opens and advertises capabilities but also produces samples.
    #[arg(long = "read-all", default_value_t = false)]
    read_all: bool,

    /// Duration of the per-device sampling window used by `--read-all`.
    /// Defaults to 1s, which is long enough for every driver we care
    /// about to stabilise and return several million samples at the
    /// target 2 Ms/s.
    #[arg(long = "read-secs", default_value_t = 1.0)]
    read_secs: f64,

    /// Restrict `--read-all` to a single device (by SoapySDR args
    /// string). Skips enumeration and targets only the matching device.
    #[arg(long = "read-device", value_name = "ARGS")]
    read_device: Option<String>,

    /// Override the sample rate used by `--read-all` (Hz).
    #[arg(long = "read-rate-hz", value_name = "HZ")]
    read_rate_hz: Option<f64>,

    /// Apply a `set_bandwidth` call with this value during `--read-all`
    /// (Hz). Omit to skip `set_bandwidth` entirely (driver default).
    #[arg(long = "read-bandwidth-hz", value_name = "HZ")]
    read_bandwidth_hz: Option<f64>,

    /// Override the center frequency used by `--read-all` (Hz).
    #[arg(long = "read-freq-hz", value_name = "HZ")]
    read_freq_hz: Option<f64>,

    /// Override the gain used by `--read-all` (dB).
    #[arg(long = "read-gain-db", value_name = "DB")]
    read_gain_db: Option<f64>,

    /// Hard timeout for `--list-devices` and `--probe-device`. Defaults
    /// to 10s — enough for slow drivers (SDRplay first probe is ~2.3s)
    /// while still bailing long before a wedged SoapySDR daemon would
    /// hang the CLI.
    #[arg(long = "probe-timeout-secs", default_value_t = 10)]
    probe_timeout_secs: u64,
}

fn load_flowgraph(path: &std::path::Path) -> Result<FlowgraphDoc> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading flowgraph {}", path.display()))?;
    FlowgraphDoc::from_json(&bytes).with_context(|| format!("parsing flowgraph {}", path.display()))
}

fn build_source_config(args: &Args) -> Result<SourceConfig> {
    if let Some(path) = args.source.as_deref() {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading source config {}", path.display()))?;
        let cfg: SourceConfig = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing source config {}", path.display()))?;
        return Ok(cfg);
    }
    let mut params = Map::new();
    if let Some(a) = &args.source_args {
        params.insert("args".into(), Value::String(a.clone()));
    }
    if let Some(a) = &args.antenna {
        params.insert("antenna".into(), Value::String(a.clone()));
    }
    if let Some(v) = args.gain_db {
        params.insert("gain_db".into(), json!(v));
    }
    if let Some(v) = args.agc {
        params.insert("agc".into(), Value::Bool(v));
    }
    if let Some(v) = args.source_bandwidth_hz {
        params.insert("bandwidth_hz".into(), json!(v));
    }
    if let Some(p) = &args.source_path {
        params.insert("path".into(), Value::String(p.display().to_string()));
        params.insert("loop_playback".into(), Value::Bool(args.loop_playback));
    }
    Ok(SourceConfig {
        type_name: args.source_type.clone(),
        params: Value::Object(params),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    let log_broadcast = log_stream::LogBroadcast::new();
    tracing_subscriber::registry()
        // Default filter mutes the 1 Hz flow telemetry — `flowdiag`
        // (rate-change lines), `flowdiag::node` (server snapshot +
        // flowcpu) and `flowdiag::browser` (browser snapshot). They're
        // ~4 lines/sec of self-similar JSON that drown real events in
        // log files and the UI stream. Operators opt back in with
        // `RUST_LOG=flowdiag=info` (or per-side) when diagnosing flow.
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,flowdiag=warn,flowdiag::node=warn,flowdiag::browser=warn")
        }))
        // Show the tracing target on stdout (e.g. `decoder::packet`,
        // `decoder::flex`, `flowdiag`) so log files and run.sh tails
        // are self-describing — same view the UI broadcast layer
        // already gives. The broadcast layer formats targets too, so
        // both surfaces stay in sync.
        .with(fmt::layer().with_target(true))
        .with(log_broadcast.layer())
        .init();

    // Hook libSoapySDR's logger into `tracing` so vendored-driver
    // warnings (`Not updating IFGR gain because AGC is enabled`,
    // `sdrplay_api_OutOfRange`, …) flow into `/ws/logs` + the history
    // ring instead of bypassing the API on stderr. Has to be after
    // tracing init so the callback's first events have a subscriber
    // to attach to.
    soapy_log::install();

    let args = Args::parse();

    let probe_timeout = Duration::from_secs(args.probe_timeout_secs);

    if args.list_devices {
        return run_list_devices(probe_timeout);
    }

    if let Some(probe_args) = args.probe_device.as_deref() {
        return run_probe_device(probe_args, probe_timeout);
    }

    if args.probe_all {
        return run_probe_all(probe_timeout);
    }

    if args.read_all {
        let read_for = Duration::from_secs_f64(args.read_secs.max(0.05));
        let overrides = device::ReadOverrides {
            sample_rate_hz: args.read_rate_hz,
            bandwidth_hz: args.read_bandwidth_hz,
            center_freq_hz: args.read_freq_hz,
            gain_db: args.read_gain_db,
        };
        return run_read_all(
            probe_timeout,
            read_for,
            args.read_device.as_deref(),
            overrides,
        );
    }

    let flowgraph_path = args
        .flowgraph
        .as_deref()
        .context("--flowgraph PATH is required to start the server (see --list-devices / --probe-device for diagnostics)")?;
    let preset = load_flowgraph(flowgraph_path)?;
    let source = build_source_config(&args)?;
    let tick_period = Duration::from_micros(args.tick_period_us);

    tracing::info!(
        flowgraph = %flowgraph_path.display(),
        source = %source.type_name,
        tick_period_us = args.tick_period_us,
        "ferrited starting"
    );

    let mut state = app_state::AppState::new(preset, source, tick_period).with_logs(log_broadcast);
    if let Some(dir) = args
        .presets_dir
        .clone()
        .or_else(|| flowgraph_path.parent().map(std::path::Path::to_path_buf))
    {
        tracing::info!(path = %dir.display(), "presets browser enabled");
        if args.validate_presets {
            let findings = ferrite_runtime::validate_corpus(&dir, None);
            if findings.is_empty() {
                tracing::info!(target: "validation", path = %dir.display(), "preset corpus clean");
            } else {
                for f in &findings {
                    tracing::warn!(target: "validation", kind = f.kind, slug = %f.slug, "{}", f.message);
                }
                tracing::warn!(
                    target: "validation",
                    count = findings.len(),
                    "preset corpus has cross-reference problems (see --validate-presets findings above)"
                );
            }
        }
        state = state.with_presets_dir(dir);
    }

    // Captures browser: the curated `samples/` tree of replayable
    // IQ/audio fixtures. `FERRITE_CAPTURES_DIR` overrides; default is
    // `./samples` (the repo layout in dev). Skipped if absent so a
    // deployed binary without samples just returns an empty list.
    {
        let captures = std::env::var_os("FERRITE_CAPTURES_DIR")
            .map_or_else(|| PathBuf::from("./samples"), PathBuf::from);
        if captures.is_dir() {
            tracing::info!(path = %captures.display(), "captures browser enabled");
            state = state.with_captures_dir(captures);
        } else {
            tracing::debug!(path = %captures.display(), "no captures dir — /api/captures empty");
        }
    }

    // Warm the device-capability cache before the listener binds. The
    // first `/api/devices` request hits the cache; SoapySource::new
    // doesn't race against an unrelated probe of the same driver.
    let warm = state.device_cache().warm_all().await;
    tracing::info!(
        attempted = warm.attempted,
        succeeded = warm.succeeded,
        failed = warm.failed,
        "device cache warm-up complete"
    );

    if args.auto_start {
        state.start().await.context("auto-start pipeline")?;
        tracing::info!("pipeline auto-started");
    }

    if let Some(secs) = args.run_for_secs {
        return run_headless_for(state, secs, args.auto_start).await;
    }

    let static_root: PathBuf = std::env::var_os("FERRITE_STATIC_ROOT")
        .map_or_else(|| PathBuf::from("./web-dist"), PathBuf::from);

    let mut app = Router::new()
        .route("/api/hello", get(routes::hello))
        .route("/api/devices", get(routes::list_devices))
        .route("/api/devices/reload", post(routes::reload_devices))
        .route(
            "/api/flowgraph",
            get(routes::get_flowgraph).patch(routes::patch_flowgraph),
        )
        .route(
            "/api/source",
            get(routes::get_source).patch(routes::patch_source),
        )
        .route(
            "/api/source/capabilities",
            get(routes::get_source_capabilities),
        )
        .route("/api/state", get(routes::get_state))
        .route("/api/state/:kind/reset", post(routes::reset_state_kind))
        .route("/api/tune", post(routes::post_tune))
        .route("/api/pipeline", get(routes::pipeline_status))
        .route("/api/pipeline/start", post(routes::pipeline_start))
        .route("/api/pipeline/stop", post(routes::pipeline_stop))
        .route("/api/pipeline/blocks", get(routes::list_pipeline_blocks))
        .route(
            "/api/pipeline/blocks/:id/params",
            post(routes::patch_pipeline_block),
        )
        .route("/api/ui-sinks", get(routes::list_ui_sinks))
        .route("/api/blocks", get(routes::list_block_schemas))
        .route("/api/presets", get(routes::list_presets))
        .route("/api/presets/:name", get(routes::get_preset))
        .route("/api/band-plan/at", get(routes::band_at))
        .route("/api/captures", get(routes::list_captures))
        .route("/api/preset", post(routes::load_preset))
        .route(
            "/api/profile",
            get(routes::get_profile).patch(routes::patch_profile),
        )
        .route("/api/decoder/recent", get(routes::recent_decoder))
        .route("/api/debug/log", post(routes::browser_log))
        .route("/api/screenshot", post(routes::save_screenshot))
        .route("/ws/logs", get(routes::ws_logs))
        .route("/ws/preset", get(routes::ws_preset))
        .route("/ws/state", get(routes::ws_state))
        .route("/ws/chat", get(routes::ws_chat))
        .route("/api/ai/chat-inject", post(routes::post_chat_inject))
        .route("/api/ui-views/:pane", get(routes::get_ui_view))
        .route("/api/view", get(routes::get_view_state))
        .route("/api/ui-view/set", post(routes::set_view_state))
        .route("/ws/ui-views", get(routes::ws_ui_views))
        .with_state(state);

    if static_root.is_dir() {
        let index = static_root.join("index.html");
        let serve_dir = ServeDir::new(&static_root).fallback(ServeFile::new(index));
        app = app.fallback_service(serve_dir);
        tracing::info!(path = %static_root.display(), "serving static assets");
    } else {
        tracing::warn!(
            path = %static_root.display(),
            "static root not found; only /api and /ws will respond"
        );
    }

    let app = app
        .layer(axum::middleware::from_fn(ai_activity_layer))
        .layer(header_layer("cross-origin-opener-policy", "same-origin"))
        .layer(header_layer("cross-origin-embedder-policy", "require-corp"))
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = args.bind.parse().context("parse --bind")?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr().context("listener local_addr")?;
    tracing::info!(addr = %local, "ferrited listening");
    // Stable machine-parseable line for test harnesses that spawn
    // ferrited with `--bind 127.0.0.1:0` and need to discover the
    // ephemeral port without parsing tracing output.
    println!("ferrited listening addr={local}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Surface CLI / AI activity onto the live log stream. ferrite-ctl
/// (and an Agent SDK driver) annotate every API call with
/// `X-Ferrite-Command` (the subcommand kebab-id, e.g. `capture-fft`)
/// and optional `X-Ferrite-Note` (free-form why-text). When either is
/// present and the request is mutating, we emit a tracing event under
/// target `ai::activity` so the existing LogBroadcast pumps it onto
/// `/ws/logs` for the UI's command-transcript panel.
///
/// We gate on method != GET so polling commands (`status`, `tail`,
/// `device list`) don't drown the panel — only writes show up.
async fn ai_activity_layer(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let note = request
        .headers()
        .get("x-ferrite-note")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let command = request
        .headers()
        .get("x-ferrite-command")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let should_log = (note.is_some() || command.is_some()) && method != axum::http::Method::GET;

    // Run the handler first, log after — so the `ai::activity` event
    // reflects a *completed* mutation. The UI's refresh-on-activity
    // path (`pipeline.refreshFromServer()` in the chat store) reads
    // server state immediately on every activity log line; if the log
    // fires before the handler commits, the UI reads pre-mutation
    // state and the Nixie / source label show stale values until the
    // next activity event eventually catches up. Order matters.
    let response = next.run(request).await;

    if should_log {
        // Pick the cleanest summary the headers gave us. The structured
        // `command` / `note` fields ride along so a UI can render them
        // separately if it wants.
        let summary = match (command.as_deref(), note.as_deref()) {
            (Some(c), Some(n)) if !c.is_empty() && !n.is_empty() => format!("{c}: {n}"),
            (Some(c), _) if !c.is_empty() => c.to_string(),
            (_, Some(n)) if !n.is_empty() => n.to_string(),
            _ => format!("{method} {path}"),
        };
        tracing::info!(
            target: "ai::activity",
            method = %method,
            path = %path,
            status = %response.status().as_u16(),
            command = command.as_deref().unwrap_or(""),
            note = note.as_deref().unwrap_or(""),
            "{summary}",
        );
    }

    response
}

/// Headless recording mode: start the pipeline, sleep `secs` (or until
/// Ctrl-C), then stop. No HTTP/WS server is bound, so this is the
/// right shape for capture-to-disk presets driven by the CLI.
async fn run_headless_for(
    state: app_state::AppState,
    secs: f64,
    already_started: bool,
) -> Result<()> {
    if !already_started {
        state.start().await.context("start pipeline")?;
        tracing::info!("pipeline started for headless run");
    }
    let duration = if secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    };
    tracing::info!(
        secs = secs,
        "running headless — pipeline will stop on timer or Ctrl-C"
    );
    // Stable line for test harnesses that spawn ferrited and want to
    // know the pipeline is live without parsing tracing output.
    println!("ferrited headless run_for_secs={secs}");

    tokio::select! {
        () = tokio::time::sleep(duration) => {
            tracing::info!("headless timer elapsed");
        }
        res = tokio::signal::ctrl_c() => {
            if let Err(e) = res {
                tracing::warn!(?e, "ctrl-c handler failed");
            } else {
                tracing::info!("received Ctrl-C");
            }
        }
    }

    let was_running = state.stop().await;
    tracing::info!(was_running, "headless run complete");
    Ok(())
}

fn run_list_devices(timeout: Duration) -> Result<()> {
    let devices = device::list_devices_with_timeout(timeout)?;
    device::print_devices(&devices);
    Ok(())
}

fn run_probe_device(args: &str, timeout: Duration) -> Result<()> {
    let caps = device::probe_with_timeout(args, timeout)?;
    device::print_capabilities(&caps);
    Ok(())
}

/// Enumerate every device and print its capability schema. A failed
/// probe for one device does not abort the run — the failure is printed
/// and the next device is tried. Exit status is non-zero only when at
/// least one device failed to probe.
fn run_probe_all(timeout: Duration) -> Result<()> {
    let devices = device::list_devices_with_timeout(timeout)?;
    if devices.is_empty() {
        println!("No SoapySDR devices found.");
        return Ok(());
    }
    println!("Found {} SoapySDR device(s):", devices.len());
    let mut failures = 0_usize;
    for (i, info) in devices.iter().enumerate() {
        println!();
        println!("=== [{i}] {} ({}) ===", info.label, info.args_string());
        match device::probe_with_timeout(&info.args_string(), timeout) {
            Ok(caps) => device::print_capabilities(&caps),
            Err(err) => {
                eprintln!("probe failed: {err:#}");
                failures += 1;
            }
        }
    }
    if failures > 0 {
        anyhow::bail!("{failures} of {} device(s) failed to probe", devices.len());
    }
    Ok(())
}

/// Enumerate every device, probe it, then open a short Rx stream and
/// report whether samples actually arrived. Probes use `probe_timeout`;
/// the sampling window itself is `read_for` per device (plus internal
/// slack for open/configure/activate).
fn run_read_all(
    probe_timeout: Duration,
    read_for: Duration,
    restrict_args: Option<&str>,
    overrides: device::ReadOverrides,
) -> Result<()> {
    let devices = if let Some(args) = restrict_args {
        // Synthetic one-entry list so the rest of the loop is uniform.
        // Probe fills in label/serial; here we only need args to target it.
        vec![device::DeviceInfo {
            driver: String::new(),
            label: args.to_string(),
            serial: None,
            args: args
                .split(',')
                .filter_map(|p| p.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect(),
        }]
    } else {
        device::list_devices_with_timeout(probe_timeout)?
    };
    if devices.is_empty() {
        println!("No SoapySDR devices found.");
        return Ok(());
    }
    println!(
        "Reading {:.2}s per device from {} SoapySDR device(s):",
        read_for.as_secs_f64(),
        devices.len()
    );
    let mut failures = 0_usize;
    for (i, info) in devices.iter().enumerate() {
        println!();
        println!("=== [{i}] {} ({}) ===", info.label, info.args_string());
        let args_s = info.args_string();
        let caps = match device::probe_with_timeout(&args_s, probe_timeout) {
            Ok(c) => c,
            Err(err) => {
                eprintln!("probe failed: {err:#}");
                failures += 1;
                continue;
            }
        };
        match device::read_samples_with_timeout(&args_s, &caps, read_for, overrides.clone()) {
            Ok(report) => {
                device::print_sample_report(&report);
                if report.samples_read == 0 {
                    failures += 1;
                    eprintln!("  (no samples read — treated as failure)");
                }
            }
            Err(err) => {
                eprintln!("read failed: {err:#}");
                failures += 1;
            }
        }
    }
    if failures > 0 {
        anyhow::bail!("{failures} of {} device(s) failed to read", devices.len());
    }
    Ok(())
}

fn header_layer(name: &'static str, value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::overriding(
        HeaderName::from_static(name),
        HeaderValue::from_static(value),
    )
}
