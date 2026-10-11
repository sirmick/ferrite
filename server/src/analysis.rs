//! The analysis toolkit over HTTP: snapshots of live IQ and the
//! measurements on them ([`ferrite_analysis`]).
//!
//! - `POST /api/analysis/snapshot {secs}` records a few seconds of IQ
//!   through a live tee — the source's own `record_path` on hardware
//!   (the full span), the channelizer's on a software source — without
//!   retuning. The last few are kept by id, so several measurements can
//!   look at the *same* moment of radio.
//! - `POST /api/analysis/{spectrum,spectrogram,envelope}` run on a
//!   snapshot (`snapshot: id`) or take a fresh one (`secs`).
//!   `POST /api/spectrum` is the spectrum route under its first name.
//!
//! Every measurement also lands as an `analysis` store record (tool,
//! params, result, who ran it, a text rendering), which the AI panel
//! draws inline — anything the AI (or a person) measures is visible.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use base64::Engine;
use ferrite_analysis as fa;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::error::AppError;

/// Widest range one call may analyse.
const MAX_RANGE_HZ: f64 = 400_000.0;
/// DC spur guard on a raw source tap (live: the RSPdx's LO spur
/// flickered as a dozen "keyed" peaks within ±1 kHz).
const DC_GUARD_HZ: f64 = 1_500.0;
/// Snapshots kept (each is rate × secs × 8 bytes on disk).
const SNAPSHOT_KEEP: usize = 4;

#[derive(Debug, Clone, Serialize)]
pub struct SnapshotInfo {
    pub id: String,
    pub center_hz: f64,
    pub rate_hz: f64,
    /// Unix ms when recording started — for UTC-aligned timelines.
    pub start_ms: u64,
    pub secs: f64,
    /// Which tee: `src` (full span) or a channelizer id (channel only).
    pub tapped: String,
}

struct Snapshot {
    info: SnapshotInfo,
    path: PathBuf,
}

fn ring() -> &'static Mutex<VecDeque<Snapshot>> {
    static RING: std::sync::OnceLock<Mutex<VecDeque<Snapshot>>> = std::sync::OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// One recording at a time: the tee has a single `record_path`.
fn capture_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// `snap-…` / `an-…` ids. The counter restarts with the daemon, so the
/// ids carry a tag of the daemon's start time too — an id held from
/// before a restart (by an agent mid-conversation) never names a
/// different recording after it.
fn mint_id(prefix: &str) -> String {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let boot = BOOT.get_or_init(|| {
        let mut n = u64::try_from(crate::capture::now_ms() / 1000).unwrap_or(0) % 36u64.pow(4);
        let mut tag = [b'0'; 4];
        for c in tag.iter_mut().rev() {
            *c = b"0123456789abcdefghijklmnopqrstuvwxyz"[(n % 36) as usize];
            n /= 36;
        }
        String::from_utf8_lossy(&tag).into_owned()
    });
    format!(
        "{prefix}-{boot}-{}",
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn remove_files(p: &Path) {
    let _ = std::fs::remove_file(p);
    let _ = std::fs::remove_file(crate::capture::sidecar_path_for(p));
}

/// Who asked: an agent over MCP, a person on the CLI, or the UI.
fn caller(headers: &HeaderMap) -> String {
    headers
        .get("x-ferrite-via")
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| "ui".to_string(), str::to_string)
}

#[derive(Debug, Clone, Deserialize)]
pub struct SnapshotReq {
    #[serde(default)]
    pub secs: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpectrumReq {
    #[serde(default)]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub secs: Option<f64>,
    pub lo_hz: f64,
    pub hi_hz: f64,
    #[serde(default)]
    pub res_hz: Option<f64>,
    #[serde(default)]
    pub max_peaks: Option<usize>,
    #[serde(default)]
    pub profile_points: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SpectrogramReq {
    #[serde(default)]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub secs: Option<f64>,
    pub lo_hz: f64,
    pub hi_hz: f64,
    #[serde(default)]
    pub res_hz: Option<f64>,
    #[serde(default)]
    pub step_ms: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnvelopeReq {
    #[serde(default)]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub secs: Option<f64>,
    pub freq_hz: f64,
    /// Bandwidth around `freq_hz` (default 200 Hz).
    #[serde(default)]
    pub bw_hz: Option<f64>,
    #[serde(default)]
    pub window_ms: Option<f64>,
}

fn check_range(lo: f64, hi: f64) -> Result<(), AppError> {
    if !(lo.is_finite() && hi.is_finite() && hi > lo) {
        return Err(AppError::bad_request(
            "ANALYSIS_REJECTED",
            "need lo_hz < hi_hz",
        ));
    }
    if hi - lo > MAX_RANGE_HZ {
        return Err(AppError::bad_request(
            "ANALYSIS_REJECTED",
            format!(
                "range {:.0} Hz is wider than {MAX_RANGE_HZ:.0} Hz — split it",
                hi - lo
            ),
        ));
    }
    Ok(())
}

fn bad(e: &anyhow::Error) -> AppError {
    AppError::bad_request("ANALYSIS_REJECTED", format!("{e:#}"))
}

fn open(path: &Path) -> anyhow::Result<std::io::BufReader<std::fs::File>> {
    Ok(std::io::BufReader::new(
        std::fs::File::open(path).context("open snapshot")?,
    ))
}

impl AppState {
    /// Record `secs` of IQ from the widest live tee and keep it by id.
    pub async fn take_snapshot(&self, secs: Option<f64>) -> Result<SnapshotInfo, AppError> {
        let secs = secs.unwrap_or(3.0).clamp(0.5, 15.0);
        if self.status().await != crate::app_state::PipelineStatus::Running {
            return Err(AppError::conflict(
                "PIPELINE_STOPPED",
                "analysis taps the running pipeline — start it first",
            ));
        }
        let block = if self.get_source().await.type_name == "SoapySource" {
            "src".to_string()
        } else {
            self.get_flowgraph()
                .await
                .blocks
                .iter()
                .find(|(_, b)| b.type_name == "Channelizer")
                .map(|(id, _)| id.clone())
                .ok_or_else(|| {
                    AppError::conflict(
                        "NO_TAP",
                        "no hardware source and no channelizer to tap in this preset",
                    )
                })?
        };
        let _gate = capture_gate().lock().await;
        let id = mint_id("snap");
        let path: PathBuf =
            std::env::temp_dir().join(format!("ferrite-{id}-{}.cf32", std::process::id()));
        let start_ms = u64::try_from(crate::capture::now_ms()).unwrap_or(0);
        self.apply_block_params(
            &block,
            json!({ "record_path": path.display().to_string(), "record_max_seconds": secs }),
        )
        .await
        .map_err(|e| AppError::radio_or_bad_request("SNAPSHOT_FAILED", &e))?;
        tokio::time::sleep(Duration::from_secs_f64(secs + 0.4)).await;
        let _ = self
            .apply_block_params(&block, json!({ "record_path": "" }))
            .await;
        let Some(side) = crate::capture::read_sidecar(&path).await else {
            remove_files(&path);
            return Err(AppError::internal(
                "SNAPSHOT_FAILED",
                format!("the `{block}` tee wrote no sidecar — nothing was recorded"),
            ));
        };
        let num = |k: &str| side.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        let info = SnapshotInfo {
            id,
            center_hz: num("center_freq_hz"),
            rate_hz: num("sample_rate_hz"),
            start_ms,
            secs,
            tapped: block,
        };
        let mut r = ring()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        r.push_back(Snapshot {
            info: info.clone(),
            path,
        });
        while r.len() > SNAPSHOT_KEEP {
            if let Some(old) = r.pop_front() {
                remove_files(&old.path);
            }
        }
        Ok(info)
    }

    /// The snapshot to measure: `id` if given (must still be kept), else
    /// a fresh one.
    async fn snapshot_for(
        &self,
        id: Option<&str>,
        secs: Option<f64>,
    ) -> Result<(SnapshotInfo, PathBuf), AppError> {
        if let Some(id) = id {
            let r = ring()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            return r
                .iter()
                .find(|s| s.info.id == id)
                .map(|s| (s.info.clone(), s.path.clone()))
                .ok_or_else(|| {
                    AppError::not_found(
                        "SNAPSHOT_GONE",
                        format!("snapshot {id:?} isn't kept any more (last {SNAPSHOT_KEEP}) — take a new one"),
                    )
                });
        }
        let info = self.take_snapshot(secs).await?;
        let path = ring()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|s| s.info.id == info.id)
            .map(|s| s.path.clone())
            .ok_or_else(|| AppError::internal("SNAPSHOT_FAILED", "snapshot vanished"))?;
        Ok((info, path))
    }

    /// Fold an `analysis` record (what the AI panel draws); returns its id.
    fn publish_analysis(&self, tool: &str, by: &str, snap: &SnapshotInfo, body: Value) -> String {
        let id = mint_id("an");
        let mut rec = json!({
            "id": id,
            "tool": tool,
            "by": by,
            "snapshot": snap,
        });
        if let (Some(r), Some(b)) = (rec.as_object_mut(), body.as_object()) {
            r.extend(b.clone());
        }
        self.decoder_store().apply("analysis", rec);
        id
    }
}

fn meta(s: &SnapshotInfo) -> fa::IqMeta {
    fa::IqMeta {
        rate_hz: s.rate_hz,
        center_hz: s.center_hz,
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, AppError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppError::internal("ANALYSIS_FAILED", format!("analysis panicked: {e}")))?
        .map_err(|e| bad(&e))
}

/// `POST /api/analysis/snapshot`.
pub async fn post_snapshot(
    State(state): State<AppState>,
    Json(req): Json<SnapshotReq>,
) -> Result<Json<SnapshotInfo>, AppError> {
    state.take_snapshot(req.secs).await.map(Json)
}

/// `POST /api/analysis/spectrum` (and `/api/spectrum`).
pub async fn post_spectrum(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SpectrumReq>,
) -> Result<Json<Value>, AppError> {
    check_range(req.lo_hz, req.hi_hz)?;
    let (snap, path) = state
        .snapshot_for(req.snapshot.as_deref(), req.secs)
        .await?;
    let p = fa::SpectrumParams {
        rate: snap.rate_hz,
        center: snap.center_hz,
        lo: req.lo_hz,
        hi: req.hi_hz,
        res_hz: req.res_hz.unwrap_or(10.0),
        max_peaks: req.max_peaks.unwrap_or(25).min(200),
        profile_points: req.profile_points.unwrap_or(120).min(2_000),
        dc_guard_hz: if snap.tapped == "src" {
            DC_GUARD_HZ
        } else {
            0.0
        },
    };
    let r = blocking(move || fa::spectrum(|| open(&path), p)).await?;
    let text = fa::text::spectrum(&r);
    let body = json!({
        "params": { "lo_hz": req.lo_hz, "hi_hz": req.hi_hz, "res_hz": p.res_hz },
        "result": r,
        "dc_guard_hz": p.dc_guard_hz,
        "text": text,
    });
    let id = state.publish_analysis("spectrum", &caller(&headers), &snap, body.clone());
    // Flat, backward-compatible shape for `/api/spectrum` callers.
    Ok(Json(json!({
        "analysis_id": id,
        "snapshot": snap,
        "lo_hz": req.lo_hz,
        "hi_hz": req.hi_hz,
        "res_hz": r.res_hz,
        "frames": r.frames,
        "secs": snap.secs,
        "center_freq_hz": snap.center_hz,
        "sample_rate_hz": snap.rate_hz,
        "tapped": snap.tapped,
        "dc_guard_hz": p.dc_guard_hz,
        "floor_dbfs": r.floor_dbfs,
        "peaks": r.peaks,
        "profile": r.profile,
        "text": text,
    })))
}

/// `POST /api/analysis/spectrogram`.
pub async fn post_spectrogram(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SpectrogramReq>,
) -> Result<Json<Value>, AppError> {
    check_range(req.lo_hz, req.hi_hz)?;
    let (snap, path) = state
        .snapshot_for(req.snapshot.as_deref(), req.secs)
        .await?;
    let m = meta(&snap);
    let mut p = fa::SpectrogramParams::new(req.lo_hz, req.hi_hz);
    if let Some(r) = req.res_hz {
        p.res_hz = r;
    }
    if let Some(s) = req.step_ms {
        p.step_ms = s;
    }
    // Slice a little wider than asked so the FIR edges stay outside.
    let pad = (req.hi_hz - req.lo_hz) * 0.1;
    let (lo, hi) = (req.lo_hz - pad, req.hi_hz + pad);
    let sg = blocking(move || {
        let sl = fa::slice(open(&path)?, m, lo, hi)?;
        fa::spectrogram(&sl, p)
    })
    .await?;
    let text = fa::text::spectrogram(&sg, 80, 30);
    let cells = base64::engine::general_purpose::STANDARD.encode(&sg.cells);
    let body = json!({
        "params": { "lo_hz": req.lo_hz, "hi_hz": req.hi_hz, "res_hz": p.res_hz, "step_ms": p.step_ms },
        "result": sg,
        "cells_b64": cells,
        "text": text,
    });
    let id = state.publish_analysis("spectrogram", &caller(&headers), &snap, body.clone());
    let mut out = body;
    out["analysis_id"] = json!(id);
    out["snapshot"] = json!(snap);
    Ok(Json(out))
}

/// `POST /api/analysis/envelope`.
pub async fn post_envelope(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<EnvelopeReq>,
) -> Result<Json<Value>, AppError> {
    let bw = req.bw_hz.unwrap_or(200.0).clamp(20.0, 20_000.0);
    check_range(req.freq_hz - bw / 2.0, req.freq_hz + bw / 2.0)?;
    let (snap, path) = state
        .snapshot_for(req.snapshot.as_deref(), req.secs)
        .await?;
    let m = meta(&snap);
    let p = fa::EnvelopeParams {
        window_ms: req.window_ms.unwrap_or(10.0).clamp(1.0, 1_000.0),
    };
    let f = req.freq_hz;
    let e = blocking(move || {
        let sl = fa::slice(open(&path)?, m, f - bw / 2.0, f + bw / 2.0)?;
        fa::envelope(&sl, p)
    })
    .await?;
    let text = fa::text::envelope(&e, 100);
    let body = json!({
        "params": { "freq_hz": f, "bw_hz": bw, "window_ms": p.window_ms },
        "result": e,
        "text": text,
    });
    let id = state.publish_analysis("envelope", &caller(&headers), &snap, body.clone());
    let mut out = body;
    out["analysis_id"] = json!(id);
    out["snapshot"] = json!(snap);
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
pub struct SpotMapReq {
    /// `ft8` (default), `ft4` or `wspr`.
    #[serde(default)]
    pub kind: Option<String>,
    /// How far back to look, minutes (default 30).
    #[serde(default)]
    pub minutes: Option<f64>,
    /// The receive station's grid; default `FERRITE_RX_GRID`.
    #[serde(default)]
    pub rx_grid: Option<String>,
}

/// `POST /api/analysis/spot_map` — where the stations a digital-mode
/// decoder heard are: one spot per callsign (grid → position, distance
/// and bearing from the receive grid), drawn as a map in the AI panel.
pub async fn post_spot_map(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SpotMapReq>,
) -> Result<Json<Value>, AppError> {
    let kind = req.kind.unwrap_or_else(|| "ft8".into());
    if !matches!(kind.as_str(), "ft8" | "ft4" | "wspr") {
        return Err(AppError::bad_request(
            "BAD_KIND",
            format!("spot_map kind {kind:?}: expected ft8, ft4 or wspr"),
        ));
    }
    let minutes = req.minutes.unwrap_or(30.0).clamp(1.0, 24.0 * 60.0);
    let rx_grid = req
        .rx_grid
        .or_else(|| std::env::var("FERRITE_RX_GRID").ok())
        .filter(|g| fa::geo::grid_to_latlon(g).is_some());
    let rx = rx_grid.as_deref().and_then(fa::geo::grid_to_latlon);
    let now = u64::try_from(crate::capture::now_ms()).unwrap_or(0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let since = now.saturating_sub((minutes * 60_000.0) as u64);
    let heard: Vec<(String, String, Option<f64>, u64)> = state
        .decoder_store()
        .snapshot_kind(&kind)
        .map(|k| {
            k.recent
                .iter()
                .filter(|r| r.at_ms >= since)
                .filter_map(|r| {
                    let call = r.data.get("de")?.as_str()?.to_string();
                    let grid = r.data.get("grid")?.as_str()?.to_string();
                    let snr = r.data.get("snr").and_then(Value::as_f64);
                    Some((call, grid, snr, r.at_ms))
                })
                .collect()
        })
        .unwrap_or_default();
    let spots = fa::geo::place(&heard, rx);
    let mut text = format!(
        "{} {kind} stations with a grid in the last {minutes:.0} min{}\n",
        spots.len(),
        rx_grid
            .as_deref()
            .map(|g| format!(", from {}", g.to_ascii_uppercase()))
            .unwrap_or_default()
    );
    for s in spots.iter().take(40) {
        let _ = std::fmt::Write::write_fmt(
            &mut text,
            format_args!(
                "{:<10} {:<6} {:>7} {:>5} {:>5}\n",
                s.call,
                s.grid,
                s.km.map(|k| format!("{k:.0} km")).unwrap_or_default(),
                s.bearing.map(|b| format!("{b:.0}°")).unwrap_or_default(),
                s.snr.map(|v| format!("{v:+.0}")).unwrap_or_default(),
            ),
        );
    }
    let body = json!({
        "params": { "kind": kind, "minutes": minutes, "rx_grid": rx_grid },
        "result": {
            "rx": rx.map(|(lat, lon)| json!({ "grid": rx_grid, "lat": lat, "lon": lon })),
            "spots": spots,
        },
        "text": text,
    });
    let id = mint_id("an");
    let mut rec = json!({ "id": id, "tool": "spot_map", "by": caller(&headers) });
    if let (Some(r), Some(b)) = (rec.as_object_mut(), body.as_object()) {
        r.extend(b.clone());
    }
    state.decoder_store().apply("analysis", rec);
    let mut out = body;
    out["analysis_id"] = json!(id);
    Ok(Json(out))
}
