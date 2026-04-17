//! `ferrited` — Ferrite SDR daemon.
//!
//! Phase A skeleton: binds to 0.0.0.0:8088 and serves `GET /api/hello`.
//! Device I/O, FFT, channelizer, WS streams all land in later commits
//! per `docs/10-commits.md`.

use std::net::SocketAddr;

use anyhow::Result;
use axum::{routing::get, Json, Router};
use serde::Serialize;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    let app = Router::new()
        .route("/api/hello", get(hello))
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = "0.0.0.0:8088".parse()?;
    tracing::info!(%addr, "ferrited listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Serialize)]
struct Hello {
    app: &'static str,
    version: &'static str,
    status: &'static str,
}

async fn hello() -> Json<Hello> {
    Json(Hello {
        app: "ferrited",
        version: env!("CARGO_PKG_VERSION"),
        status: "ok",
    })
}
