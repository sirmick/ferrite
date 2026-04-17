//! `ferrited` — Ferrite SDR daemon.
//!
//! Phase A skeleton. Binds to `0.0.0.0:8088`, serves `GET /api/hello`,
//! and accepts a WebSocket upgrade on `/ws` that echoes text frames.
//! Device I/O, FFT, channelizer, and the real binary WS protocol all
//! land in later commits per `docs/10-commits.md`.

use std::net::SocketAddr;

use anyhow::Result;
use axum::{routing::get, Router};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod routes;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    let app = Router::new()
        .route("/api/hello", get(routes::hello))
        .route("/ws", get(routes::ws_upgrade))
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = "0.0.0.0:8088".parse()?;
    tracing::info!(%addr, "ferrited listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
