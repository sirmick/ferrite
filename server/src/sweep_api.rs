//! `POST /api/sweep` — the spectrum analyzer as a *measurement*: what an
//! agent (MCP `sweep`), a person (`ferrite-ctl sweep`) or the UI gets
//! back instead of thousands of raw bins.
//!
//! Optionally (re)aims the analyzer — range / RBW / averages — then
//! collects `passes` fresh passes from the `panorama` store kind and
//! summarises them: a noise floor per step (so the RSPdx's per-band LNA
//! steps don't read as signals), the emitters above it (frequency,
//! level, occupied width, duty across passes), and a coarse max-hold
//! profile. The summary also lands as an `analysis` record, so the AI
//! panel shows what was measured, whoever asked.
//!
//! Lease: if the receiver is running, switching it to the analyzer
//! needs `force` (409 `RADIO_BUSY` otherwise); a stopped radio is free.
//! `restore` puts the receiver back afterwards.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::HeaderMap;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app_state::{AppState, PipelineStatus};
use crate::error::AppError;

const PANORAMA_PRESET: &str = "panorama";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SweepReq {
    #[serde(default)]
    pub lo_hz: Option<f64>,
    #[serde(default)]
    pub hi_hz: Option<f64>,
    #[serde(default)]
    pub rbw_hz: Option<f64>,
    #[serde(default)]
    pub averages: Option<u32>,
    /// Fresh passes to fold (max-hold / duty across them). Default 1.
    #[serde(default)]
    pub passes: Option<u32>,
    /// A bin is "signal" when its max-hold is this far over its step's
    /// floor. Default 10 dB.
    #[serde(default)]
    pub threshold_db: Option<f32>,
    #[serde(default)]
    pub max_peaks: Option<usize>,
    #[serde(default)]
    pub profile_points: Option<usize>,
    /// Give up after this long (default 120 s, max 600).
    #[serde(default)]
    pub timeout_s: Option<f64>,
    /// Antenna for the whole sweep (overrides the station profile).
    #[serde(default)]
    pub antenna: Option<String>,
    /// Gain dial for the whole sweep, dB (overrides the station profile).
    #[serde(default)]
    pub gain_db: Option<f64>,
    /// Apply the station profile's per-band antenna / gain (default
    /// true; ignored for whatever `antenna` / `gain_db` override).
    #[serde(default)]
    pub station: Option<bool>,
    /// Take the radio from a running receiver.
    #[serde(default)]
    pub force: bool,
    /// Put the receiver back when done.
    #[serde(default)]
    pub restore: bool,
}

/// One emitter in the summary. `*_db` are over the local (per-step)
/// floor, like the `spectrum` tool's peaks.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct SweepPeak {
    pub freq_hz: f64,
    /// Max-hold level, dBFS.
    pub level_dbfs: f32,
    /// Max-hold over the floor, dB.
    pub peak_db: f32,
    /// Power-average over the floor, dB.
    pub avg_db: f32,
    /// Occupied width: span of the max-hold above the threshold, Hz.
    pub width_hz: f64,
    /// Fraction of passes the signal was up (≥ threshold).
    pub duty: f32,
    /// Seen in some passes but not others (needs ≥ 2 passes).
    pub bursty: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SweepSummary {
    pub range_hz: [f64; 2],
    pub rbw_hz: f64,
    pub steps: usize,
    pub passes: u32,
    /// Median of the per-step floors, dBFS.
    pub floor_dbfs: f32,
    /// Lowest / highest per-step floor — a wide spread is the radio's
    /// gain ladder changing across bands, not signal.
    pub floor_range_dbfs: [f32; 2],
    pub peaks: Vec<SweepPeak>,
    /// `[freq_hz, maxhold_dbfs]`, evenly spaced over the range.
    pub profile: Vec<[f64; 2]>,
}

/// Per-step accumulator over passes.
#[derive(Debug, Clone)]
pub struct StepAcc {
    pub f_lo_hz: f64,
    pub bin_hz: f64,
    pub max: Vec<f32>,
    pub sum_lin: Vec<f64>,
    pub on: Vec<u32>,
    pub passes: u32,
}

impl StepAcc {
    pub fn new(f_lo_hz: f64, bin_hz: f64, n: usize) -> Self {
        Self {
            f_lo_hz,
            bin_hz,
            max: vec![f32::NEG_INFINITY; n],
            sum_lin: vec![0.0; n],
            on: vec![0; n],
            passes: 0,
        }
    }

    /// Fold one pass of this step's row.
    pub fn fold(&mut self, db: &[f32], threshold_db: f32) {
        let floor = median(db.iter().copied());
        for (k, &v) in db.iter().enumerate().take(self.max.len()) {
            if !v.is_finite() {
                continue;
            }
            self.max[k] = self.max[k].max(v);
            self.sum_lin[k] += 10f64.powf(f64::from(v) / 10.0);
            if v >= floor + threshold_db {
                self.on[k] += 1;
            }
        }
        self.passes += 1;
    }

    fn avg_db(&self, k: usize) -> f32 {
        if self.passes == 0 || self.sum_lin[k] <= 0.0 {
            return f32::NAN;
        }
        (10.0 * (self.sum_lin[k] / f64::from(self.passes)).log10()) as f32
    }

    fn floor(&self) -> f32 {
        median((0..self.max.len()).map(|k| self.avg_db(k)))
    }
}

fn median(it: impl Iterator<Item = f32>) -> f32 {
    let mut v: Vec<f32> = it.filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return f32::NAN;
    }
    v.sort_by(f32::total_cmp);
    v[v.len() / 2]
}

/// The pure part: stitched steps → summary.
#[must_use]
pub fn summarize(
    steps: &[StepAcc],
    range: [f64; 2],
    threshold_db: f32,
    max_peaks: usize,
    profile_points: usize,
) -> SweepSummary {
    struct Bin {
        f: f64,
        max: f32,
        avg: f32,
        on: u32,
        floor: f32,
        passes: u32,
    }
    let floors: Vec<f32> = steps.iter().map(StepAcc::floor).collect();
    let mut bins: Vec<Bin> = Vec::new();
    for (s, &floor) in steps.iter().zip(&floors) {
        for k in 0..s.max.len() {
            bins.push(Bin {
                f: s.f_lo_hz + k as f64 * s.bin_hz,
                max: s.max[k],
                avg: s.avg_db(k),
                on: s.on[k],
                floor,
                passes: s.passes,
            });
        }
    }
    let bin_hz = steps.first().map_or(0.0, |s| s.bin_hz);
    let hot = |b: &Bin| b.max.is_finite() && b.max - b.floor >= threshold_db;
    // Clusters of hot bins. A modulated signal's max-hold at a fine RBW
    // is spiky (broadcast FM drops below threshold between sidebands; HD
    // Radio sidebands sit just outside the analog carrier), so bridge
    // gaps up to ~15 kHz (at least 3 bins): one emitter,
    // one peak. (The emitter list groups broadcast FM by channel cell.)
    let bridge = ((15_000.0 / bin_hz.max(1.0)).round() as usize).max(3);
    let mut clusters: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < bins.len() {
        if !hot(&bins[i]) {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        let mut j = i + 1;
        while j < bins.len() && j <= end + bridge {
            if hot(&bins[j]) {
                end = j;
            }
            j += 1;
        }
        clusters.push((start, end));
        i = end + 1;
    }
    let mut peaks: Vec<SweepPeak> = clusters
        .iter()
        .map(|&(a, b)| {
            let k = (a..=b)
                .max_by(|&x, &y| bins[x].max.total_cmp(&bins[y].max))
                .unwrap_or(a);
            let top = bins[k].max;
            // Parabolic interpolation on the max-hold for a sub-bin centre.
            let mut f = bins[k].f;
            if k > 0 && k + 1 < bins.len() {
                let (l, c, r) = (bins[k - 1].max, top, bins[k + 1].max);
                let den = l - 2.0 * c + r;
                if l.is_finite() && r.is_finite() && den.abs() > 1e-6 {
                    let d = (0.5 * (l - r) / den).clamp(-0.5, 0.5);
                    f += f64::from(d) * bin_hz;
                }
            }
            // Occupied width: the cluster's span above the threshold.
            let (lo, hi) = (a, b);
            let passes = bins[k].passes.max(1);
            let duty = bins[k].on as f32 / passes as f32;
            SweepPeak {
                freq_hz: f.round(),
                level_dbfs: round1(top),
                peak_db: round1(top - bins[k].floor),
                avg_db: round1(bins[k].avg - bins[k].floor),
                width_hz: ((hi - lo + 1) as f64 * bin_hz).round(),
                duty: (duty * 100.0).round() / 100.0,
                bursty: passes >= 2 && duty < 0.9,
            }
        })
        .collect();
    peaks.sort_by(|a, b| b.peak_db.total_cmp(&a.peak_db));
    peaks.truncate(max_peaks);
    let profile = if profile_points == 0 || bins.is_empty() {
        Vec::new()
    } else {
        let (lo, hi) = (range[0], range[1]);
        let w = (hi - lo) / profile_points as f64;
        (0..profile_points)
            .map(|p| {
                let (a, b) = (lo + p as f64 * w, lo + (p + 1) as f64 * w);
                let m = bins
                    .iter()
                    .filter(|x| x.f >= a && x.f < b)
                    .map(|x| x.max)
                    .fold(f32::NEG_INFINITY, f32::max);
                [
                    ((a + b) / 2.0).round(),
                    if m.is_finite() {
                        f64::from(round1(m))
                    } else {
                        f64::NAN
                    },
                ]
            })
            .collect()
    };
    let finite: Vec<f32> = floors.iter().copied().filter(|f| f.is_finite()).collect();
    SweepSummary {
        range_hz: range,
        rbw_hz: bin_hz.round(),
        steps: steps.len(),
        passes: steps.iter().map(|s| s.passes).min().unwrap_or(0),
        floor_dbfs: round1(median(floors.iter().copied())),
        floor_range_dbfs: [
            round1(finite.iter().copied().fold(f32::INFINITY, f32::min)),
            round1(finite.iter().copied().fold(f32::NEG_INFINITY, f32::max)),
        ],
        peaks,
        profile,
    }
}

fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

/// Plain-text rendering for the CLI / the AI panel.
#[must_use]
pub fn render_text(s: &SweepSummary) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let mut out = String::new();
    let vals: Vec<f64> = s
        .profile
        .iter()
        .map(|p| p[1])
        .filter(|v| v.is_finite())
        .collect();
    if let (Some(lo), Some(hi)) = (
        vals.iter().copied().reduce(f64::min),
        vals.iter().copied().reduce(f64::max),
    ) {
        let span = (hi - lo).max(1.0);
        out.extend(s.profile.iter().map(|p| {
            if p[1].is_finite() {
                BARS[(((p[1] - lo) / span) * 7.0).round() as usize]
            } else {
                ' '
            }
        }));
        out.push('\n');
    }
    out.push_str(&format!(
        "{:.3}–{:.3} MHz · {} steps · RBW {:.0} Hz · {} pass(es) · floor {:.1} dBFS ({:.1}…{:.1})\n",
        s.range_hz[0] / 1e6,
        s.range_hz[1] / 1e6,
        s.steps,
        s.rbw_hz,
        s.passes,
        s.floor_dbfs,
        s.floor_range_dbfs[0],
        s.floor_range_dbfs[1],
    ));
    for p in &s.peaks {
        out.push_str(&format!(
            "{:>12.4} MHz  +{:>4.1} dB  {:>6.1} dBFS  {:>7.1} kHz{}\n",
            p.freq_hz / 1e6,
            p.peak_db,
            p.level_dbfs,
            p.width_hz / 1e3,
            if p.bursty {
                format!("  bursty (duty {:.2})", p.duty)
            } else {
                String::new()
            }
        ));
    }
    out
}

/// Who asked (activity attribution), same rule as the analysis tools.
fn caller(headers: &HeaderMap) -> String {
    headers
        .get("x-ferrite-via")
        .and_then(|v| v.to_str().ok())
        .map_or_else(|| "ui".to_string(), str::to_string)
}

/// Rows of the newest config newer than `since_ms`: `(step, pass, row)`.
fn fresh_rows(state: &AppState, since_ms: u64) -> (Option<u64>, Vec<Value>) {
    let Some(kind) = state.decoder_store().snapshot_kind("panorama") else {
        return (None, Vec::new());
    };
    let rows: Vec<Value> = kind.current.into_values().map(|r| r.data).collect();
    let cfg = rows.iter().filter_map(|r| r.get("cfg")?.as_u64()).max();
    let rows = rows
        .into_iter()
        .filter(|r| r.get("cfg").and_then(Value::as_u64) == cfg)
        .filter(|r| r.get("t_ms").and_then(Value::as_u64).unwrap_or(0) > since_ms)
        .collect();
    (cfg, rows)
}

pub async fn post_sweep(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SweepReq>,
) -> Result<Json<Value>, AppError> {
    let by = caller(&headers);
    // 1. Take the radio if needed.
    let mut entered = false;
    if !state.stepped_active().await {
        let running = state.status().await == PipelineStatus::Running;
        if running && !req.force {
            return Err(AppError::conflict(
                "RADIO_BUSY",
                "the receiver is using the radio — pass force=true to switch it to the \
                 spectrum analyzer (and restore=true to put the receiver back after)",
            ));
        }
        state
            .load_preset_by_name(PANORAMA_PRESET)
            .await
            .map_err(|e| AppError::radio_or_bad_request("SWEEP_FAILED", &e))?;
        entered = true;
    }
    if state.status().await != PipelineStatus::Running {
        state
            .start()
            .await
            .map_err(|e| AppError::radio_or_bad_request("SWEEP_FAILED", &e))?;
    }
    // 2. Aim it.
    let mut aim = serde_json::Map::new();
    if let Some(v) = req.lo_hz {
        aim.insert("sweep_start_hz".into(), json!(v));
    }
    if let Some(v) = req.hi_hz {
        aim.insert("sweep_stop_hz".into(), json!(v));
    }
    if let Some(v) = req.rbw_hz {
        aim.insert("rbw_hz".into(), json!(v));
    }
    if let Some(v) = req.averages {
        aim.insert("averages".into(), json!(v));
    }
    // Always restate the front-end table: it stays staged on the
    // analyzer, and a previous sweep's override mustn't linger.
    aim.insert(
        "bands".into(),
        crate::station::sweep_bands(
            &crate::station::load(&state.decoder_store()),
            req.station.unwrap_or(true),
            req.antenna.as_deref(),
            req.gain_db,
        ),
    );
    if let (Some(lo), Some(hi)) = (req.lo_hz, req.hi_hz) {
        if !(hi > lo) {
            return Err(AppError::bad_request(
                "SWEEP_REJECTED",
                "need lo_hz < hi_hz",
            ));
        }
    }
    if !aim.is_empty() {
        state
            .apply_src_params(aim)
            .await
            .map_err(|e| AppError::radio_or_bad_request("SWEEP_FAILED", &e))?;
    }
    // 3. Collect fresh passes.
    let since = crate::capture::now_ms() as u64;
    let passes = req.passes.unwrap_or(1).clamp(1, 20);
    let thr = req.threshold_db.unwrap_or(10.0).clamp(3.0, 60.0);
    let deadline =
        Instant::now() + Duration::from_secs_f64(req.timeout_s.unwrap_or(120.0).clamp(5.0, 600.0));
    let mut accs: BTreeMap<u64, StepAcc> = BTreeMap::new();
    let mut seen: BTreeSet<(u64, u64)> = BTreeSet::new(); // (step, pass)
    let mut steps_total = 0usize;
    let mut range = [0.0f64; 2];
    let mut cfg_seen = None;
    let mut front_ends: BTreeMap<u64, StepFrontEnd> = BTreeMap::new();
    let complete = loop {
        let (cfg, rows) = fresh_rows(&state, since);
        if cfg != cfg_seen {
            // A config change mid-collection (someone re-aimed): restart.
            accs.clear();
            seen.clear();
            front_ends.clear();
            cfg_seen = cfg;
        }
        for r in rows {
            let (Some(step), Some(pass)) = (
                r.get("step").and_then(Value::as_u64),
                r.get("pass").and_then(Value::as_u64),
            ) else {
                continue;
            };
            if !seen.insert((step, pass)) {
                continue;
            }
            let Some(db) = r
                .get("power")
                .and_then(Value::as_str)
                .and_then(|b| ferrite_blocks::sweep::decode_power(b).ok())
            else {
                continue;
            };
            steps_total = r.get("steps").and_then(Value::as_u64).unwrap_or(0) as usize;
            if let Some(rg) = r.get("range").and_then(Value::as_array) {
                range = [
                    rg.first().and_then(Value::as_f64).unwrap_or(0.0),
                    rg.get(1).and_then(Value::as_f64).unwrap_or(0.0),
                ];
            }
            let f_lo = r.get("f_lo_hz").and_then(Value::as_f64).unwrap_or(0.0);
            let bin = r.get("bin_hz").and_then(Value::as_f64).unwrap_or(1.0);
            front_ends.insert(
                step,
                (
                    f_lo,
                    f_lo + bin * db.len() as f64,
                    r.get("antenna").and_then(Value::as_str).map(str::to_string),
                    r.get("gain_db").and_then(Value::as_f64),
                ),
            );
            let acc = accs
                .entry(step)
                .or_insert_with(|| StepAcc::new(f_lo, bin, db.len()));
            if acc.max.len() == db.len() && acc.passes < passes {
                acc.fold(&db, thr);
            }
        }
        let done = steps_total > 0
            && accs.len() == steps_total
            && accs.values().all(|a| a.passes >= passes);
        if done {
            break true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    };
    if accs.is_empty() {
        return Err(AppError::internal(
            "SWEEP_FAILED",
            "no sweep rows arrived — check `health` (radio state)",
        ));
    }
    let steps: Vec<StepAcc> = accs.into_values().collect();
    let summary = summarize(
        &steps,
        range,
        thr,
        req.max_peaks.unwrap_or(20).clamp(1, 200),
        req.profile_points.unwrap_or(64).min(512),
    );
    let front_end = front_end_segments(&front_ends);
    let mut text = render_text(&summary);
    for seg in &front_end {
        text.push_str(&format!(
            "\nfront end {:.3}–{:.3} MHz: {} · gain {}",
            seg["lo_hz"].as_f64().unwrap_or(0.0) / 1e6,
            seg["hi_hz"].as_f64().unwrap_or(0.0) / 1e6,
            seg["antenna"].as_str().unwrap_or("radio's antenna"),
            seg["gain_db"]
                .as_f64()
                .map_or_else(|| "radio's".to_string(), |g| format!("{g} dB")),
        ));
    }
    // 4. Visible in the AI panel, whoever asked.
    let id = format!("sweep-{}", crate::capture::now_ms());
    state.decoder_store().apply(
        "analysis",
        json!({
            "id": id,
            "tool": "sweep",
            "by": by,
            "params": {
                "lo_hz": summary.range_hz[0], "hi_hz": summary.range_hz[1],
                "rbw_hz": summary.rbw_hz, "passes": summary.passes,
                "threshold_db": thr,
            },
            "result": &summary,
            "text": text,
        }),
    );
    // 5. Hand the radio back if asked.
    let mut restored = false;
    if req.restore {
        state
            .return_from_stepped()
            .await
            .map_err(|e| AppError::radio_or_bad_request("SWEEP_FAILED", &e))?;
        restored = true;
    }
    let mut out = serde_json::to_value(&summary).unwrap_or_default();
    if let Some(o) = out.as_object_mut() {
        o.insert("complete".into(), json!(complete));
        o.insert("entered".into(), json!(entered));
        o.insert("restored".into(), json!(restored));
        o.insert("analysis_id".into(), json!(id));
        o.insert("front_end".into(), json!(front_end));
        o.insert("text".into(), json!(text));
    }
    Ok(Json(out))
}

/// (own_lo_hz, own_hi_hz, antenna, gain_db) of one step's row.
type StepFrontEnd = (f64, f64, Option<String>, Option<f64>);

/// Adjacent steps measured with the same antenna / gain, merged:
/// `[{lo_hz, hi_hz, antenna?, gain_db?}]` in frequency order.
fn front_end_segments(steps: &BTreeMap<u64, StepFrontEnd>) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    let mut sorted: Vec<&StepFrontEnd> = steps.values().collect();
    sorted.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    for (lo, hi, ant, gain) in sorted {
        if let Some(last) = out.last_mut() {
            if last["antenna"].as_str() == ant.as_deref() && last["gain_db"].as_f64() == *gain {
                last["hi_hz"] = json!(hi);
                continue;
            }
        }
        out.push(json!({ "lo_hz": lo, "hi_hz": hi, "antenna": ant, "gain_db": gain }));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(f_lo: f64, floor: f32, n: usize, sigs: &[(usize, f32)]) -> Vec<f32> {
        let _ = f_lo;
        let mut v = vec![floor; n];
        // a little texture so medians are realistic
        for (k, x) in v.iter_mut().enumerate() {
            *x += ((k * 7919) % 5) as f32 * 0.2;
        }
        for &(k, lvl) in sigs {
            v[k] = lvl;
            if k > 0 {
                v[k - 1] = lvl - 4.0;
            }
            if k + 1 < n {
                v[k + 1] = lvl - 4.0;
            }
        }
        v
    }

    #[test]
    fn per_step_floor_absorbs_gain_steps_and_finds_signals() {
        // Two steps whose floors differ by 15 dB (an LNA band edge): the
        // higher floor must not read as a signal; each step's carrier must.
        let mut a = StepAcc::new(100e6, 1_000.0, 1000);
        let mut b = StepAcc::new(101e6, 1_000.0, 1000);
        a.fold(&step(100e6, -100.0, 1000, &[(500, -60.0)]), 10.0);
        b.fold(&step(101e6, -85.0, 1000, &[(250, -50.0)]), 10.0);
        let s = summarize(&[a, b], [100e6, 102e6], 10.0, 10, 20);
        assert_eq!(s.peaks.len(), 2, "{:?}", s.peaks);
        let f: Vec<f64> = s.peaks.iter().map(|p| p.freq_hz).collect();
        assert!(f.iter().any(|x| (x - 100.5e6).abs() < 1_000.0));
        assert!(f.iter().any(|x| (x - 101.25e6).abs() < 1_000.0));
        assert!(s.floor_range_dbfs[1] - s.floor_range_dbfs[0] > 10.0);
        assert_eq!(s.profile.len(), 20);
    }

    #[test]
    fn bursty_signal_has_partial_duty() {
        let mut a = StepAcc::new(0.0, 1_000.0, 200);
        a.fold(&step(0.0, -100.0, 200, &[(50, -40.0), (120, -60.0)]), 10.0);
        a.fold(&step(0.0, -100.0, 200, &[(50, -40.0)]), 10.0);
        let s = summarize(&[a], [0.0, 200e3], 10.0, 10, 0);
        let steady = s
            .peaks
            .iter()
            .find(|p| (p.freq_hz - 50e3).abs() < 1e3)
            .unwrap();
        let burst = s
            .peaks
            .iter()
            .find(|p| (p.freq_hz - 120e3).abs() < 1e3)
            .unwrap();
        assert!(!steady.bursty && (steady.duty - 1.0).abs() < 1e-6);
        assert!(burst.bursty && (burst.duty - 0.5).abs() < 1e-6);
        assert!(burst.avg_db < burst.peak_db, "avg sees it half the time");
    }

    #[test]
    fn width_is_the_occupied_span_above_threshold() {
        let mut a = StepAcc::new(0.0, 1_000.0, 100);
        let mut row = vec![-100.0f32; 100];
        row[40..60].fill(-50.0); // a 20 kHz-wide flat signal
        a.fold(&row, 10.0);
        let s = summarize(&[a], [0.0, 100e3], 10.0, 5, 0);
        assert_eq!(s.peaks.len(), 1);
        assert!(
            (s.peaks[0].width_hz - 20_000.0).abs() < 1.0,
            "{:?}",
            s.peaks[0]
        );
    }

    #[test]
    fn sweep_bands_follow_the_station_unless_overridden() {
        let st: crate::station::Station = serde_json::from_value(json!({
            "bands": [
                { "lo_hz": 0.0, "hi_hz": 30e6, "antenna": "Antenna C", "gain_db": 40.0 },
                { "lo_hz": 400e6, "hi_hz": 1e9, "antenna": "Antenna A" },
                { "lo_hz": 88e6, "hi_hz": 108e6, "label": "nothing to set" }
            ]
        }))
        .unwrap();
        let b = crate::station::sweep_bands(&st, true, None, None);
        assert_eq!(
            b.as_array().unwrap().len(),
            2,
            "rows that set nothing are skipped"
        );
        assert_eq!(b[0]["antenna"], "Antenna C");
        let b = crate::station::sweep_bands(&st, true, None, Some(25.0));
        assert_eq!(b[0]["gain_db"], 25.0, "override wins on station rows");
        assert_eq!(b[0]["antenna"], "Antenna C");
        assert_eq!(
            b[2]["lo_hz"], 0.0,
            "plus a catch-all for steps outside them"
        );
        assert_eq!(
            crate::station::sweep_bands(&st, false, None, None),
            json!([])
        );
    }

    #[test]
    fn front_end_segments_merge_adjacent_equal_steps() {
        let mut m = BTreeMap::new();
        m.insert(0, (1.8e6, 9.4e6, Some("Antenna C".into()), Some(40.0)));
        m.insert(1, (9.4e6, 17e6, Some("Antenna C".into()), Some(40.0)));
        m.insert(2, (17e6, 24.6e6, None, None));
        let segs = front_end_segments(&m);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0]["hi_hz"], 17e6);
        assert!(segs[1]["antenna"].is_null());
    }
}
