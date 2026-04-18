//! `ferrited` — Ferrite SDR daemon.
//!
//! Phase A skeleton. Binds to `0.0.0.0:8088`, serves `GET /api/hello`,
//! accepts a WebSocket upgrade on `/ws`, and serves the `SvelteKit`
//! static bundle from `FERRITE_STATIC_ROOT` (defaults to `./web-dist`).
//! Every response carries COOP/COEP so the browser grants
//! `SharedArrayBuffer` access.

use std::{net::SocketAddr, path::PathBuf};

use anyhow::Result;
use axum::{
    routing::{get, post},
    Router,
};
use http::{HeaderName, HeaderValue};
use tower_http::{
    services::{ServeDir, ServeFile},
    set_header::SetResponseHeaderLayer,
    trace::TraceLayer,
};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod routes;
mod session;
mod ws_frame;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer().with_target(false))
        .init();

    let static_root: PathBuf = std::env::var_os("FERRITE_STATIC_ROOT")
        .map_or_else(|| PathBuf::from("./web-dist"), PathBuf::from);

    let state = session::AppState::new();

    let mut app = Router::new()
        .route("/api/hello", get(routes::hello))
        .route("/api/device/open", post(routes::open_session))
        .route("/api/device/:id/close", post(routes::close_session))
        .route("/ws", get(routes::ws_upgrade))
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

    let addr: SocketAddr = "0.0.0.0:8088".parse()?;
    tracing::info!(%addr, "ferrited listening");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn header_layer(name: &'static str, value: &'static str) -> SetResponseHeaderLayer<HeaderValue> {
    SetResponseHeaderLayer::overriding(
        HeaderName::from_static(name),
        HeaderValue::from_static(value),
    )
}
