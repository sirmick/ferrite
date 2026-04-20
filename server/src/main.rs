//! `ferrited` — Ferrite SDR daemon.
//!
//! Binds to `0.0.0.0:8088`, serves the REST API under `/api`, the
//! per-session and preset WebSocket streams under `/ws/*`, and the
//! `SvelteKit` static bundle from `FERRITE_STATIC_ROOT` (defaults to
//! `./web-dist`). Every response carries COOP/COEP so the browser
//! grants `SharedArrayBuffer` access.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use anyhow::{Context, Result};
use axum::{
    routing::{get, patch, post},
    Router,
};
use clap::Parser;
use ferrite_runtime::FlowgraphDoc;
use http::{HeaderName, HeaderValue};
use tokio::sync::broadcast;
use tower_http::{
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod block_schema;
mod bridge_sink;
mod device;
mod log_stream;
mod preset_pipeline;
mod routes;
mod session;
#[cfg(feature = "soapysdr")]
mod soapy_source;
mod ws_frame;

use session::{CliConfig, SourceKind};

/// Ferrite SDR daemon.
#[derive(Parser, Debug)]
#[command(name = "ferrited", version)]
struct Args {
    /// Address to bind the HTTP/WS server to.
    #[arg(long, default_value = "0.0.0.0:8088")]
    bind: String,

    /// Sample source URI. Supported:
    ///   `sine` (built-in synthetic tone, default),
    ///   `file:///absolute/path.wav` or `file:///path.cf32` (replay capture).
    #[arg(long, default_value = "sine", verbatim_doc_comment)]
    source: String,

    /// Sample rate override (Hz). Required for raw `cf32` replay; ignored
    /// for WAV (the header wins) and sine (the session request wins).
    #[arg(long)]
    rate: Option<f64>,

    /// Center frequency override (Hz). IQ files do not carry this; set it
    /// here so the display axes and any downstream tuner are correct.
    #[arg(long)]
    freq: Option<f64>,

    /// Loop the file on EOF (replay mode only).
    #[arg(long = "loop", default_value_t = false)]
    loop_playback: bool,

    /// Enumerate `SoapySDR` devices and exit. Requires the `soapysdr`
    /// feature at build time; prints a guidance message otherwise.
    #[arg(long = "list-devices", default_value_t = false)]
    list_devices: bool,

    /// Probe one device's capabilities and exit. Takes a Soapy args
    /// string (e.g. `driver=rtlsdr,serial=0000001`). Requires the
    /// `soapysdr` feature at build time.
    #[arg(long = "probe-device", value_name = "ARGS")]
    probe_device: Option<String>,

    /// Run in preset mode: load the flowgraph JSON at `<path>`, spawn
    /// the Rust runtime for its node half, and expose the resulting IQ
    /// stream on `/ws/preset`. Mutually exclusive with the legacy
    /// `--source` path (`--source` is ignored in this mode).
    #[arg(long = "flowgraph", value_name = "PATH")]
    flowgraph: Option<PathBuf>,

    /// Runtime tick period, in microseconds, for preset mode. Defaults
    /// to 400µs (2.5kHz) which is fine for any source running at 2MHz
    /// or below given `DEFAULT_FRAMES_HINT` of 1024 samples per tick.
    #[arg(long = "tick-period-us", default_value_t = 400)]
    tick_period_us: u64,
}

fn parse_source(arg: &str, loop_playback: bool) -> Result<SourceKind> {
    if arg == "sine" {
        return Ok(SourceKind::Sine);
    }
    let path = arg
        .strip_prefix("file://")
        .with_context(|| format!("unknown --source scheme: {arg}"))?;
    Ok(SourceKind::File {
        path: PathBuf::from(path),
        loop_playback,
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

    let source = parse_source(&args.source, args.loop_playback)?;
    let cli = CliConfig {
        source,
        rate_override_hz: args.rate,
        center_override_hz: args.freq,
    };
    tracing::info!(?cli, "ferrited starting");

    let static_root: PathBuf = std::env::var_os("FERRITE_STATIC_ROOT")
        .map_or_else(|| PathBuf::from("./web-dist"), PathBuf::from);

    let state = if let Some(flowgraph_path) = args.flowgraph.as_deref() {
        let mount = spawn_preset_mount(flowgraph_path, args.tick_period_us)?;
        tracing::info!(
            path = %flowgraph_path.display(),
            tick_period_us = args.tick_period_us,
            "preset pipeline mounted"
        );
        session::AppState::new_with_preset(cli, mount).with_logs(log_broadcast)
    } else {
        session::AppState::new(cli).with_logs(log_broadcast)
    };

    let mut app = Router::new()
        .route("/api/hello", get(routes::hello))
        .route("/api/devices", get(routes::list_devices))
        .route("/api/device/open", post(routes::open_session))
        .route("/api/device/:id/close", post(routes::close_session))
        .route("/api/device/:id/state", get(routes::session_state))
        .route("/api/device/:id/settings", patch(routes::patch_settings))
        .route("/api/device/:id/vfo", post(routes::add_vfo))
        .route(
            "/api/device/:id/vfo/:vfo_id",
            patch(routes::patch_vfo).delete(routes::delete_vfo),
        )
        .route(
            "/api/flowgraph",
            get(routes::get_flowgraph).patch(routes::patch_flowgraph),
        )
        .route("/api/blocks", get(routes::list_block_schemas))
        .route("/ws/logs", get(routes::ws_logs))
        .route("/ws/preset", get(routes::ws_preset))
        .route("/ws/:id", get(routes::ws_session))
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
    tracing::info!(%addr, "ferrited listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
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

/// Read `path`, parse as a `FlowgraphDoc`, spawn the preset pipeline,
/// and hand back the `PresetMount` the `AppState` will own.
fn spawn_preset_mount(
    path: &std::path::Path,
    tick_period_us: u64,
) -> Result<preset_pipeline::PresetMount> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading flowgraph {}", path.display()))?;
    let doc: FlowgraphDoc = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing flowgraph {}", path.display()))?;
    let (frames, _) = broadcast::channel(32);
    preset_pipeline::spawn_preset(&doc, frames, Duration::from_micros(tick_period_us))
        .context("spawn preset pipeline")
}
