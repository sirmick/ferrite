//! `POST /api/decode` — run a decoder preset over recorded IQ instead of
//! the live radio: a kept snapshot (`snapshot`, from `snapshot` /
//! `inspect`) or a capture file (`path`, a `.cf32` with its sidecar
//! JSON). Capture once, then try several decoders on the same seconds
//! without retuning or waiting for the next transmission — and a hunt
//! becomes reproducible.
//!
//! It builds a second, short-lived runtime (`FileIqSource` → the
//! preset's chain, every block node-side, no audio) in a blocking task,
//! collects what the decoders emit, and reports their health like
//! `listen`. The live pipeline is untouched. Presets whose decoders keep
//! process-global state or time slots by the wall clock are refused.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use ferrite_blocks::event_store::{DecoderSink, EventStore};
use ferrite_blocks::file_source::FileIqSource;
use ferrite_runtime::{
    apply_profile, compose_source, split_for_environment, AudioSplit, Environment, FlowgraphDoc,
    InventorySpecRegistry, Profile, Runtime, SourceConfig, DEFAULT_FRAMES_HINT,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::app_state::AppState;
use crate::error::AppError;

/// Decoders that can't run beside the live receiver offline: FT8 / WSPR
/// cut their slots from the wall clock (a file plays faster than real
/// time), and fldigi modems share a process-global waterfall stub.
const OFFLINE_UNSAFE: &[&str] = &[
    "Ft8Demod",
    "WsprDemod",
    "FldigiAuto",
    "ContestiaDemod",
    "CwDemod",
    "DominoexDemod",
    "Mt63Demod",
    "NavtexDemod",
    "OliviaDemod",
    "Psk31Demod",
    "RttyDemod",
    "ThrobDemod",
];
/// How fast the file plays, × real time (subprocess helpers like dsd-fme
/// read a bounded socket buffer; faster drops input).
const DEFAULT_SPEED: f64 = 4.0;
/// Keep ticking this long after the file ends so asynchronous helpers
/// (dsd-fme over TCP) finish what they were given.
const DRAIN_AFTER_EOF: Duration = Duration::from_millis(1_500);
/// Hard ceiling on one offline run.
const MAX_RUN: Duration = Duration::from_secs(120);
const MAX_RECORDS: usize = 50;
/// Where `capture iq` writes by default.
const CAPTURE_TMP: &str = "/tmp/ferrite-captures";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DecodeReq {
    /// A kept snapshot id (`snap-…`).
    #[serde(default)]
    pub snapshot: Option<String>,
    /// Or a `.cf32` capture (its sidecar `.json` gives rate + centre),
    /// under the captures directory or /tmp/ferrite-captures.
    #[serde(default)]
    pub path: Option<String>,
    /// Decoder preset slug (`dsd-fme`, `dmr`, `rtl433-433`, `pager`, …).
    pub preset: String,
    /// Channel to decode, Hz (default: the recording's centre).
    #[serde(default)]
    pub freq_hz: Option<f64>,
    /// Playback speed × real time (default 4, 1–50).
    #[serde(default)]
    pub speed: Option<f64>,
}

/// Collects what the offline graph's `EventStore`s emit.
#[derive(Default)]
struct Collector(Mutex<Vec<(String, Value)>>);

impl DecoderSink for Collector {
    fn emit(&self, kind: &str, data: Value) {
        if let Ok(mut v) = self.0.lock() {
            v.push((kind.to_string(), data));
        }
    }
    fn reset(&self, _kind: &str) {}
}

/// Recording to play: path, centre, rate.
struct Recording {
    label: String,
    path: PathBuf,
    center_hz: f64,
    rate_hz: f64,
}

fn allowed_roots(state: &AppState) -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from(CAPTURE_TMP)];
    if let Some(d) = state.captures_dir() {
        roots.push(d.to_path_buf());
    }
    roots
        .into_iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect()
}

fn read_sidecar(path: &Path) -> Option<(f64, f64)> {
    let text = std::fs::read_to_string(path.with_extension("json")).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let side = v.get("sidecar").unwrap_or(&v);
    Some((
        side.get("center_freq_hz")?.as_f64()?,
        side.get("sample_rate_hz")?.as_f64()?,
    ))
}

async fn recording(state: &AppState, req: &DecodeReq) -> Result<Recording, AppError> {
    if let Some(id) = &req.snapshot {
        let (info, path) = state.snapshot_for(Some(id), None).await?;
        return Ok(Recording {
            label: id.clone(),
            path,
            center_hz: info.center_hz,
            rate_hz: info.rate_hz,
        });
    }
    let Some(p) = &req.path else {
        return Err(AppError::bad_request(
            "DECODE_REJECTED",
            "give `snapshot` (an id from snapshot / inspect) or `path` (a .cf32 capture)",
        ));
    };
    let path = std::fs::canonicalize(p)
        .map_err(|e| AppError::not_found("DECODE_NO_FILE", format!("{p}: {e}")))?;
    if !allowed_roots(state).iter().any(|r| path.starts_with(r)) {
        return Err(AppError::bad_request(
            "DECODE_REJECTED",
            "path must be under the captures directory or /tmp/ferrite-captures",
        ));
    }
    let (center_hz, rate_hz) = read_sidecar(&path).ok_or_else(|| {
        AppError::bad_request(
            "DECODE_REJECTED",
            "no sidecar .json with center_freq_hz / sample_rate_hz next to the capture",
        )
    })?;
    Ok(Recording {
        label: path.display().to_string(),
        path,
        center_hz,
        rate_hz,
    })
}

/// The preset recomposed to play `rec` with every block node-side, no
/// audio, its channelizers aimed at `freq_hz`.
fn offline_doc(
    preset: &FlowgraphDoc,
    rec: &Recording,
    freq_hz: f64,
) -> Result<FlowgraphDoc, String> {
    let source = SourceConfig {
        type_name: "FileIqSource".into(),
        params: json!({
            "path": rec.path,
            "rate_hz_hint": rec.rate_hz,
            "center_freq_hz": rec.center_hz,
            "loop_playback": false,
        }),
    };
    let mut doc = compose_source(preset, &source).map_err(|e| format!("compose: {e}"))?;
    for decl in doc.blocks.values_mut() {
        if decl.type_name == "Channelizer" {
            let mut p = match decl.params.take() {
                Some(Value::Object(m)) => m,
                _ => Map::new(),
            };
            p.insert("freq_shift_hz".into(), json!(freq_hz - rec.center_hz));
            p.insert("input_rate_hz".into(), json!(rec.rate_hz));
            decl.params = Some(Value::Object(p));
        }
    }
    let profile = Profile {
        audio: false,
        transcribe: false,
        audio_split: AudioSplit::Server,
    };
    apply_profile(&mut doc, &profile, &InventorySpecRegistry);
    split_for_environment(&doc, Environment::Node, &InventorySpecRegistry)
        .map_err(|e| format!("env_split: {e}"))
}

/// `(kind or block id, JSON)` pairs.
type Tagged = Vec<(String, Value)>;

/// What one offline run produced.
struct OfflineRun {
    /// `(kind, data)` per emitted record.
    records: Tagged,
    /// `(block id, health)` at the end.
    health: Tagged,
    /// Seconds of recording played.
    secs: f64,
}

/// Play the file through `doc`; returns the emitted records and the
/// final flowdiag health per block.
fn run_offline(doc: &FlowgraphDoc, rate_hz: f64, speed: f64) -> anyhow::Result<OfflineRun> {
    let mut rt = Runtime::load_doc(
        doc,
        Environment::Node,
        DEFAULT_FRAMES_HINT,
        Duration::from_millis(10),
    )?;
    let sink = Arc::new(Collector::default());
    let store_ids: Vec<String> = doc
        .blocks
        .iter()
        .filter(|(_, d)| d.type_name == "EventStore")
        .map(|(id, _)| id.clone())
        .collect();
    for id in &store_ids {
        if let Some(es) = rt.block_typed::<EventStore>(id) {
            es.attach_store(Arc::clone(&sink) as Arc<dyn DecoderSink>);
        }
    }
    rt.init()?;
    rt.start()?;
    let t0 = Instant::now();
    let mut eof_at: Option<Instant> = None;
    let src_samples = |rt: &Runtime| -> u64 {
        rt.diag_snapshot()
            .blocks
            .iter()
            .find(|b| b.id == ferrite_runtime::SOURCE_ID)
            .and_then(|b| b.outputs.first())
            .map_or(0, |p| p.samples_cum)
    };
    loop {
        rt.tick()?;
        let eof = rt
            .block_typed::<FileIqSource>(ferrite_runtime::SOURCE_ID)
            .is_none_or(|s| s.is_eof());
        if eof && eof_at.is_none() {
            eof_at = Some(Instant::now());
        }
        if eof_at.is_some_and(|t| t.elapsed() >= DRAIN_AFTER_EOF) || t0.elapsed() >= MAX_RUN {
            break;
        }
        // Pace to `speed` × real time.
        #[allow(clippy::cast_precision_loss)]
        let played = src_samples(&rt) as f64 / rate_hz;
        let due = Duration::from_secs_f64(played / speed);
        if let Some(wait) = due.checked_sub(t0.elapsed()) {
            std::thread::sleep(wait.min(Duration::from_millis(50)));
        } else if eof_at.is_some() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let secs = src_samples(&rt) as f64 / rate_hz;
    let health = rt
        .diag_snapshot()
        .blocks
        .into_iter()
        .filter_map(|b| {
            let mut h = b.health?;
            h["type"] = json!(b.type_name);
            Some((b.id, h))
        })
        .collect();
    let _ = rt.stop();
    let records = std::mem::take(
        &mut *sink
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    Ok(OfflineRun {
        records,
        health,
        secs,
    })
}

/// `POST /api/decode`.
pub async fn post_decode(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DecodeReq>,
) -> Result<Json<Value>, AppError> {
    let by = headers
        .get("x-ferrite-via")
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| "ui".to_string(), str::to_string);
    let rec = recording(&state, &req).await?;
    let raw = state
        .read_preset_doc(&req.preset)
        .await
        .map_err(|e| AppError::not_found("DECODE_NO_PRESET", format!("{e:#}")))?;
    let preset: FlowgraphDoc = serde_json::from_value(raw)
        .map_err(|e| AppError::bad_request("DECODE_REJECTED", format!("preset: {e}")))?;
    if ferrite_runtime::wants_stepped(&preset) {
        return Err(AppError::bad_request(
            "DECODE_REJECTED",
            "that's the spectrum analyzer, not a decoder",
        ));
    }
    if let Some(t) = preset
        .blocks
        .values()
        .map(|b| b.type_name.as_str())
        .find(|t| OFFLINE_UNSAFE.contains(t))
    {
        return Err(AppError::bad_request(
            "DECODE_UNSUPPORTED",
            format!(
                "{t} can't run offline (wall-clock slots or process-global state shared with the live receiver) — `listen` live instead"
            ),
        ));
    }
    let freq_hz = req.freq_hz.unwrap_or(rec.center_hz);
    if (freq_hz - rec.center_hz).abs() > rec.rate_hz / 2.0 {
        return Err(AppError::bad_request(
            "DECODE_REJECTED",
            format!(
                "{:.4} MHz is outside the recording ({:.4} MHz ± {:.0} kHz)",
                freq_hz / 1e6,
                rec.center_hz / 1e6,
                rec.rate_hz / 2e3
            ),
        ));
    }
    let doc = offline_doc(&preset, &rec, freq_hz)
        .map_err(|e| AppError::bad_request("DECODE_REJECTED", e))?;
    let speed = req.speed.unwrap_or(DEFAULT_SPEED).clamp(1.0, 50.0);
    let rate = rec.rate_hz;
    let OfflineRun {
        records,
        health,
        secs,
    } = tokio::task::spawn_blocking(move || run_offline(&doc, rate, speed))
        .await
        .map_err(|e| AppError::internal("DECODE_FAILED", format!("{e}")))?
        .map_err(|e| AppError::internal("DECODE_FAILED", format!("{e:#}")))?;
    let mut by_kind = Map::new();
    for (k, _) in &records {
        let n = by_kind.get(k).and_then(Value::as_u64).unwrap_or(0);
        by_kind.insert(k.clone(), json!(n + 1));
    }
    let total = records.len();
    let health: Map<String, Value> = health
        .into_iter()
        .map(|(id, h)| (id, crate::listen_api::health_delta(None, &h)))
        .collect();
    let mut lines = vec![if total > 0 {
        format!(
            "{total} record(s) from {secs:.1} s of {}: {}",
            rec.label,
            by_kind
                .iter()
                .map(|(k, n)| format!("{k} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        format!("nothing decoded from {secs:.1} s of {}", rec.label)
    }];
    lines.extend(
        health
            .iter()
            .filter_map(|(id, h)| crate::listen_api::decoder_verdict(id, h, secs)),
    );
    let verdict = lines.join("\n");
    let shown: Vec<Value> = records
        .iter()
        .take(MAX_RECORDS)
        .map(|(k, d)| json!({ "kind": k, "data": d }))
        .collect();
    let out = json!({
        "recording": rec.label,
        "preset": req.preset,
        "freq_hz": freq_hz,
        "center_hz": rec.center_hz,
        "rate_hz": rec.rate_hz,
        "secs": secs,
        "decodes": { "count": total, "by_kind": by_kind, "records": shown },
        "health": health,
        "verdict": verdict,
    });
    state.decoder_store().apply(
        "analysis",
        json!({
            "id": format!("decode-{}", crate::capture::now_ms()),
            "tool": "decode",
            "by": by,
            "params": { "recording": out["recording"], "preset": req.preset, "freq_hz": freq_hz },
            "result": { "decodes": total, "by_kind": out["decodes"]["by_kind"] },
            "text": verdict,
        }),
    );
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped dsd-fme preset recomposed for a recording: the file
    /// source stands in for the radio, the channelizer is aimed at the
    /// requested channel, and the audio chain is gone.
    #[test]
    fn offline_doc_plays_the_file_through_the_preset() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../flowgraphs/dsd-fme.json");
        let preset: FlowgraphDoc =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let rec = Recording {
            label: "t".into(),
            path: PathBuf::from("/tmp/x.cf32"),
            center_hz: 454.3e6,
            rate_hz: 2.4e6,
        };
        let doc = offline_doc(&preset, &rec, 454.3443e6).unwrap();
        assert_eq!(doc.blocks["src"].type_name, "FileIqSource");
        let chan = doc.blocks["chan"].params.as_ref().unwrap();
        assert!((chan["freq_shift_hz"].as_f64().unwrap() - 44_300.0).abs() < 1.0);
        assert!(doc.blocks.values().all(|b| b.type_name != "AudioSink"));
        assert!(doc.blocks.values().any(|b| b.type_name == "DsdHelper"));
    }
}
