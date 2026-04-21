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
mod block_schema;
mod bridge_sink;
mod device;
mod log_stream;
mod preset_pipeline;
mod routes;
#[cfg(feature = "soapysdr")]
mod soapy_source;

/// Ferrite SDR daemon.
#[derive(Parser, Debug)]
#[command(name = "ferrited", version)]
struct Args {
    /// Address to bind the HTTP/WS server to.
    #[arg(long, default_value = "0.0.0.0:8088")]
    bind: String,

    /// Flowgraph preset JSON. Required. The `src` block must be a
    /// `Source` placeholder (see `runtime::compose`); the concrete
    /// source is supplied separately via `--source` or inline flags.
    #[arg(long = "flowgraph", value_name = "PATH")]
    flowgraph: PathBuf,

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

    /// Runtime tick period, in microseconds. Defaults to 400µs
    /// (2.5kHz) which is fine for any source at 2MHz or below given
    /// `DEFAULT_FRAMES_HINT` of 1024 samples per tick.
    #[arg(long = "tick-period-us", default_value_t = 400)]
    tick_period_us: u64,

    /// Enumerate `SoapySDR` devices and exit. Requires the `soapysdr`
    /// feature at build time.
    #[arg(long = "list-devices", default_value_t = false)]
    list_devices: bool,

    /// Probe one device's capabilities and exit.
    #[arg(long = "probe-device", value_name = "ARGS")]
    probe_device: Option<String>,
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
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .with(log_broadcast.layer())
        .init();

    let args = Args::parse();

    if args.list_devices {
        return run_list_devices();
    }

    if let Some(probe_args) = args.probe_device.as_deref() {
        return run_probe_device(probe_args);
    }

    let preset = load_flowgraph(&args.flowgraph)?;
    let source = build_source_config(&args)?;
    let tick_period = Duration::from_micros(args.tick_period_us);

    tracing::info!(
        flowgraph = %args.flowgraph.display(),
        source = %source.type_name,
        tick_period_us = args.tick_period_us,
        "ferrited starting"
    );

    let mut state = app_state::AppState::new(preset, source, tick_period).with_logs(log_broadcast);
    if let Some(dir) = args
        .presets_dir
        .clone()
        .or_else(|| args.flowgraph.parent().map(std::path::Path::to_path_buf))
    {
        tracing::info!(path = %dir.display(), "presets browser enabled");
        state = state.with_presets_dir(dir);
    }

    if args.auto_start {
        state.start().await.context("auto-start pipeline")?;
        tracing::info!("pipeline auto-started");
    }

    let static_root: PathBuf = std::env::var_os("FERRITE_STATIC_ROOT")
        .map_or_else(|| PathBuf::from("./web-dist"), PathBuf::from);

    let mut app = Router::new()
        .route("/api/hello", get(routes::hello))
        .route("/api/devices", get(routes::list_devices))
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
        .route("/api/preset", post(routes::load_preset))
        .route("/ws/logs", get(routes::ws_logs))
        .route("/ws/preset", get(routes::ws_preset))
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

#[cfg(feature = "soapysdr")]
fn run_list_devices() -> Result<()> {
    let devices = device::list_devices()?;
    device::print_devices(&devices);
    Ok(())
}

#[cfg(not(feature = "soapysdr"))]
fn run_list_devices() -> Result<()> {
    anyhow::bail!(
        "ferrited was built without the `soapysdr` feature; rebuild with \
         `cargo run -p ferrited --features soapysdr -- --list-devices` \
         (see CONTRIBUTING.md for the SoapySDR dev setup)"
    )
}

#[cfg(feature = "soapysdr")]
fn run_probe_device(args: &str) -> Result<()> {
    let caps = device::probe(args)?;
    device::print_capabilities(&caps);
    Ok(())
}

#[cfg(not(feature = "soapysdr"))]
fn run_probe_device(_args: &str) -> Result<()> {
    anyhow::bail!(
        "ferrited was built without the `soapysdr` feature; rebuild with \
         `cargo run -p ferrited --features soapysdr -- --probe-device …` \
         (see CONTRIBUTING.md for the SoapySDR dev setup)"
    )
}

fn header_layer(name: &'static str, value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::overriding(
        HeaderName::from_static(name),
        HeaderValue::from_static(value),
    )
}
