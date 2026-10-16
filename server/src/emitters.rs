//! The emitter list — every transmitter the station has seen, whichever
//! tool saw it. One model behind the receiver's Strongest Signals pane
//! and the spectrum analyzer's emitter panel, and the AI's memory of the
//! band.
//!
//! Two detectors feed it, both already in the store:
//! - **Receiver** — `signals` (the `SignalList` block on the wideband FFT:
//!   `{freq_hz, bw_hz, power_db, snr_db}` per tracked signal, ~4 Hz).
//! - **Spectrum analyzer** — `panorama` rows (one per sweep step); each
//!   row's peaks over its local floor (the `sweep` summarizer) are the
//!   observations.
//!
//! A detector frame's detections are merged first: inside broadcast FM
//! by channel cell (a fine-RBW detector splits a station and its HD
//! Radio sidebands into a dozen fragments), elsewhere when within
//! [`BRIDGE_HZ`]; weak satellites fold into the wideband carrier they
//! flank (without widening it). A detection that overlaps (or is a
//! satellite of) a known emitter updates it; emitters that come to
//! overlap merge into the older one; anything else with enough SNR
//! becomes a new numbered emitter. Each carries its band (US plan),
//! level / SNR / width, first / last seen, how often and by which
//! detector it was seen, and a status — `new` until someone (the
//! operator in the UI, the AI over MCP) labels it `identified` or marks
//! it `ignored` (a local spur, a birdie). Emitters live in the store
//! kind `emitter` (keyed by id, persisted), so the station's knowledge
//! survives restarts; updates are throttled per emitter.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::decoder_store::{DecoderStore, DeltaOp};
use crate::sweep_api::{summarize, StepAcc};

pub const KIND: &str = "emitter";

/// Outside broadcast FM, detections whose spans come within this of
/// each other are one emitter. Narrow on purpose: land-mobile channels
/// sit 12.5 / 25 kHz apart and must stay separate.
pub const BRIDGE_HZ: f64 = 10_000.0;

/// US broadcast FM: channels on odd tenths of a MHz, 200 kHz apart. A
/// fine-RBW detector splits a station (and its HD Radio sidebands) into
/// a dozen fragments; inside the band, detections group by channel cell
/// instead of by proximity, and emitters in different cells never merge.
const FM_LO_HZ: f64 = 87.8e6;
const FM_HI_HZ: f64 = 108.1e6;
const FM_GRID_HZ: f64 = 200e3;
const FM_FIRST_HZ: f64 = 87.9e6;
const FM_MAX_WIDTH_HZ: f64 = 400e3;

/// The broadcast-FM channel cell (its centre, Hz) a frequency falls in.
fn fm_cell(f: f64) -> Option<i64> {
    (FM_LO_HZ..FM_HI_HZ)
        .contains(&f)
        .then(|| ((f - FM_FIRST_HZ) / FM_GRID_HZ).round() as i64)
}

/// Sweep rows whose median floor is below this come from a step where
/// the radio's front end is attenuated hard (the RSPdx gain ladder above
/// 1 GHz reads −140…−158 dBFS there); "peaks" over such a floor are
/// artefacts, not emitters.
const MIN_ROW_FLOOR_DBFS: f32 = -130.0;

/// The store kind holding [`EmitterConfig`] (persisted).
pub const CONFIG_KIND: &str = "emitter_config";

/// Operator-tunable emitter-list policy, shared by every tab, the CLI and
/// the AI (one server value, persisted in the store).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EmitterConfig {
    /// SNR cutoff, dB: a detection must clear it to *found* an emitter,
    /// and lists hide emitters whose best SNR never reached it.
    pub min_snr_db: f32,
    /// Unlabelled emitters seen only once are dropped after this long
    /// without a second sighting (ms; 0 = keep).
    pub tentative_ttl_ms: u64,
    /// Unlabelled emitters not seen for this long are forgotten (ms;
    /// 0 = never).
    pub forget_after_ms: u64,
}

impl Default for EmitterConfig {
    fn default() -> Self {
        Self {
            min_snr_db: 15.0,
            tentative_ttl_ms: 10 * 60_000,
            forget_after_ms: 7 * 24 * 3_600_000,
        }
    }
}

/// Minimum interval between store updates for one emitter (new emitters,
/// merges and label changes publish at once).
const PUBLISH_EVERY_MS: u64 = 5_000;

/// A seen emitter. Field names match the `signals` / `sweep` vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Emitter {
    /// Stable number (shown as `#id`).
    pub id: u64,
    pub freq_hz: f64,
    pub width_hz: f64,
    /// Last observed level, dBFS.
    pub level_dbfs: f32,
    /// Strongest level ever observed, dBFS.
    pub peak_dbfs: f32,
    /// Last observed level over the local floor, dB.
    pub snr_db: f32,
    /// Best SNR ever observed, dB (what the cutoff filters on).
    #[serde(default)]
    pub peak_snr_db: f32,
    /// Narrowest band-plan allocation containing it.
    #[serde(default)]
    pub band: Option<String>,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    /// Observations folded in.
    pub seen: u64,
    /// Detectors that have seen it: `signals` (receiver), `sweep`.
    #[serde(default)]
    pub sources: Vec<String>,
    /// `new` | `identified` | `ignored`.
    pub status: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    /// Who labelled it, how sure, on what evidence.
    #[serde(default)]
    pub ident: Option<Value>,
    /// The last `inspect` of it: when, the analysis id, and the headline
    /// features (hint, width, SNR, keying, FSK shift, period).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspect: Option<Value>,
}

/// One detection from either detector.
#[derive(Debug, Clone, Copy)]
pub struct Obs {
    pub freq_hz: f64,
    pub width_hz: f64,
    pub level_dbfs: f32,
    pub snr_db: f32,
    pub source: &'static str,
}

/// Store change the tracker asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Put(Box<Emitter>),
    Drop(u64),
}

fn span(f: f64, w: f64) -> (f64, f64) {
    (f - w / 2.0, f + w / 2.0)
}

fn band_for(hz: f64) -> Option<String> {
    crate::band_plan::at(hz)
        .into_iter()
        .filter(|b| b.name != "Not Allocated")
        .min_by(|a, b| (a.end - a.start).total_cmp(&(b.end - b.start)))
        .map(|b| {
            // The plan names each 25 kHz marine VHF channel by its bare
            // number ("18"); say what it is.
            if b.name.chars().all(|c| c.is_ascii_digit()) {
                if (156e6..162.05e6).contains(&hz) {
                    format!("Marine VHF Ch {}", b.name)
                } else {
                    format!("Ch {}", b.name)
                }
            } else {
                b.name
            }
        })
}

/// Wideband carriers (broadcast FM with HD Radio, TV) have weaker
/// satellites — the digital sidebands — a little beyond their edge. A
/// detection at least [`SATELLITE_DB`] weaker than an emitter at least
/// [`SATELLITE_MIN_WIDTH_HZ`] wide, within [`SATELLITE_REACH`] × its
/// width beyond its edge, is part of it. Narrowband neighbours (land
/// mobile 12.5 / 25 kHz apart) are never folded.
const SATELLITE_DB: f32 = 10.0;
const SATELLITE_MIN_WIDTH_HZ: f64 = 100_000.0;
const SATELLITE_REACH: f64 = 0.75;

/// Is a detection at `f` (level `lvl`) a satellite of a carrier at `cf`
/// (width `cw`, level `clvl`)?
fn is_satellite(f: f64, lvl: f32, cf: f64, cw: f64, clvl: f32) -> bool {
    cw >= SATELLITE_MIN_WIDTH_HZ
        && clvl - lvl >= SATELLITE_DB
        && (f - cf).abs() <= cw / 2.0 + SATELLITE_REACH * cw
}

/// Merge one detector frame's detections that are within [`BRIDGE_HZ`]
/// of each other (union span; the strongest sets level and frequency),
/// then fold weak satellites into the wideband carriers they flank.
#[must_use]
pub fn merge_frame(obs: Vec<Obs>) -> Vec<Obs> {
    let mut merged = bridge_frame(obs);
    merged.sort_by(|a, b| b.level_dbfs.total_cmp(&a.level_dbfs));
    let mut out: Vec<Obs> = Vec::new();
    for o in merged {
        // A satellite joins its carrier without widening it.
        if out
            .iter()
            .any(|c| is_satellite(o.freq_hz, o.level_dbfs, c.freq_hz, c.width_hz, c.level_dbfs))
        {
            continue;
        }
        out.push(o);
    }
    out
}

/// Union detections in the same broadcast-FM cell, or (elsewhere) whose
/// spans are within [`BRIDGE_HZ`].
fn bridge_frame(mut obs: Vec<Obs>) -> Vec<Obs> {
    obs.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz));
    let mut out: Vec<(Obs, f64, f64)> = Vec::new();
    for o in obs {
        let (lo, hi) = span(o.freq_hz, o.width_hz);
        if let Some((m, mlo, mhi)) = out.last_mut() {
            let same = match (fm_cell(m.freq_hz), fm_cell(o.freq_hz)) {
                (Some(a), Some(b)) => a == b,
                (None, None) => lo - *mhi <= BRIDGE_HZ,
                _ => false,
            };
            if same {
                *mlo = mlo.min(lo);
                *mhi = mhi.max(hi);
                if o.level_dbfs > m.level_dbfs {
                    m.freq_hz = o.freq_hz;
                    m.level_dbfs = o.level_dbfs;
                }
                m.snr_db = m.snr_db.max(o.snr_db);
                m.width_hz = *mhi - *mlo;
                continue;
            }
        }
        out.push((o, lo, hi));
    }
    out.into_iter()
        .map(|(mut o, _, _)| {
            if fm_cell(o.freq_hz).is_some() {
                // A station plus its HD sidebands is ≤ ~400 kHz; a wider
                // "detection" in the dense FM band is the detector merging
                // neighbours, not one emitter.
                o.width_hz = o.width_hz.min(FM_MAX_WIDTH_HZ);
            }
            o
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct Tracker {
    emitters: BTreeMap<u64, Emitter>,
    next_id: u64,
    last_pub: HashMap<u64, u64>,
    pub config: EmitterConfig,
}

impl Tracker {
    /// Seed from the persisted store (ids continue past the restored ones).
    pub fn reseed(&mut self, store: &DecoderStore) {
        if let Some(k) = store.snapshot_kind(KIND) {
            for r in k.current.into_values() {
                if let Ok(mut e) = serde_json::from_value::<Emitter>(r.data) {
                    if e.peak_snr_db == 0.0 {
                        e.peak_snr_db = e.snr_db; // records from before the field
                    }
                    self.next_id = self.next_id.max(e.id + 1);
                    self.emitters.insert(e.id, e);
                }
            }
        }
        if let Some(c) = store
            .snapshot_kind(CONFIG_KIND)
            .and_then(|k| k.current.into_values().next())
            .and_then(|r| serde_json::from_value::<EmitterConfig>(r.data).ok())
        {
            self.config = c;
        }
        self.next_id = self.next_id.max(1);
    }

    /// Drop unlabelled emitters the policy no longer keeps: seen once and
    /// not again within `tentative_ttl_ms`, or not seen for
    /// `forget_after_ms`. With `below_cutoff`, also those whose best SNR
    /// never reached the cutoff (the explicit "prune" action). Labelled
    /// and ignored emitters are station knowledge and always stay.
    pub fn prune(&mut self, now_ms: u64, below_cutoff: bool) -> Vec<Change> {
        let c = self.config;
        let doomed: Vec<u64> = self
            .emitters
            .values()
            .filter(|e| e.label.is_none() && e.status == "new")
            .filter(|e| {
                let idle = now_ms.saturating_sub(e.last_seen_ms);
                (c.tentative_ttl_ms > 0 && e.seen < 2 && idle >= c.tentative_ttl_ms)
                    || (c.forget_after_ms > 0 && idle >= c.forget_after_ms)
                    || (below_cutoff && e.peak_snr_db < c.min_snr_db)
            })
            .map(|e| e.id)
            .collect();
        for id in &doomed {
            self.emitters.remove(id);
            self.last_pub.remove(id);
        }
        doomed.into_iter().map(Change::Drop).collect()
    }

    /// Forget everything (the store kind was reset). Numbering carries on.
    pub fn clear(&mut self) {
        self.emitters.clear();
        self.last_pub.clear();
    }

    /// Known emitter whose span overlaps the detection's (with a small
    /// margin), or that the detection is a weak satellite of; nearest
    /// centre first.
    fn find(&self, o: &Obs) -> Option<u64> {
        let (lo, hi) = span(o.freq_hz, o.width_hz);
        self.emitters
            .values()
            .filter(|e| {
                let (elo, ehi) = span(e.freq_hz, e.width_hz);
                let near = match (fm_cell(e.freq_hz), fm_cell(o.freq_hz)) {
                    (Some(a), Some(b)) => a == b,
                    (None, None) => lo - ehi <= 5_000.0 && elo - hi <= 5_000.0,
                    _ => false,
                };
                near || is_satellite(o.freq_hz, o.level_dbfs, e.freq_hz, e.width_hz, e.peak_dbfs)
            })
            .min_by(|a, b| {
                (a.freq_hz - o.freq_hz)
                    .abs()
                    .total_cmp(&(b.freq_hz - o.freq_hz).abs())
            })
            .map(|e| e.id)
    }

    /// Fold one detector frame (merged first — see [`merge_frame`]).
    pub fn observe_frame(&mut self, obs: Vec<Obs>, now_ms: u64) -> Vec<Change> {
        merge_frame(obs)
            .into_iter()
            .flat_map(|o| self.observe(o, now_ms))
            .collect()
    }

    /// Fold one observation; returns the store changes that are due.
    pub fn observe(&mut self, o: Obs, now_ms: u64) -> Vec<Change> {
        let Some(id) = self.find(&o) else {
            if o.snr_db < self.config.min_snr_db {
                return Vec::new();
            }
            let id = self.next_id;
            self.next_id += 1;
            let e = Emitter {
                id,
                freq_hz: o.freq_hz,
                width_hz: o.width_hz,
                level_dbfs: o.level_dbfs,
                peak_dbfs: o.level_dbfs,
                snr_db: o.snr_db,
                peak_snr_db: o.snr_db,
                band: band_for(o.freq_hz),
                first_seen_ms: now_ms,
                last_seen_ms: now_ms,
                seen: 1,
                sources: vec![o.source.to_string()],
                status: "new".into(),
                label: None,
                notes: None,
                ident: None,
                inspect: None,
            };
            self.emitters.insert(id, e.clone());
            self.last_pub.insert(id, now_ms);
            return vec![Change::Put(Box::new(e))];
        };
        let mut news = false;
        if let Some(e) = self.emitters.get_mut(&id) {
            // Frequency follows the strongest sighting (a modulated
            // signal's peak wanders; the strong one is the best estimate);
            // the span grows to cover what's been seen of it.
            if o.level_dbfs >= e.peak_dbfs {
                e.freq_hz = o.freq_hz;
            }
            // Smoothed, never a union: a union only ever grows, and one
            // wide detection would turn the emitter into a black hole.
            e.width_hz = 0.8 * e.width_hz + 0.2 * o.width_hz;
            e.level_dbfs = o.level_dbfs;
            e.peak_dbfs = e.peak_dbfs.max(o.level_dbfs);
            e.snr_db = o.snr_db;
            e.peak_snr_db = e.peak_snr_db.max(o.snr_db);
            e.last_seen_ms = now_ms;
            e.seen += 1;
            if !e.sources.iter().any(|s| s == o.source) {
                e.sources.push(o.source.to_string());
                news = true; // a new detector is news
            }
        }
        let (mut out, survivor) = self.absorb_overlaps(id);
        let due = news
            || !out.is_empty()
            || self
                .last_pub
                .get(&survivor)
                .is_none_or(|&t| now_ms.saturating_sub(t) >= PUBLISH_EVERY_MS);
        if due {
            if let Some(e) = self.emitters.get(&survivor) {
                self.last_pub.insert(survivor, now_ms);
                out.push(Change::Put(Box::new(e.clone())));
            }
        }
        out
    }

    /// Merge every emitter whose span overlaps `id`'s into the older of
    /// the two (labels / status survive: identified > ignored > new).
    /// Returns the drops and the surviving id.
    fn absorb_overlaps(&mut self, mut id: u64) -> (Vec<Change>, u64) {
        let mut out = Vec::new();
        loop {
            let Some(me) = self.emitters.get(&id) else {
                return (out, id);
            };
            let (lo, hi) = span(me.freq_hz, me.width_hz);
            let Some(other) = self
                .emitters
                .values()
                .find(|e| {
                    let (elo, ehi) = span(e.freq_hz, e.width_hz);
                    e.id != id
                        && match (fm_cell(e.freq_hz), fm_cell(me.freq_hz)) {
                            (Some(a), Some(b)) => a == b,
                            (None, None) => lo <= ehi && elo <= hi,
                            _ => false,
                        }
                })
                .map(|e| e.id)
            else {
                return (out, id);
            };
            let (keep, gone) = (id.min(other), id.max(other));
            let g = self.emitters.remove(&gone).expect("present");
            let k = self.emitters.get_mut(&keep).expect("present");
            merge_into(k, g);
            self.last_pub.remove(&gone);
            out.push(Change::Drop(gone));
            id = keep;
        }
    }

    /// Operator / AI edit: label, status, notes, ident evidence.
    pub fn edit(
        &mut self,
        id: u64,
        patch: &EmitterPatch,
        by: &str,
        now_ms: u64,
    ) -> Option<Emitter> {
        let e = self.emitters.get_mut(&id)?;
        if let Some(l) = &patch.label {
            let l = l.trim();
            e.label = (!l.is_empty()).then(|| l.to_string());
            if patch.status.is_none() {
                e.status = if e.label.is_some() {
                    "identified"
                } else {
                    "new"
                }
                .into();
            }
        }
        if let Some(s) = &patch.status {
            e.status.clone_from(s);
        }
        if let Some(n) = &patch.notes {
            e.notes = (!n.trim().is_empty()).then(|| n.clone());
        }
        if patch.label.is_some() || patch.confidence.is_some() || patch.evidence.is_some() {
            e.ident = Some(json!({
                "by": by,
                "at_ms": now_ms,
                "confidence": patch.confidence,
                "evidence": patch.evidence,
            }));
        }
        self.last_pub.insert(id, now_ms);
        Some(e.clone())
    }

    #[must_use]
    pub fn get(&self, id: u64) -> Option<&Emitter> {
        self.emitters.get(&id)
    }

    /// The emitter a frequency falls in (span + 5 kHz), nearest centre
    /// first; ignored ones too.
    #[must_use]
    pub fn at(&self, freq_hz: f64) -> Option<&Emitter> {
        self.emitters
            .values()
            .filter(|e| (freq_hz - e.freq_hz).abs() <= e.width_hz / 2.0 + 5_000.0)
            .min_by(|a, b| {
                (a.freq_hz - freq_hz)
                    .abs()
                    .total_cmp(&(b.freq_hz - freq_hz).abs())
            })
    }

    /// Record an `inspect` on emitter `id`; the updated emitter to publish.
    pub fn attach_inspect(&mut self, id: u64, summary: Value) -> Option<Emitter> {
        let e = self.emitters.get_mut(&id)?;
        e.inspect = Some(summary);
        Some(e.clone())
    }

    /// Emitters, filtered: `status` (`new` / `identified` / `ignored`;
    /// `all`; default everything but ignored), `[lo, hi]` range, seen
    /// since `since_ms`.
    #[must_use]
    pub fn list(&self, q: &EmitterQuery) -> Vec<Emitter> {
        let cutoff = q.min_snr_db.unwrap_or(self.config.min_snr_db);
        let mut v: Vec<Emitter> = self
            .emitters
            .values()
            // Labelled emitters always show; the rest must have cleared
            // the cutoff at least once.
            .filter(|e| e.label.is_some() || e.peak_snr_db >= cutoff)
            .filter(|e| q.tentative || e.label.is_some() || e.seen >= 2)
            .filter(|e| match q.status.as_deref() {
                None => e.status != "ignored",
                Some("all") => true,
                Some(s) => e.status == s,
            })
            .filter(|e| q.lo_hz.is_none_or(|lo| e.freq_hz >= lo))
            .filter(|e| q.hi_hz.is_none_or(|hi| e.freq_hz <= hi))
            .filter(|e| q.since_ms.is_none_or(|t| e.last_seen_ms >= t))
            .cloned()
            .collect();
        match q.sort.as_deref() {
            Some("freq") => v.sort_by(|a, b| a.freq_hz.total_cmp(&b.freq_hz)),
            Some("recent") => v.sort_by_key(|e| std::cmp::Reverse(e.last_seen_ms)),
            Some("new") => v.sort_by_key(|e| std::cmp::Reverse(e.first_seen_ms)),
            _ => v.sort_by(|a, b| b.peak_dbfs.total_cmp(&a.peak_dbfs)),
        }
        v.truncate(q.limit.unwrap_or(200).clamp(1, 5_000));
        v
    }
}

/// Fold `g` into `k` (the older emitter keeps its number).
fn merge_into(k: &mut Emitter, g: Emitter) {
    let rank = |s: &str| match s {
        "identified" => 2,
        "ignored" => 1,
        _ => 0,
    };
    if g.peak_dbfs > k.peak_dbfs {
        k.freq_hz = g.freq_hz;
    }
    k.width_hz = k.width_hz.max(g.width_hz);
    k.peak_dbfs = k.peak_dbfs.max(g.peak_dbfs);
    k.peak_snr_db = k.peak_snr_db.max(g.peak_snr_db);
    k.first_seen_ms = k.first_seen_ms.min(g.first_seen_ms);
    if g.last_seen_ms > k.last_seen_ms {
        k.last_seen_ms = g.last_seen_ms;
        k.level_dbfs = g.level_dbfs;
        k.snr_db = g.snr_db;
    }
    k.seen += g.seen;
    for s in g.sources {
        if !k.sources.contains(&s) {
            k.sources.push(s);
        }
    }
    if rank(&g.status) > rank(&k.status) {
        k.status = g.status;
        k.label = k.label.take().or(g.label);
        k.ident = k.ident.take().or(g.ident);
    } else if k.label.is_none() {
        k.label = g.label;
    }
    if k.notes.is_none() {
        k.notes = g.notes;
    }
}

/// `GET /api/emitters` query.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EmitterQuery {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub lo_hz: Option<f64>,
    #[serde(default)]
    pub hi_hz: Option<f64>,
    #[serde(default)]
    pub since_ms: Option<u64>,
    /// `level` (default, strongest first) | `freq` | `recent` | `new`.
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    /// Hide unlabelled emitters whose best SNR is below this (default:
    /// the configured cutoff; `0` shows everything).
    #[serde(default)]
    pub min_snr_db: Option<f32>,
    /// Include tentative emitters (seen once, unlabelled — mostly noise
    /// peaks; they expire after `tentative_ttl_ms` unless seen again).
    #[serde(default)]
    pub tentative: bool,
}

/// `POST /api/emitters/:id` body.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EmitterPatch {
    #[serde(default)]
    pub label: Option<String>,
    /// `new` | `identified` | `ignored`.
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub confidence: Option<f32>,
    #[serde(default)]
    pub evidence: Option<Value>,
}

pub type Shared = Arc<Mutex<Tracker>>;

/// The daemon's one tracker (module-level so `app_state` stays free of
/// it — integration tests include that file by path).
pub fn tracker() -> &'static Shared {
    static T: std::sync::OnceLock<Shared> = std::sync::OnceLock::new();
    T.get_or_init(|| Arc::new(Mutex::new(Tracker::default())))
}

/// `GET /api/emitters` — the emitter list (see `crate::emitters`):
/// `?status=new|identified|ignored|all&lo_hz=&hi_hz=&since_ms=&sort=level|freq|recent|new&limit=`.
/// Ignored emitters are hidden unless asked for.
pub async fn get_emitters(
    axum::extract::Query(q): axum::extract::Query<EmitterQuery>,
) -> axum::Json<serde_json::Value> {
    let t = tracker()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let list = t.list(&q);
    axum::Json(serde_json::json!({ "count": list.len(), "emitters": list }))
}

/// `POST /api/emitters/:id` — label / status / notes / identification
/// evidence. Whoever edits (UI, CLI, an agent over MCP) is recorded.
pub async fn post_emitter(
    axum::extract::State(state): axum::extract::State<crate::app_state::AppState>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id): axum::extract::Path<u64>,
    axum::Json(patch): axum::Json<EmitterPatch>,
) -> Result<axum::Json<serde_json::Value>, crate::error::AppError> {
    if let Some(s) = patch.status.as_deref() {
        if !matches!(s, "new" | "identified" | "ignored") {
            return Err(crate::error::AppError::bad_request(
                "EMITTER_REJECTED",
                "status must be new | identified | ignored",
            ));
        }
    }
    let by = headers
        .get("x-ferrite-via")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("ui")
        .to_string();
    let e = tracker()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .edit(id, &patch, &by, crate::capture::now_ms() as u64)
        .ok_or_else(|| {
            crate::error::AppError::not_found("EMITTER_UNKNOWN", format!("no emitter #{id}"))
        })?;
    let v = serde_json::to_value(&e).unwrap_or_default();
    state.decoder_store().apply(KIND, v.clone());
    Ok(axum::Json(v))
}

/// Observations in one `signals` snapshot.
fn from_signals(data: &Value) -> Vec<Obs> {
    data.get("signals")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| {
                    Some(Obs {
                        freq_hz: s.get("freq_hz")?.as_f64()?,
                        width_hz: s.get("bw_hz").and_then(Value::as_f64).unwrap_or(0.0),
                        level_dbfs: s.get("power_db")?.as_f64()? as f32,
                        snr_db: s.get("snr_db").and_then(Value::as_f64).unwrap_or(0.0) as f32,
                        source: "signals",
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Observations in one `panorama` row: its peaks over the local floor.
fn from_row(data: &Value) -> Vec<Obs> {
    let (Some(power), Some(f_lo), Some(bin)) = (
        data.get("power").and_then(Value::as_str),
        data.get("f_lo_hz").and_then(Value::as_f64),
        data.get("bin_hz").and_then(Value::as_f64),
    ) else {
        return Vec::new();
    };
    let Ok(db) = ferrite_blocks::sweep::decode_power(power) else {
        return Vec::new();
    };
    let mut finite: Vec<f32> = db.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.is_empty() {
        return Vec::new();
    }
    finite.sort_by(f32::total_cmp);
    if finite[finite.len() / 2] < MIN_ROW_FLOOR_DBFS {
        return Vec::new();
    }
    let mut acc = StepAcc::new(f_lo, bin, db.len());
    acc.fold(&db, 10.0);
    let hi = f_lo + db.len() as f64 * bin;
    summarize(&[acc], [f_lo, hi], 10.0, 64, 0)
        .peaks
        .into_iter()
        .map(|p| Obs {
            freq_hz: p.freq_hz,
            width_hz: p.width_hz,
            level_dbfs: p.level_dbfs,
            snr_db: p.peak_db,
            source: "sweep",
        })
        .collect()
}

/// Write tracker changes to the store (puts and merged-away drops).
pub fn apply_changes(store: &DecoderStore, changes: Vec<Change>) {
    for c in changes {
        match c {
            Change::Put(e) => {
                if let Ok(v) = serde_json::to_value(&*e) {
                    store.apply(KIND, v);
                }
            }
            Change::Drop(id) => store.remove(KIND, &id.to_string()),
        }
    }
}

/// Run the tracker off the store's delta stream until the store drops;
/// prune per the policy once a minute.
pub fn spawn(store: Arc<DecoderStore>, tracker: Shared) {
    let mut rx = store.subscribe();
    tokio::spawn(async move {
        // `signals` arrives at ~4 Hz; one fold a second is plenty.
        let mut last_signals = 0u64;
        let mut prune_tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            let delta = tokio::select! {
                d = rx.recv() => match d {
                    Ok(d) => d,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                },
                _ = prune_tick.tick() => {
                    let now = crate::capture::now_ms() as u64;
                    let changes = tracker
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .prune(now, false);
                    apply_changes(&store, changes);
                    continue;
                }
            };
            if delta.kind == KIND && matches!(delta.op, DeltaOp::Reset) {
                tracker
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clear();
                continue;
            }
            let DeltaOp::Upsert { record } = &delta.op else {
                continue;
            };
            let now = record.at_ms;
            let obs = match delta.kind.as_str() {
                "signals" if now.saturating_sub(last_signals) >= 1_000 => {
                    last_signals = now;
                    from_signals(&record.data)
                }
                "panorama" => from_row(&record.data),
                _ => continue,
            };
            let changes = tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .observe_frame(obs, now);
            apply_changes(&store, changes);
        }
    });
}

/// `GET /api/emitters/config` — the emitter-list policy.
pub async fn get_config() -> axum::Json<EmitterConfig> {
    axum::Json(
        tracker()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .config,
    )
}

/// `POST /api/emitters/config {min_snr_db?, tentative_ttl_ms?,
/// forget_after_ms?}` — change it (merge); persisted in the store and
/// mirrored to every tab.
pub async fn post_config(
    axum::extract::State(state): axum::extract::State<crate::app_state::AppState>,
    axum::Json(patch): axum::Json<serde_json::Map<String, Value>>,
) -> Result<axum::Json<EmitterConfig>, crate::error::AppError> {
    let next = {
        let mut t = tracker()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut v = serde_json::to_value(t.config).unwrap_or_default();
        if let Some(m) = v.as_object_mut() {
            for (k, val) in patch {
                m.insert(k, val);
            }
        }
        let next: EmitterConfig = serde_json::from_value(v).map_err(|e| {
            crate::error::AppError::bad_request("EMITTER_CONFIG_REJECTED", format!("{e}"))
        })?;
        if !(0.0..=80.0).contains(&next.min_snr_db) {
            return Err(crate::error::AppError::bad_request(
                "EMITTER_CONFIG_REJECTED",
                "min_snr_db must be 0–80",
            ));
        }
        t.config = next;
        next
    };
    state
        .decoder_store()
        .apply(CONFIG_KIND, serde_json::to_value(next).unwrap_or_default());
    Ok(axum::Json(next))
}

/// `POST /api/emitters/prune` — drop unlabelled emitters that never
/// cleared the cutoff, were seen only once and not since the tentative
/// window, or have been forgotten. Labelled / ignored ones stay.
pub async fn post_prune(
    axum::extract::State(state): axum::extract::State<crate::app_state::AppState>,
) -> axum::Json<Value> {
    let now = crate::capture::now_ms() as u64;
    let (changes, left) = {
        let mut t = tracker()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let c = t.prune(now, true);
        (c, t.emitters.len())
    };
    let removed = changes.len();
    apply_changes(&state.decoder_store(), changes);
    axum::Json(json!({ "removed": removed, "remaining": left }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(f: f64, w: f64, lvl: f32, src: &'static str) -> Obs {
        Obs {
            freq_hz: f,
            width_hz: w,
            level_dbfs: lvl,
            snr_db: 20.0,
            source: src,
        }
    }

    fn fresh() -> Tracker {
        let mut t = Tracker::default();
        t.reseed(&DecoderStore::new());
        t
    }

    /// The emitter a change list puts (last `Put`).
    fn put(c: Vec<Change>) -> Emitter {
        c.into_iter()
            .rev()
            .find_map(|c| match c {
                Change::Put(e) => Some(*e),
                Change::Drop(_) => None,
            })
            .expect("a Put")
    }

    #[test]
    fn both_detectors_land_on_one_emitter() {
        let mut t = fresh();
        let a = put(t.observe(obs(98.5e6, 150e3, -40.0, "sweep"), 1_000));
        // The receiver's SignalList sees the same station 20 kHz off with
        // its own (narrower) width estimate.
        let b = put(t.observe(obs(98.52e6, 100e3, -35.0, "signals"), 1_500));
        assert_eq!(a.id, b.id, "same emitter");
        assert_eq!(b.sources, vec!["sweep", "signals"]);
        assert!(
            (b.freq_hz - 98.52e6).abs() < 1.0,
            "follows the strongest sighting"
        );
        assert!(b.band.is_some());
        // A different station 400 kHz away is a different emitter.
        let c = put(t.observe(obs(98.9e6, 150e3, -50.0, "sweep"), 2_000));
        assert_ne!(c.id, a.id);
    }

    #[test]
    fn a_fragmented_fm_station_is_one_emitter() {
        // SignalList at ~146 Hz bins: one broadcast station → a dozen
        // narrow detections 5–40 kHz apart, plus HD Radio sidebands.
        let mut t = fresh();
        let mut frame: Vec<Obs> = [-95e3, -60e3, -30e3, -12e3, 0.0, 9e3, 21e3, 45e3, 70e3, 95e3]
            .iter()
            .map(|d| {
                obs(
                    99.1e6 + d,
                    5e3,
                    if *d == 0.0 { -40.0 } else { -60.0 },
                    "signals",
                )
            })
            .collect();
        // HD Radio sidebands: ~70 kHz wide at ±129–198 kHz, 20 dB down.
        frame.push(obs(99.1e6 - 163e3, 70e3, -62.0, "signals"));
        frame.push(obs(99.1e6 + 163e3, 70e3, -62.0, "signals"));
        let ch = t.observe_frame(frame, 0);
        let ids: std::collections::BTreeSet<u64> = ch
            .iter()
            .filter_map(|c| match c {
                Change::Put(e) => Some(e.id),
                Change::Drop(_) => None,
            })
            .collect();
        assert_eq!(ids.len(), 1, "{ch:?}");
        let e = put(ch);
        assert!(
            (e.freq_hz - 99.1e6).abs() < 1.0,
            "the carrier is the frequency"
        );
        assert!(
            (150e3..=400e3).contains(&e.width_hz),
            "core width, not the sidebands' union: {}",
            e.width_hz
        );
    }

    #[test]
    fn overlapping_emitters_merge_into_the_older_keeping_its_label() {
        let mut t = fresh();
        let a = put(t.observe(obs(150.0e6, 10e3, -40.0, "sweep"), 0));
        let b = put(t.observe(obs(150.1e6, 10e3, -50.0, "sweep"), 0));
        let p = EmitterPatch {
            label: Some("pager".into()),
            ..Default::default()
        };
        t.edit(b.id, &p, "ui", 1);
        // A wide emitter keeps being seen between them; once its smoothed
        // span reaches both, they merge into the oldest.
        let mut ch = Vec::new();
        for i in 0..40 {
            ch = t.observe(obs(150.05e6, 200e3, -45.0, "signals"), 2 + i);
            if ch.iter().any(|c| matches!(c, Change::Drop(_))) {
                break;
            }
        }
        assert!(ch.contains(&Change::Drop(b.id)), "{ch:?}");
        let k = put(ch);
        assert_eq!(k.id, a.id);
        assert_eq!(k.label.as_deref(), Some("pager"));
        assert_eq!(k.status, "identified");
        assert!(t.get(b.id).is_none());
    }

    #[test]
    fn a_weak_narrowband_neighbour_stays_separate() {
        // Satellite folding is for wideband carriers only: two land-mobile
        // channels 25 kHz apart, one 20 dB down, are two emitters.
        let mut t = fresh();
        let ch = t.observe_frame(
            vec![
                obs(462.5625e6, 11e3, -40.0, "signals"),
                obs(462.5875e6, 11e3, -60.0, "signals"),
            ],
            0,
        );
        let n = ch.iter().filter(|c| matches!(c, Change::Put(_))).count();
        assert_eq!(n, 2, "{ch:?}");
    }

    #[test]
    fn one_wide_detection_does_not_swallow_the_band() {
        // Regression: a 2.4 MHz-wide SignalList detection in the busy FM
        // band became a 2.5 MHz emitter that absorbed every station.
        let mut t = fresh();
        let wide = put(t.observe_frame(vec![obs(100.3e6, 2.4e6, -40.0, "signals")], 0));
        assert!(wide.width_hz <= 400e3, "capped: {}", wide.width_hz);
        let other = put(t.observe_frame(vec![obs(98.5e6, 150e3, -45.0, "signals")], 1));
        assert_ne!(other.id, wide.id);
        // Repeated observations don't grow it either.
        for i in 0..50 {
            t.observe_frame(vec![obs(100.3e6, 2.4e6, -40.0, "signals")], 2 + i);
        }
        assert!(t.get(wide.id).unwrap().width_hz <= 400e3);
    }

    #[test]
    fn marine_channels_get_a_readable_band() {
        assert_eq!(band_for(161.5054e6).as_deref(), Some("Marine VHF Ch 18"));
        assert_eq!(band_for(162.425e6).as_deref(), Some("NOAA Radio"));
    }

    #[test]
    fn prune_drops_tentative_and_below_cutoff_but_keeps_knowledge() {
        let mut t = fresh();
        let once = put(t.observe(obs(300e6, 10e3, -60.0, "sweep"), 0));
        let twice = put(t.observe(obs(400e6, 10e3, -60.0, "sweep"), 0));
        t.observe(obs(400e6, 10e3, -60.0, "sweep"), 1_000);
        let mut weak = obs(500e6, 10e3, -80.0, "sweep");
        weak.snr_db = 16.0;
        let weak = put(t.observe(weak, 0));
        let labelled = put(t.observe(obs(600e6, 10e3, -60.0, "sweep"), 0));
        t.edit(
            labelled.id,
            &EmitterPatch {
                label: Some("x".into()),
                ..Default::default()
            },
            "ui",
            1,
        );
        // Within the tentative window nothing goes.
        assert!(t.prune(60_000, false).is_empty());
        // After it, the seen-once emitters go (labelled stays).
        let gone = t.prune(11 * 60_000, false);
        assert!(gone.contains(&Change::Drop(once.id)) && gone.contains(&Change::Drop(weak.id)));
        assert!(t.get(twice.id).is_some() && t.get(labelled.id).is_some());
        // Raising the cutoff + an explicit prune drops below-cutoff ones.
        t.config.min_snr_db = 25.0;
        assert!(t.prune(11 * 60_000, true).contains(&Change::Drop(twice.id)));
        assert!(t.get(labelled.id).is_some());
    }

    #[test]
    fn cutoff_gates_founding_and_listing() {
        let mut t = fresh();
        t.config.min_snr_db = 20.0;
        let mut o = obs(200e6, 10e3, -70.0, "sweep");
        o.snr_db = 18.0;
        assert!(t.observe(o, 0).is_empty(), "below cutoff founds nothing");
        o.snr_db = 22.0;
        put(t.observe(o, 0));
        let q = EmitterQuery::default();
        assert!(t.list(&q).is_empty(), "seen once = tentative, hidden");
        t.observe(o, 1_000);
        assert_eq!(t.list(&q).len(), 1);
        t.config.min_snr_db = 30.0;
        assert!(t.list(&q).is_empty(), "list hides below the cutoff");
        let all = EmitterQuery {
            min_snr_db: Some(0.0),
            ..Default::default()
        };
        assert_eq!(t.list(&all).len(), 1);
    }

    #[test]
    fn rows_from_a_collapsed_floor_yield_nothing() {
        let mut db = vec![-145.0f32; 400];
        db[200] = -113.0; // the L-band "comb" artefact
        let row = json!({
            "power": ferrite_blocks::sweep::encode_power(&db),
            "f_lo_hz": 1380e6, "bin_hz": 2500.0,
        });
        assert!(from_row(&row).is_empty());
    }

    #[test]
    fn weak_detections_do_not_found_emitters() {
        let mut t = fresh();
        let mut o = obs(300e6, 5e3, -95.0, "signals");
        o.snr_db = 4.0;
        assert!(t.observe(o, 0).is_empty());
    }

    #[test]
    fn updates_are_throttled_but_new_ones_publish() {
        let mut t = fresh();
        assert!(!t.observe(obs(162.55e6, 12e3, -50.0, "sweep"), 0).is_empty());
        assert!(t
            .observe(obs(162.55e6, 12e3, -50.0, "sweep"), 1_000)
            .is_empty());
        assert!(!t
            .observe(obs(162.55e6, 12e3, -50.0, "sweep"), 6_000)
            .is_empty());
    }

    #[test]
    fn labelling_identifies_and_ignoring_sticks() {
        let mut t = fresh();
        let id = put(t.observe(obs(14.048e6, 1e3, -60.0, "signals"), 0)).id;
        let p = EmitterPatch {
            label: Some("local spur".into()),
            ..Default::default()
        };
        assert_eq!(t.edit(id, &p, "mcp", 5).unwrap().status, "identified");
        let p = EmitterPatch {
            status: Some("ignored".into()),
            ..Default::default()
        };
        let e = t.edit(id, &p, "ui", 6).unwrap();
        assert_eq!(e.status, "ignored");
        assert_eq!(e.label.as_deref(), Some("local spur"));
    }

    #[test]
    fn reseed_continues_numbering_from_the_store() {
        let store = DecoderStore::new();
        let mut t = Tracker::default();
        t.reseed(&store);
        let e = put(t.observe(obs(100e6, 10e3, -50.0, "sweep"), 0));
        store.apply(KIND, serde_json::to_value(&e).unwrap());
        let mut t2 = Tracker::default();
        t2.reseed(&store);
        let e2 = put(t2.observe(obs(200e6, 10e3, -50.0, "sweep"), 0));
        assert_eq!(e2.id, e.id + 1);
        assert!(t2.get(e.id).is_some());
    }

    #[test]
    fn panorama_rows_yield_their_peaks() {
        let mut db = vec![-90.0f32; 400];
        db[200] = -40.0;
        db[199] = -45.0;
        db[201] = -45.0;
        let row = json!({
            "power": ferrite_blocks::sweep::encode_power(&db),
            "f_lo_hz": 100e6, "bin_hz": 2500.0,
        });
        let o = from_row(&row);
        assert_eq!(o.len(), 1);
        assert!((o[0].freq_hz - 100.5e6).abs() < 2_500.0);
    }

    #[test]
    fn store_remove_drops_the_record() {
        let store = DecoderStore::new();
        store.apply(KIND, json!({"id": 7}));
        assert!(store.snapshot_kind(KIND).unwrap().current.contains_key("7"));
        store.remove(KIND, "7");
        assert!(!store.snapshot_kind(KIND).unwrap().current.contains_key("7"));
    }
}
