//! `POST /api/listen` — "listen to this frequency with this decoder":
//! load the preset (if it changes), set the front end from the station
//! profile's row for the frequency (or the caller's overrides), tune,
//! start — one call instead of four that could each fail quietly. With
//! `secs`, it then listens: the decodes that arrived, what each decoder
//! saw (its `flowdiag` health, as deltas over the window) and a verdict
//! that tells "nothing there" from "a signal it can't sync" from "syncs
//! that never decode".
//!
//! Lease: like `tune` — while the spectrum analyzer owns the radio this
//! is 409 `RADIO_BUSY` unless `force`.

use std::collections::BTreeMap;
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::app_state::{AppState, PipelineStatus};
use crate::error::AppError;

/// Longest listen window, seconds.
const MAX_SECS: f64 = 300.0;
/// Decodes returned in full (the rest are counted).
const MAX_RECORDS: usize = 20;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListenReq {
    /// Frequency to listen at, Hz (the dial, as `tune`).
    pub freq_hz: f64,
    /// Preset slug (`nbfm`, `ft8-ft8`, `dsd-fme`, …). Omit to keep the
    /// running receiver preset.
    #[serde(default)]
    pub preset: Option<String>,
    /// Front-end overrides; unset ones come from the station profile.
    #[serde(default)]
    pub antenna: Option<String>,
    #[serde(default)]
    pub gain_db: Option<f64>,
    #[serde(default)]
    pub agc: Option<bool>,
    /// Apply the station profile's row for `freq_hz` (default true).
    #[serde(default)]
    pub station: Option<bool>,
    /// Listen this long after tuning and report (0 / omitted = return
    /// once tuned).
    #[serde(default)]
    pub secs: Option<f64>,
    /// Take the radio from the spectrum analyzer.
    #[serde(default)]
    pub force: bool,
}

fn caller(headers: &HeaderMap) -> String {
    headers
        .get("x-ferrite-via")
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| "ui".to_string(), str::to_string)
}

/// The decode kinds a preset publishes: every `ui:<kind>` wire target
/// that carries decodes (not meters / views).
fn decode_kinds(doc: &ferrite_runtime::FlowgraphDoc) -> Vec<String> {
    let mut kinds: Vec<String> = doc
        .wires
        .iter()
        .filter_map(|w| w.dst.strip_prefix("ui:"))
        .map(|k| k.split('.').next().unwrap_or(k).to_string())
        .filter(|k| crate::decoder_store::is_decode_kind(k))
        .filter(|k| !matches!(k.as_str(), "fft" | "audio" | "waterfall" | "panorama"))
        .collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

/// `{block_id: health}` from the store's latest `flowdiag`, plus each
/// block's type and input sample count.
fn health_now(state: &AppState) -> BTreeMap<String, Value> {
    let Some(diag) = state
        .decoder_store()
        .snapshot_kind("flowdiag")
        .and_then(|k| k.current.into_values().next())
    else {
        return BTreeMap::new();
    };
    let mut out = BTreeMap::new();
    for b in diag.data["blocks"].as_array().into_iter().flatten() {
        let Some(h) = b.get("health").filter(|h| h.is_object()) else {
            continue;
        };
        let id = b["id"].as_str().unwrap_or_default().to_string();
        let mut h = h.clone();
        h["type"] = b["type_name"].clone();
        out.insert(id, h);
    }
    out
}

/// `after` with every `*_cum` counter replaced by its increase since
/// `before` (keys renamed without `_cum`; `frames_cum` maps diffed per
/// frame type).
pub(crate) fn health_delta(before: Option<&Value>, after: &Value) -> Value {
    let mut out = Map::new();
    for (k, v) in after.as_object().into_iter().flatten() {
        let Some(name) = k.strip_suffix("_cum") else {
            out.insert(k.clone(), v.clone());
            continue;
        };
        let prev = before.and_then(|b| b.get(k));
        let d = match (v, prev) {
            (Value::Number(a), Some(Value::Number(b))) => {
                json!(a
                    .as_u64()
                    .unwrap_or(0)
                    .saturating_sub(b.as_u64().unwrap_or(0)))
            }
            (Value::Object(a), prev) => {
                let mut m = Map::new();
                for (fk, fv) in a {
                    let p = prev
                        .and_then(|p| p.get(fk))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    let n = fv.as_u64().unwrap_or(0).saturating_sub(p);
                    if n > 0 {
                        m.insert(fk.clone(), json!(n));
                    }
                }
                Value::Object(m)
            }
            _ => v.clone(),
        };
        out.insert(name.to_string(), d);
    }
    Value::Object(out)
}

fn u(h: &Value, k: &str) -> u64 {
    h.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// One line per decoder about what it saw over the window.
pub(crate) fn decoder_verdict(id: &str, h: &Value, secs: f64) -> Option<String> {
    let level = h
        .get("input_dbfs")
        .and_then(Value::as_f64)
        .map(|d| format!(" (input {d:.1} dBFS)"))
        .unwrap_or_default();
    if h.get("samples").is_some() && u(h, "samples") == 0 {
        return Some(format!(
            "{id}: no samples reached it — the chain is stalled or the source is down (check `health` / `flowdiag`)"
        ));
    }
    let line = match h["type"].as_str().unwrap_or_default() {
        "Ft8Demod" => {
            let (slots, dec) = (u(h, "slots"), u(h, "decodes"));
            let cand = u(h, "last_slot_candidates");
            if slots == 0 {
                format!("{id}: no slot completed in {secs:.0} s — listen at least one full slot (15 s FT8, 7.5 s FT4)")
            } else if dec > 0 {
                format!("{id}: {dec} decodes over {slots} slots{level}")
            } else if cand > 0 {
                format!("{id}: {cand} sync candidates last slot but nothing decoded — signals too weak or QRM; check gain / antenna{level}")
            } else {
                format!("{id}: {slots} slots, no FT8-shaped signal at all — dead band or wrong dial{level}")
            }
        }
        "Rtl433Demod" => {
            let pk = u(h, "ook_packages") + u(h, "fsk_packages");
            let dec = u(h, "decodes");
            if dec > 0 {
                format!("{id}: {dec} decodes from {pk} pulse packages{level}")
            } else if pk > 0 {
                format!("{id}: {pk} pulse packages but none matched a decoder — an unsupported protocol (try decoder_set=all) or a rate too narrow for it{level}")
            } else {
                format!("{id}: no pulses above the detector's threshold{level} — nothing keyed here, or too weak")
            }
        }
        "DsdHelper" => {
            if h.get("helper_alive") == Some(&Value::Bool(false)) {
                format!("{id}: dsd-fme isn't running — install it / set FERRITE_DSD_FME")
            } else {
                let syncs = u(h, "sync_lines");
                if syncs == 0 {
                    format!("{id}: no digital sync in {secs:.0} s — not a DMR / P25 / NXDN / D-STAR / YSF signal, off-centre, or too weak{level}")
                } else {
                    let proto = h["protocol"].as_str().unwrap_or("?");
                    let cc = h["cc"]
                        .as_u64()
                        .map(|c| format!(" CC {c}"))
                        .unwrap_or_default();
                    let frames = h.get("frames").map(Value::to_string).unwrap_or_default();
                    let sys = h
                        .get("system")
                        .map(|s| format!("; network {s}"))
                        .unwrap_or_default();
                    format!("{id}: {proto}{cc} sync ×{syncs}, frames {frames}{sys}")
                }
            }
        }
        "DmrDecode" => {
            let (syncs, lc) = (u(h, "syncs"), u(h, "lc_ok"));
            if syncs == 0 {
                format!("{id}: no DMR sync")
            } else if lc == 0 {
                format!("{id}: {syncs} DMR syncs but no call header decoded — a control / data channel, or errors (try the dsd-fme preset)")
            } else {
                format!("{id}: {syncs} DMR syncs, {lc} call headers")
            }
        }
        _ => format!("{id}: {h}"),
    };
    Some(line)
}

/// `POST /api/listen`.
pub async fn post_listen(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ListenReq>,
) -> Result<Json<Value>, AppError> {
    let by = caller(&headers);
    if !(req.freq_hz.is_finite() && req.freq_hz > 0.0) {
        return Err(AppError::bad_request(
            "LISTEN_REJECTED",
            "freq_hz must be a positive frequency",
        ));
    }
    // 1. The radio: leave the analyzer only when asked to.
    if state.stepped_active().await {
        if !req.force {
            return Err(AppError::conflict(
                "RADIO_BUSY",
                "the spectrum analyzer owns the radio — pass force=true to leave it",
            ));
        }
        if req.preset.is_none() {
            state
                .return_from_stepped()
                .await
                .map_err(|e| AppError::radio_or_bad_request("LISTEN_FAILED", &e))?;
        }
    }
    // 2. The preset.
    let mut loaded = false;
    if let Some(p) = &req.preset {
        if state.loaded_slug().as_deref() != Some(p.as_str()) || state.stepped_active().await {
            state
                .load_preset_by_name(p)
                .await
                .map_err(|e| AppError::radio_or_bad_request("LISTEN_FAILED", &e))?;
            loaded = true;
        }
    }
    let doc = state.get_flowgraph().await;
    if ferrite_runtime::wants_stepped(&doc) {
        return Err(AppError::bad_request(
            "LISTEN_REJECTED",
            "that's the spectrum analyzer — listen needs a receiver preset",
        ));
    }
    // 3. The front end: station row, then explicit overrides.
    let station = crate::station::load(&state.decoder_store());
    let row = if req.station.unwrap_or(true) {
        station.band_for(req.freq_hz).cloned()
    } else {
        None
    };
    let mut fe = row
        .as_ref()
        .map(crate::station::BandProfile::src_params)
        .unwrap_or_default();
    if let Some(a) = &req.antenna {
        fe.insert("antenna".into(), json!(a));
    }
    if let Some(g) = req.gain_db {
        fe.insert("gain_db".into(), json!(g));
        if req.agc.is_none() {
            fe.insert("agc".into(), json!(false));
        }
    }
    if let Some(agc) = req.agc {
        fe.insert("agc".into(), json!(agc));
    }
    let mut notes = Vec::new();
    if !fe.is_empty() {
        let (_, n) = state
            .apply_src_params(fe.clone())
            .await
            .map_err(|e| AppError::radio_or_bad_request("LISTEN_FAILED", &e))?;
        notes.extend(n);
    }
    // 4. Tune + run.
    state
        .tune(req.freq_hz, None, None, false)
        .await
        .map_err(|e| AppError::radio_or_bad_request("LISTEN_FAILED", &e))?;
    if state.status().await != PipelineStatus::Running {
        state
            .start()
            .await
            .map_err(|e| AppError::radio_or_bad_request("LISTEN_FAILED", &e))?;
    }
    let kinds = decode_kinds(&doc);
    let preset = state.loaded_slug().unwrap_or_else(|| doc.name.clone());
    let mut out = json!({
        "freq_hz": req.freq_hz,
        "preset": preset,
        "loaded": loaded,
        "front_end": {
            "applied": Value::Object(fe),
            "band": row.and_then(|r| r.label),
        },
        "decode_kinds": kinds,
        "notes": notes,
    });
    let secs = req.secs.unwrap_or(0.0).clamp(0.0, MAX_SECS);
    if secs <= 0.0 {
        out["readback"] = json!(state.source_readback().await);
        return Ok(Json(out));
    }
    // 5. Listen.
    let store = state.decoder_store();
    let start_seq = store.snapshot().seq;
    let before = health_now(&state);
    tokio::time::sleep(Duration::from_secs_f64(secs)).await;
    // One diag tick so the counters cover the whole window.
    tokio::time::sleep(Duration::from_millis(1_100)).await;
    let after = health_now(&state);
    let mut records = Vec::new();
    let mut by_kind = Map::new();
    for k in &kinds {
        let Some(ks) = store.snapshot_kind(k) else {
            continue;
        };
        let mut n = 0u64;
        for r in ks.recent.iter().chain(ks.current.values()) {
            if r.seq > start_seq {
                n += 1;
                records.push((
                    r.seq,
                    json!({ "kind": k, "seq": r.seq, "at_ms": r.at_ms, "data": r.data }),
                ));
            }
        }
        if n > 0 {
            by_kind.insert(k.clone(), json!(n));
        }
    }
    records.sort_by_key(|(s, _)| *s);
    let total = records.len();
    let recent: Vec<Value> = records
        .into_iter()
        .rev()
        .take(MAX_RECORDS)
        .rev()
        .map(|(_, v)| v)
        .collect();
    let health: Map<String, Value> = after
        .iter()
        .map(|(id, h)| (id.clone(), health_delta(before.get(id), h)))
        .collect();
    let mut lines: Vec<String> = Vec::new();
    if total > 0 {
        lines.push(format!(
            "{total} decode(s) in {secs:.0} s: {}",
            by_kind
                .iter()
                .map(|(k, n)| format!("{k} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else if kinds.is_empty() {
        lines.push(format!(
            "this preset publishes no decodes (listen-only); nothing to count in {secs:.0} s"
        ));
    } else {
        lines.push(format!(
            "nothing decoded in {secs:.0} s ({})",
            kinds.join(", ")
        ));
    }
    lines.extend(
        health
            .iter()
            .filter_map(|(id, h)| decoder_verdict(id, h, secs)),
    );
    if health.is_empty() && total == 0 && !kinds.is_empty() {
        lines.push(
            "(this preset's decoder reports no health — check level with `spectrum` / `inspect`)"
                .into(),
        );
    }
    let verdict = lines.join("\n");
    out["secs"] = json!(secs);
    out["decodes"] = json!({ "count": total, "by_kind": by_kind, "recent": recent });
    out["health"] = Value::Object(health);
    out["verdict"] = json!(verdict);
    out["readback"] = json!(state.source_readback().await);
    // Visible in the AI panel, whoever asked.
    store.apply(
        "analysis",
        json!({
            "id": format!("listen-{}", crate::capture::now_ms()),
            "tool": "listen",
            "by": by,
            "params": { "freq_hz": req.freq_hz, "preset": out["preset"], "secs": secs },
            "result": { "decodes": out["decodes"]["count"], "by_kind": out["decodes"]["by_kind"] },
            "text": verdict,
        }),
    );
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_delta_diffs_counters_and_keeps_latest_values() {
        let before = json!({"slots_cum": 4, "decodes_cum": 10, "frames_cum": {"IDLE": 5}});
        let after = json!({
            "type": "Ft8Demod", "slots_cum": 6, "decodes_cum": 10,
            "last_slot_candidates": 7, "input_dbfs": -40.0,
            "frames_cum": {"IDLE": 9, "CSBK": 2}
        });
        let d = health_delta(Some(&before), &after);
        assert_eq!(d["slots"], 2);
        assert_eq!(d["decodes"], 0);
        assert_eq!(d["last_slot_candidates"], 7);
        assert_eq!(d["frames"], json!({"IDLE": 4, "CSBK": 2}));
    }

    #[test]
    fn verdicts_tell_silence_from_unsynced_from_undecoded() {
        let h = json!({"type": "Ft8Demod", "slots": 2, "decodes": 0, "last_slot_candidates": 5, "samples": 9});
        assert!(decoder_verdict("ft8", &h, 30.0)
            .unwrap()
            .contains("candidates"));
        let h = json!({"type": "Rtl433Demod", "ook_packages": 3, "fsk_packages": 0, "decodes": 0, "samples": 9});
        assert!(decoder_verdict("rtl433", &h, 60.0)
            .unwrap()
            .contains("none matched"));
        let h = json!({"type": "DsdHelper", "helper_alive": true, "sync_lines": 0, "samples": 9});
        assert!(decoder_verdict("dsd", &h, 20.0)
            .unwrap()
            .contains("no digital sync"));
        let h = json!({"type": "DsdHelper", "helper_alive": true, "sync_lines": 40,
                       "protocol": "DMR", "cc": 3, "frames": {"CSBK": 20},
                       "system": {"system": "Connect Plus", "site_id": 28}, "samples": 9});
        let v = decoder_verdict("dsd", &h, 20.0).unwrap();
        assert!(v.contains("DMR CC 3") && v.contains("Connect Plus"), "{v}");
        let h = json!({"type": "Ft8Demod", "samples": 0});
        assert!(decoder_verdict("ft8", &h, 30.0)
            .unwrap()
            .contains("no samples"));
    }

    #[test]
    fn decode_kinds_skip_meters_and_views() {
        let doc: ferrite_runtime::FlowgraphDoc = serde_json::from_value(json!({
            "name": "t", "environments": ["node"], "blocks": {},
            "wires": [["a.events", "ui:ft8"], ["b.out", "ui:fft"], ["c.events", "ui:rssi"],
                      ["d.events", "ui:ft8"]]
        }))
        .unwrap();
        assert_eq!(decode_kinds(&doc), ["ft8"]);
    }
}
