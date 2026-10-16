//! `POST /api/inspect` — characterise one signal: point the receiver at
//! it, record a few seconds of IQ, and measure what an operator reads off
//! a waterfall before naming a mode (ferrite_analysis::inspect): refined
//! centre, occupied width, level / SNR, spectral lines + FSK shift,
//! keying / duty / burst timing / periodicity, constant-envelope hint —
//! plus the spectrogram as a picture. The result is an `analysis` record
//! (tool `inspect`, drawn in the AI panel) and, when it's an emitter, its
//! headline lands on the emitter (`inspect`).
//!
//! Lease: like `tune` — while the spectrum analyzer owns the radio this is
//! 409 `RADIO_BUSY` unless `force` (which leaves the analyzer); `restore`
//! goes back to it afterwards, same sweep range.

use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use base64::Engine;
use ferrite_analysis as fa;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::analysis::{blocking, caller, open, DC_GUARD_HZ, MAX_RANGE_HZ};
use crate::app_state::{AppState, PipelineStatus};
use crate::emitters;
use crate::error::AppError;

const PANORAMA_PRESET: &str = "panorama";
/// Span when the width is unknown.
const DEFAULT_SPAN_HZ: f64 = 25_000.0;
/// Narrowest span.
const MIN_SPAN_HZ: f64 = 2_000.0;
/// Let a retuned stream settle before recording.
const SETTLE: Duration = Duration::from_millis(600);

#[derive(Debug, Deserialize)]
pub struct InspectReq {
    /// An emitter (`id` from `emitters`): its frequency and width.
    #[serde(default)]
    pub emitter_id: Option<u64>,
    /// Or a frequency, Hz.
    #[serde(default)]
    pub freq_hz: Option<f64>,
    /// Expected width, Hz (default: the emitter's; else unknown).
    #[serde(default)]
    pub width_hz: Option<f64>,
    /// Seconds of IQ (default 3, 1–15).
    #[serde(default)]
    pub secs: Option<f64>,
    /// Measure an existing snapshot instead of tuning + recording.
    #[serde(default)]
    pub snapshot: Option<String>,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub restore: bool,
}

/// Leave the analyzer, remembering its sweep knobs for `restore`.
async fn leave_analyzer(state: &AppState) -> Result<serde_json::Map<String, Value>, AppError> {
    let doc = serde_json::to_value(state.get_flowgraph().await).unwrap_or_default();
    let mut knobs = doc
        .pointer("/blocks/src/params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    knobs.retain(|k, _| {
        matches!(
            k.as_str(),
            "sweep_start_hz" | "sweep_stop_hz" | "rbw_hz" | "averages"
        )
    });
    state
        .return_from_stepped()
        .await
        .map_err(|e| AppError::radio_or_bad_request("INSPECT_FAILED", &e))?;
    Ok(knobs)
}

pub async fn post_inspect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<InspectReq>,
) -> Result<Json<Value>, AppError> {
    let by = caller(&headers);
    // 1. What to look at.
    let known = req.emitter_id.map(|id| {
        emitters::tracker()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(id)
            .cloned()
            .ok_or(id)
    });
    let emitter = match known {
        Some(Ok(e)) => Some(e),
        Some(Err(id)) => {
            return Err(AppError::not_found(
                "EMITTER_UNKNOWN",
                format!("no emitter #{id}"),
            ))
        }
        None => None,
    };
    let freq = match (&emitter, req.freq_hz) {
        (_, Some(f)) if f.is_finite() && f > 0.0 => f,
        (Some(e), _) => e.freq_hz,
        _ => {
            return Err(AppError::bad_request(
                "INSPECT_REJECTED",
                "give emitter_id or freq_hz",
            ))
        }
    };
    let emitter = emitter.or_else(|| {
        emitters::tracker()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .at(freq)
            .cloned()
    });
    let width = req
        .width_hz
        .or(emitter.as_ref().map(|e| e.width_hz))
        .filter(|w| w.is_finite() && *w > 0.0)
        .unwrap_or(0.0);

    // 2. Point the receiver at it (unless measuring an old snapshot).
    let mut left: Option<serde_json::Map<String, Value>> = None;
    if req.snapshot.is_none() {
        if state.stepped_active().await {
            if !req.force {
                return Err(AppError::conflict(
                    "RADIO_BUSY",
                    "the spectrum analyzer owns the radio — inspect with force=true to switch \
                     to the receiver (restore=true to go back to the sweep after)",
                ));
            }
            left = Some(leave_analyzer(&state).await?);
        }
        if state.status().await != PipelineStatus::Running {
            state
                .start()
                .await
                .map_err(|e| AppError::radio_or_bad_request("INSPECT_FAILED", &e))?;
        }
        let plan = state
            .tune(freq, None, None, true)
            .await
            .map_err(|e| AppError::radio_or_bad_request("INSPECT_FAILED", &e))?;
        if plan.is_some() || left.is_some() {
            tokio::time::sleep(SETTLE).await;
        }
    }
    let secs = req.secs.unwrap_or(3.0).clamp(1.0, 15.0);
    let result = measure(&state, &by, req.snapshot.as_deref(), secs, freq, width).await;

    // 3. Hand the radio back to the sweep if asked (even after an error).
    let mut restored = false;
    if let (Some(knobs), true) = (left, req.restore) {
        state
            .load_preset_by_name(PANORAMA_PRESET)
            .await
            .map_err(|e| AppError::radio_or_bad_request("INSPECT_FAILED", &e))?;
        if !knobs.is_empty() {
            let _ = state.apply_src_params(knobs).await;
        }
        restored = true;
    }
    let (mut out, f, analysis_id) = result?;

    // 4. Remember it on the emitter.
    if let Some(e) = &emitter {
        let summary = json!({
            "at_ms": crate::capture::now_ms(),
            "analysis_id": analysis_id,
            "by": by,
            "hint": f.hint,
            "freq_hz": f.freq_hz,
            "occupied_hz": f.occupied_hz,
            "snr_db": f.snr_db,
            "duty": f.duty,
            "keyed": f.keyed,
            "fsk_shift_hz": f.fsk_shift_hz,
            "period_s": f.bursts.period_s.or(f.envelope_period_s),
            "constant_envelope": f.constant_envelope,
        });
        let updated = emitters::tracker()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .attach_inspect(e.id, summary);
        if let Some(u) = updated {
            state
                .decoder_store()
                .apply(emitters::KIND, serde_json::to_value(&u).unwrap_or_default());
        }
        out["emitter"] =
            json!({ "id": e.id, "label": e.label, "band": e.band, "status": e.status });
    }
    out["restored"] = json!(restored);
    Ok(Json(out))
}

/// Record (or reuse) a snapshot and measure it; the response body, the
/// features, and the analysis record id.
async fn measure(
    state: &AppState,
    by: &str,
    snapshot: Option<&str>,
    secs: f64,
    freq: f64,
    width: f64,
) -> Result<(Value, fa::Features, String), AppError> {
    let (snap, path) = state.snapshot_for(snapshot, Some(secs)).await?;
    let half = snap.rate_hz / 2.0 * 0.95;
    let (s_lo, s_hi) = (snap.center_hz - half, snap.center_hz + half);
    if !(s_lo..=s_hi).contains(&freq) {
        return Err(AppError::bad_request(
            "INSPECT_REJECTED",
            format!(
                "{:.4} MHz is outside the snapshot ({:.4}–{:.4} MHz)",
                freq / 1e6,
                s_lo / 1e6,
                s_hi / 1e6
            ),
        ));
    }
    let span = if width > 0.0 {
        (width * 3.0).max(width + MIN_SPAN_HZ)
    } else {
        DEFAULT_SPAN_HZ
    }
    .clamp(MIN_SPAN_HZ, MAX_RANGE_HZ);
    let lo = (freq - span / 2.0).max(s_lo);
    let hi = (freq + span / 2.0).min(s_hi);
    let p = fa::InspectParams {
        lo_hz: lo,
        hi_hz: hi,
        freq_hz: freq,
        width_hz: width,
        dc_hz: (snap.tapped == "src").then_some(snap.center_hz),
        dc_guard_hz: DC_GUARD_HZ.min(span / 20.0),
    };
    let meta = fa::IqMeta {
        rate_hz: snap.rate_hz,
        center_hz: snap.center_hz,
    };
    let r = blocking(move || fa::inspect(|| open(&path), meta, p)).await?;
    let png = fa::png::spectrogram_png(&r.spectrogram);
    let text = fa::text::inspect(&r.features, &r.spectrogram);
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut result = serde_json::to_value(&r.spectrogram).unwrap_or_default();
    result["features"] = serde_json::to_value(&r.features).unwrap_or_default();
    let body = json!({
        "params": { "freq_hz": freq, "width_hz": width, "lo_hz": lo, "hi_hz": hi, "secs": secs },
        "result": result,
        "cells_b64": b64.encode(&r.spectrogram.cells),
        "text": text,
    });
    let id = state.publish_analysis("inspect", by, &snap, body);
    let out = json!({
        "analysis_id": id,
        "snapshot": snap,
        "lo_hz": lo,
        "hi_hz": hi,
        "features": r.features,
        "spectrogram": {
            "f0_hz": r.spectrogram.f0_hz, "df_hz": r.spectrogram.df_hz,
            "dt_s": r.spectrogram.dt_s, "rows": r.spectrogram.rows, "cols": r.spectrogram.cols,
            "note": "time runs down, frequency left → right",
        },
        "segments": r.envelope.segments.iter().take(40).collect::<Vec<_>>(),
        "text": text,
        "png_b64": b64.encode(&png),
    });
    Ok((out, r.features, id))
}
