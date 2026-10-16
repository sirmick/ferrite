//! Server-side decoder store — the single home for structured decoder
//! output (APRS, ADS-B, FT8, transcribe, signals, …).
//!
//! Replaces the two old disconnected paths (the flat tracing text ring +
//! per-decoder browser WS stores). Blocks emit records here via the
//! [`DecoderSink`] trait (the node-side `EventStore` block holds an
//! `Arc<DecoderStore>`); the store folds them per a per-kind [`Policy`]
//! and broadcasts a [`StateDelta`] for the WS mirror (P2). REST serves a
//! snapshot for MCP/headless.
//!
//! POJO + serde throughout so a later disk-backed persistence is a drop-in.
//! The block owns the record *schema* (`data` is its JSON verbatim); the
//! store owns the *fold policy*.

/// Speaker attribution for `dsd` / `transcribe` records (see the module).
/// A `#[path]` child so the integration tests that `#[path]`-include this
/// file get it too.
#[path = "speakers.rs"]
pub mod speakers;

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use ferrite_blocks::DecoderSink;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

/// How the store folds successive records of a kind.
#[derive(Debug, Clone, Copy)]
pub enum Policy {
    /// Bounded time-ordered log (FT8/WSPR/pager/transcribe/…).
    Append { cap: usize },
    /// Keyed table with TTL — latest record per entity (ADS-B/AIS/APRS).
    Upsert {
        key_field: &'static str,
        /// Read by `expire` (wired into the diag-tick).
        ttl_ms: u64,
        /// Accumulate a capped `[lon,lat]` position track into each
        /// record's `data.track`, carried forward across position-less
        /// frames. Powers the ADS-B/APRS map trails without the frontend
        /// re-deriving them from the shared `recent` log. Reads `lat`/`lon`.
        track: bool,
        /// Shallow-merge each record into the entity's previous data
        /// instead of replacing it — for decoders whose message types
        /// carry different halves of an entity (AIS: position reports vs
        /// static name/callsign).
        merge: bool,
    },
    /// Single current record holding the whole payload (RSSI/signals).
    Replace,
    /// Single current record built by shallow-merging each event's fields
    /// into it — for decoders that emit *partial* updates the consumer is
    /// meant to accumulate (RDS: `pi` in one group, `ps` in another).
    Merge,
}

/// Default cap for append logs.
const LOG_CAP: usize = 512;

/// Cap for the `analysis` log.
const ANALYSIS_CAP: usize = 100;

/// Newest `analysis` records that keep spectrogram pixels (`cells_b64`).
const ANALYSIS_CELLS_KEEP: usize = 10;

/// Cap for the `ai` session log (one agent turn is several records).
const AI_LOG_CAP: usize = 1500;

/// Only this many of the newest `ai` records keep inline tool-result
/// images (base64 data URLs); older ones are stripped to an `images`
/// count so the log, the snapshot and the persisted store stay small.
const AI_IMAGE_KEEP: usize = 6;

/// Per-kind fold policy. Block owns the schema; this owns the folding.
fn policy_for(kind: &str) -> Policy {
    match kind {
        // Keyed tables — decoders that emit a structured entity id.
        "adsb" => Policy::Upsert {
            key_field: "icao",
            ttl_ms: 60_000,
            track: true,
            merge: false,
        },
        // AIS vessels keyed by MMSI; position (types 1-3/18) and static
        // (5/24) reports merge into one vessel record. 10 min TTL —
        // class B ships report every few minutes.
        "ais" => Policy::Upsert {
            key_field: "mmsi",
            ttl_ms: 600_000,
            track: true,
            merge: true,
        },
        // APRS stations persist for the session (no TTL expiry).
        "aprs" => Policy::Upsert {
            key_field: "call",
            ttl_ms: 0,
            track: true,
            merge: false,
        },
        // RDS emits partial groups → accumulate into one record.
        "rds" => Policy::Merge,
        // Single-current snapshot (whole payload each emit). `readback`
        // (live SoapySource driver state) and `flowdiag` (per-block
        // throughput/health) are folded by the pipeline diag tick — the
        // store is the one home for all live state, sliced by MCP and
        // mirrored to the UI over WS. `graph` (the composed doc + browser
        // half + source at the current epoch), `status` (pipeline run
        // state) and `params/<block>` (last live param set per block) are
        // the coherence kinds: whole-document, retained, no reducer.
        "rssi" | "signals" | "readback" | "flowdiag" | "graph" | "status" | "health" | "blocks"
        | "emitter_config" => Policy::Replace,
        k if k.starts_with("params/") => Policy::Replace,
        // Capture / survey / scan jobs — keyed by id, never expire (the
        // registry caps its own list), no track.
        "jobs" => Policy::Upsert {
            key_field: "job_id",
            ttl_ms: 0,
            track: false,
            merge: false,
        },
        // Spectrum-analyzer rows (`SweepSource` / `SweepSim`), one per
        // sweep step: keyed by step so the current set *is* the last full
        // pass (late joiners and MCP read it whole). A stale range's rows
        // age out; consumers also filter on the row's `cfg`.
        "panorama" => Policy::Upsert {
            key_field: "step",
            ttl_ms: 300_000,
            track: false,
            merge: false,
        },
        // The emitter list (`emitters.rs`): every transmitter seen by the
        // receiver's SignalList or the spectrum analyzer, keyed by its
        // number, persisted — the station's memory of the band.
        "emitter" => Policy::Upsert {
            key_field: "id",
            ttl_ms: 0,
            track: false,
            merge: false,
        },
        // AI session bus — every conversation, whichever client drives
        // it (the ferrite-ai sidecar, a Claude Code session's hooks):
        // one record per user prompt / finished text block / tool use /
        // tool result / turn end / reset, tagged with `session` and
        // `source`, published through `ferrite/ingest/ai`. Every AI
        // panel renders from it; MCP reads it via `decodes ai`. Larger
        // cap than a decode log: one agent turn is several records.
        "ai" => Policy::Append { cap: AI_LOG_CAP },
        // Toolkit measurements (spectrum / spectrogram / envelope) the AI
        // panel draws inline; heavy spectrogram cells kept on the newest
        // few only.
        "analysis" => Policy::Append { cap: ANALYSIS_CAP },
        // In-flight assistant text per session (partial block, ≤4 Hz) so
        // a panel can show the reply forming. Keyed by session; the
        // finished block lands in `ai`.
        "ai_live" => Policy::Upsert {
            key_field: "session",
            ttl_ms: 600_000,
            track: false,
            merge: false,
        },
        // One record per mutating CLI / agent request (the activity
        // middleware) — the AI panel's transcript and `ferrite/state/activity`.
        "activity" => Policy::Append { cap: LOG_CAP },
        // Everything else is an append log — ft8, wspr, pager, eas, dtmf,
        // cw, rtl_433, transcribe, fldigi, …. (rtl_433 can become a keyed
        // table once it emits a stable device id.)
        _ => Policy::Append { cap: LOG_CAP },
    }
}

/// Kinds that describe the running process — the composed graph, run
/// state, driver readback, live meters, per-block params, health. A new
/// process rebuilds them, so they are never persisted or restored (a
/// restored `graph` would point the browser at a topology that no
/// longer exists).
#[must_use]
pub fn is_live_kind(kind: &str) -> bool {
    matches!(
        kind,
        "rssi"
            | "signals"
            | "readback"
            | "flowdiag"
            | "graph"
            | "status"
            | "health"
            | "blocks"
            | "ai_live"
            | "panorama"
    ) || kind.starts_with("params/")
}

/// Kinds that carry decodes heard off the air (and so get an RF
/// [`Record::ctx`]): everything except the live kinds and the
/// bookkeeping logs (activity, AI transcript, jobs).
#[must_use]
pub fn is_decode_kind(kind: &str) -> bool {
    !is_live_kind(kind)
        && !matches!(
            kind,
            "activity" | "ai" | "jobs" | "analysis" | "emitter" | "emitter_config"
        )
        && !kind.starts_with("ai/")
}

/// One decoder record. `data` is the emitting block's JSON verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub seq: u64,
    /// Graph epoch current when this record was folded. A consumer that
    /// has loaded a different graph can attribute (or discard) it.
    pub epoch: u64,
    pub at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub data: Value,
    /// Where the radio was listening when this decode was folded
    /// (`{listen_hz, lo_hz, preset}`), stamped on decoder kinds only — so
    /// a decode carries its RF frequency without a join on `graph`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx: Option<Value>,
}

/// Per-kind state. `recent` for append logs; `current` for keyed tables /
/// replace. Both serialize so the browser mirror is a 1:1 copy.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KindState {
    pub recent: VecDeque<Record>,
    pub current: BTreeMap<String, Record>,
}

/// Whole-store snapshot for the REST/MCP read path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreSnapshot {
    pub seq: u64,
    pub epoch: u64,
    pub kinds: BTreeMap<String, KindState>,
}

/// A change broadcast to WS mirrors (consumed in P2).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum DeltaOp {
    /// Append a record (log kinds).
    Add { record: Record },
    /// Set the keyed entity / single-current record (upsert / replace).
    Upsert { record: Record },
    /// Drop a keyed entity (TTL expiry). Constructed by `expire`.
    Expire { key: String },
    /// Clear the whole kind.
    Reset,
}

#[derive(Debug, Clone, Serialize)]
pub struct StateDelta {
    pub kind: String,
    pub seq: u64,
    pub epoch: u64,
    #[serde(flatten)]
    pub op: DeltaOp,
}

/// Replace-kind store their single record under this fixed key.
const REPLACE_KEY: &str = "current";

struct Inner {
    seq: u64,
    kinds: BTreeMap<String, KindState>,
}

/// The store. `Arc`-shared like the broadcast sink; the `EventStore`
/// block writes through the [`DecoderSink`] impl, REST reads snapshots.
pub struct DecoderStore {
    inner: Mutex<Inner>,
    tx: broadcast::Sender<StateDelta>,
    /// Graph epoch — owned here, shared (by `Arc`) with the frame sink
    /// and bumped by `AppState` once per topology rebuild. Every record
    /// and every frame carries the value current at fold/emit time.
    epoch: Arc<AtomicU64>,
    /// RF context stamped onto decoder records ([`Record::ctx`]); kept
    /// current by the daemon (supervisor tick + every tune).
    ctx: Mutex<Option<Value>>,
    /// Who-said-what: stamps `speaker` onto `dsd` and `transcribe`
    /// records before they are stored ([`speakers`]).
    speakers: Mutex<speakers::SpeakerBook>,
}

impl Default for DecoderStore {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for DecoderStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = self.guard();
        f.debug_struct("DecoderStore")
            .field("seq", &g.seq)
            .field("epoch", &self.epoch())
            .field("kinds", &g.kinds.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl DecoderStore {
    #[must_use]
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(1024);
        Self {
            inner: Mutex::new(Inner {
                seq: 0,
                kinds: BTreeMap::new(),
            }),
            tx,
            epoch: Arc::new(AtomicU64::new(0)),
            ctx: Mutex::new(None),
            speakers: Mutex::new(speakers::SpeakerBook::new()),
        }
    }

    /// Set the RF context stamped onto subsequent decoder records.
    pub fn set_ctx(&self, ctx: Option<Value>) {
        *self.ctx.lock().unwrap_or_else(|e| e.into_inner()) = ctx;
    }

    /// Subscribe to the live delta stream (the WS mirror feed).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<StateDelta> {
        self.tx.subscribe()
    }

    /// The shared epoch counter (hand it to the frame sink / mount).
    #[must_use]
    pub fn epoch_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.epoch)
    }

    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// Bump the epoch; returns the new value. Called by the pipeline
    /// layer at each topology commit.
    pub fn bump_epoch(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Lock the inner state, recovering from a poisoned mutex (mirrors
    /// `FrameBus` / `BroadcastSink`). A panic while holding this lock —
    /// e.g. a decoder block unwinding mid-`apply` — leaves no broken
    /// invariant: a snapshot is a clone and a fold is idempotent per
    /// policy. Recovering the guard keeps the store (and every decode +
    /// REST read after it) alive instead of propagating the poison and
    /// wedging the whole daemon.
    fn guard(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Full snapshot — the REST/MCP read path.
    #[must_use]
    pub fn snapshot(&self) -> StoreSnapshot {
        let g = self.guard();
        StoreSnapshot {
            seq: g.seq,
            epoch: self.epoch(),
            kinds: g.kinds.clone(),
        }
    }

    /// Snapshot of only the kinds that outlive the process — what the
    /// persister writes to disk. Live kinds ([`is_live_kind`]) describe
    /// *this* process's graph and radio and are rebuilt on boot.
    #[must_use]
    pub fn persistable_snapshot(&self) -> StoreSnapshot {
        let g = self.guard();
        StoreSnapshot {
            seq: g.seq,
            epoch: self.epoch(),
            kinds: g
                .kinds
                .iter()
                .filter(|(k, _)| !is_live_kind(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        }
    }

    /// Seed the store from a snapshot a previous process persisted. Call
    /// once at boot, before anything publishes: no deltas are broadcast
    /// (the event plane republishes retained state from the store when
    /// it starts). Live kinds in `snap` are ignored. `seq` and `epoch`
    /// continue from the saved values so a mirror's seq watermark and the
    /// ingest epoch check stay monotonic across the restart. Jobs that
    /// were still `running` when the old process died are marked
    /// `interrupted` — nothing will ever finish them. Returns the number
    /// of kinds restored.
    pub fn restore(&self, snap: StoreSnapshot) -> usize {
        let mut g = self.guard();
        g.seq = g.seq.max(snap.seq);
        self.epoch.fetch_max(snap.epoch, Ordering::Relaxed);
        let mut n = 0;
        for (kind, mut state) in snap.kinds {
            if is_live_kind(&kind) {
                continue;
            }
            if kind == "jobs" {
                for rec in state.current.values_mut().chain(state.recent.iter_mut()) {
                    if rec.data.get("status").and_then(Value::as_str) == Some("running") {
                        if let Some(obj) = rec.data.as_object_mut() {
                            obj.insert("status".into(), Value::from("interrupted"));
                            obj.insert(
                                "error".into(),
                                Value::from("daemon restarted before the job finished"),
                            );
                        }
                    }
                }
            }
            g.kinds.insert(kind, state);
            n += 1;
        }
        n
    }

    /// One kind's state (the spot map reads its decoder kind here).
    #[must_use]
    pub fn snapshot_kind(&self, kind: &str) -> Option<KindState> {
        self.guard().kinds.get(kind).cloned()
    }

    /// Fold a live param delta into the stored `blocks` snapshot, in
    /// place — no new seq, no broadcast. Live edits travel to connected
    /// clients as `params/<block>` deltas (a 35 KB `blocks` re-publish per
    /// VFO click would be wasteful), but the snapshot is what a *newly*
    /// connecting client (and REST / MCP) reads, and `params/<block>` only
    /// keeps the last delta per block. Without this fold the snapshot kept
    /// the values from the last graph compose: a page loaded after a few
    /// clicks showed a VFO two tunes old, and its own click-to-tune looked
    /// dead (found live 2026-10-14).
    pub fn fold_block_params(&self, block: &str, delta: &serde_json::Map<String, Value>) {
        let mut g = self.guard();
        let Some(rec) = g
            .kinds
            .get_mut("blocks")
            .and_then(|k| k.current.get_mut(REPLACE_KEY))
        else {
            return;
        };
        let Some(list) = rec.data.get_mut("blocks").and_then(Value::as_array_mut) else {
            return;
        };
        for b in list.iter_mut() {
            if b.get("id").and_then(Value::as_str) != Some(block) {
                continue;
            }
            if !b.get("values").is_some_and(Value::is_object) {
                b["values"] = Value::Object(serde_json::Map::new());
            }
            if let Some(values) = b.get_mut("values").and_then(Value::as_object_mut) {
                for (k, v) in delta {
                    values.insert(k.clone(), v.clone());
                }
            }
        }
    }

    /// Fold one record into `kind` per its policy; broadcasts the delta.
    pub fn apply(&self, kind: &str, mut data: Value) {
        if kind == "dsd" || kind == "transcribe" {
            self.speakers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .enrich(kind, &mut data, now_ms());
        }
        let policy = policy_for(kind);
        let epoch = self.epoch();
        let ctx = if is_decode_kind(kind) {
            self.ctx.lock().unwrap_or_else(|e| e.into_inner()).clone()
        } else {
            None
        };
        let mut g = self.guard();
        if kind == "dsd" && data.get("event").and_then(Value::as_str) == Some("alias") {
            if let (Some(key), Some(label)) = (
                data.pointer("/speaker/key").and_then(Value::as_str),
                data.pointer("/speaker/label").and_then(Value::as_str),
            ) {
                relabel_speaker(&mut g.kinds, key, label);
            }
        }
        g.seq += 1;
        let seq = g.seq;
        let entry = g.kinds.entry(kind.to_string()).or_default();

        let op = match policy {
            Policy::Append { cap } => {
                let record = Record {
                    seq,
                    epoch,
                    at_ms: now_ms(),
                    key: None,
                    data,
                    ctx: ctx.clone(),
                };
                entry.recent.push_back(record.clone());
                while entry.recent.len() > cap {
                    entry.recent.pop_front();
                }
                if kind == "ai" {
                    strip_old_images(&mut entry.recent);
                }
                if kind == "analysis" {
                    strip_old_field(&mut entry.recent, "cells_b64", ANALYSIS_CELLS_KEEP);
                }
                DeltaOp::Add { record }
            }
            Policy::Upsert {
                key_field,
                track,
                merge,
                ..
            } => {
                let key = extract_key(&data, key_field);
                let mut data = data;
                if merge {
                    if let Some(Value::Object(prev)) = entry.current.get(&key).map(|r| &r.data) {
                        let mut merged = prev.clone();
                        if let Some(new) = data.as_object() {
                            for (k, v) in new {
                                merged.insert(k.clone(), v.clone());
                            }
                        }
                        data = Value::Object(merged);
                    }
                }
                if track {
                    // Carry the prior track forward and append this fix.
                    let prev = entry
                        .current
                        .get(&key)
                        .and_then(|r| r.data.get("track"))
                        .cloned();
                    if let Some(t) = push_track(prev, &data) {
                        if let Some(obj) = data.as_object_mut() {
                            obj.insert("track".to_string(), t);
                        }
                    }
                }
                let record = Record {
                    seq,
                    epoch,
                    at_ms: now_ms(),
                    key: Some(key.clone()),
                    data,
                    ctx: ctx.clone(),
                };
                entry.current.insert(key, record.clone());
                // Tables also keep a capped frame log in `recent`, so
                // consumers can derive history the latest-per-key `current`
                // can't (APRS packet console, ADS-B trails, per-key counts).
                // Not panorama rows: ~10 KB each, and `current` already is
                // the whole pass.
                if kind != "panorama" && kind != "emitter" {
                    entry.recent.push_back(record.clone());
                    while entry.recent.len() > LOG_CAP {
                        entry.recent.pop_front();
                    }
                }
                DeltaOp::Upsert { record }
            }
            Policy::Replace => {
                let record = Record {
                    seq,
                    epoch,
                    at_ms: now_ms(),
                    key: None,
                    data,
                    ctx: ctx.clone(),
                };
                entry.current.clear();
                entry
                    .current
                    .insert(REPLACE_KEY.to_string(), record.clone());
                DeltaOp::Upsert { record }
            }
            Policy::Merge => {
                // Shallow-merge the new fields into the accumulated record.
                let mut merged = entry
                    .current
                    .get(REPLACE_KEY)
                    .map(|r| r.data.clone())
                    .filter(Value::is_object)
                    .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
                if let (Some(dst), Some(src)) = (merged.as_object_mut(), data.as_object()) {
                    for (k, v) in src {
                        dst.insert(k.clone(), v.clone());
                    }
                }
                let record = Record {
                    seq,
                    epoch,
                    at_ms: now_ms(),
                    key: None,
                    data: merged,
                    ctx: ctx.clone(),
                };
                entry
                    .current
                    .insert(REPLACE_KEY.to_string(), record.clone());
                // Send the full merged record — mirrors just replace.
                DeltaOp::Upsert { record }
            }
        };
        drop(g);
        let _ = self.tx.send(StateDelta {
            kind: kind.to_string(),
            seq,
            epoch,
            op,
        });
    }

    /// Clear a kind (fresh decode session / explicit reset).
    pub fn reset_kind(&self, kind: &str) {
        let epoch = self.epoch();
        let mut g = self.guard();
        g.seq += 1;
        let seq = g.seq;
        if let Some(k) = g.kinds.get_mut(kind) {
            k.recent.clear();
            k.current.clear();
        }
        drop(g);
        let _ = self.tx.send(StateDelta {
            kind: kind.to_string(),
            seq,
            epoch,
            op: DeltaOp::Reset,
        });
    }

    /// Drop one keyed entity (an explicit removal — e.g. an emitter merged
    /// into another). Broadcasts the same `expire` delta TTL expiry does,
    /// so every mirror drops it.
    pub fn remove(&self, kind: &str, key: &str) {
        let epoch = self.epoch();
        let mut g = self.guard();
        let removed = g
            .kinds
            .get_mut(kind)
            .is_some_and(|k| k.current.remove(key).is_some());
        if !removed {
            return;
        }
        g.seq += 1;
        let seq = g.seq;
        drop(g);
        let _ = self.tx.send(StateDelta {
            kind: kind.to_string(),
            seq,
            epoch,
            op: DeltaOp::Expire {
                key: key.to_string(),
            },
        });
    }

    /// Drop stale keyed entities for TTL kinds. Wired into the diag-tick.
    /// Returns the number expired.
    pub fn expire(&self, now: u64) -> usize {
        let mut g = self.guard();
        let mut expired: Vec<(String, String)> = Vec::new();
        for (kind, state) in &mut g.kinds {
            let Policy::Upsert { ttl_ms, .. } = policy_for(kind) else {
                continue;
            };
            if ttl_ms == 0 {
                continue;
            }
            let cutoff = now.saturating_sub(ttl_ms);
            let stale: Vec<String> = state
                .current
                .iter()
                .filter(|(_, r)| r.at_ms < cutoff)
                .map(|(k, _)| k.clone())
                .collect();
            for k in stale {
                state.current.remove(&k);
                expired.push((kind.clone(), k));
            }
        }
        let n = expired.len();
        let epoch = self.epoch();
        for (kind, key) in expired {
            g.seq += 1;
            let seq = g.seq;
            let _ = self.tx.send(StateDelta {
                kind,
                seq,
                epoch,
                op: DeltaOp::Expire { key },
            });
        }
        n
    }
}

/// `DecoderSink` is the block-side trait; the store is its server impl.
impl DecoderSink for DecoderStore {
    fn emit(&self, kind: &str, data: Value) {
        self.apply(kind, data);
    }
    fn reset(&self, kind: &str) {
        self.reset_kind(kind);
    }
}

/// Drop `field` from all but the newest `keep` records that carry it.
/// A talker alias often arrives a few overs after a radio's first call,
/// so its earlier `dsd` / `transcribe` records were stamped with the bare
/// ID. Rewrite their `speaker.label` in place — no new seq, no broadcast
/// (like [`DecoderStore::fold_block_params`]): a newly connecting client,
/// REST and MCP read the corrected history, and live tabs relabel on
/// render from the newest `dsd` label (`web/src/lib/transcript/flow.ts`).
fn relabel_speaker(kinds: &mut BTreeMap<String, KindState>, key: &str, label: &str) {
    for kind in ["dsd", "transcribe"] {
        let Some(state) = kinds.get_mut(kind) else {
            continue;
        };
        for rec in state.recent.iter_mut().chain(state.current.values_mut()) {
            if let Some(sp) = rec.data.get_mut("speaker").filter(|s| s["key"] == key) {
                sp["label"] = Value::from(label);
            }
        }
    }
}

fn strip_old_field(log: &mut VecDeque<Record>, field: &str, keep: usize) {
    let mut seen = 0usize;
    for rec in log.iter_mut().rev() {
        let Some(obj) = rec.data.as_object_mut() else {
            continue;
        };
        if obj.contains_key(field) {
            seen += 1;
            if seen > keep {
                obj.remove(field);
            }
        }
    }
}

/// Keep inline `images` only on the newest [`AI_IMAGE_KEEP`] records
/// that have them; replace older ones with their count.
fn strip_old_images(log: &mut VecDeque<Record>) {
    let mut seen = 0usize;
    for rec in log.iter_mut().rev() {
        let Some(obj) = rec.data.as_object_mut() else {
            continue;
        };
        let Some(Value::Array(imgs)) = obj.get("images") else {
            continue;
        };
        seen += 1;
        if seen > AI_IMAGE_KEEP {
            let n = imgs.len();
            obj.remove("images");
            obj.insert("images_stripped".into(), Value::from(n));
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Max position fixes retained per entity in `data.track`.
const TRACK_MAX: usize = 20;

/// Append this record's `[lon,lat]` to the running position track,
/// carrying the prior track forward. Frames without a numeric `lat`/`lon`
/// (e.g. velocity-only ADS-B messages) keep the existing track unchanged;
/// consecutive duplicate fixes are dropped; the track is capped at
/// [`TRACK_MAX`]. Returns `None` only when there's no track to set yet (no
/// position has ever been seen for this entity).
fn push_track(prev: Option<Value>, data: &Value) -> Option<Value> {
    let mut track: Vec<Value> = match prev {
        Some(Value::Array(a)) => a,
        _ => Vec::new(),
    };
    let lon = data.get("lon").and_then(Value::as_f64);
    let lat = data.get("lat").and_then(Value::as_f64);
    let (Some(lon), Some(lat)) = (lon, lat) else {
        // No new fix — preserve whatever history we had (if any).
        return (!track.is_empty()).then_some(Value::Array(track));
    };
    let point = serde_json::json!([lon, lat]);
    if track.last() != Some(&point) {
        track.push(point);
        while track.len() > TRACK_MAX {
            track.remove(0);
        }
    }
    Some(Value::Array(track))
}

/// Pull the table key from a record. Strings used verbatim; non-strings
/// (numeric mmsi) fall back to their JSON text. Missing → empty string
/// (still upserts, into a single "" slot, rather than dropping).
fn extract_key(data: &Value, key_field: &str) -> String {
    match data.get(key_field) {
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Regression: a panic while holding the inner lock poisons the mutex;
    // the store must recover (via `guard()`) rather than propagating the
    // poison and panicking on every subsequent decode + read. Without the
    // fix (`.lock().unwrap()`), the `apply` below panics.
    #[test]
    fn survives_a_poisoned_lock() {
        use std::sync::Arc;
        let store = Arc::new(DecoderStore::new());
        let s2 = Arc::clone(&store);
        let poisoned = std::thread::spawn(move || {
            let _g = s2.inner.lock().unwrap();
            panic!("poison the decoder-store lock");
        })
        .join();
        assert!(poisoned.is_err(), "the helper thread must have panicked");

        // Post-poison the store still folds + reads.
        store.apply("ft8", serde_json::json!({ "call": "CQ" }));
        let k = store
            .snapshot_kind("ft8")
            .expect("kind present after poison recovery");
        assert_eq!(k.recent.len(), 1);
        assert_eq!(store.snapshot().seq, 1);
    }

    #[test]
    fn ais_vessels_merge_position_and_static_reports() {
        let s = DecoderStore::new();
        s.apply(
            "ais",
            serde_json::json!({"mmsi": 366053209, "lat": 37.8, "lon": -122.3}),
        );
        s.apply(
            "ais",
            serde_json::json!({"mmsi": 366053209, "name": "MT.MITCHELL"}),
        );
        let k = s.snapshot_kind("ais").unwrap();
        let v = &k.current["366053209"].data;
        assert_eq!(v["name"], "MT.MITCHELL");
        assert_eq!(v["lat"], 37.8, "a static report keeps the last position");
        assert!(v["track"].is_array());
    }

    #[test]
    fn decode_records_carry_the_rf_context_bookkeeping_does_not() {
        let s = DecoderStore::new();
        s.set_ctx(Some(
            serde_json::json!({"listen_hz": 14_074_000.0, "preset": "ft8"}),
        ));
        s.apply("ft8", serde_json::json!({"de": "K6ABC"}));
        s.apply("activity", serde_json::json!({"summary": "tune"}));
        s.apply("graph", serde_json::json!({}));
        let snap = s.snapshot();
        assert_eq!(
            snap.kinds["ft8"].recent[0].ctx.as_ref().unwrap()["listen_hz"],
            14_074_000.0
        );
        assert!(snap.kinds["activity"].recent[0].ctx.is_none());
        assert!(snap.kinds["graph"].current["current"].ctx.is_none());
    }

    #[test]
    fn ai_log_keeps_images_only_on_the_newest_records() {
        let s = DecoderStore::new();
        for i in 0..(AI_IMAGE_KEEP + 3) {
            s.apply(
                "ai",
                serde_json::json!({"type": "tool_result", "n": i, "images": ["data:image/png;base64,AA"]}),
            );
        }
        let k = s.snapshot_kind("ai").unwrap();
        let with: Vec<_> = k
            .recent
            .iter()
            .filter(|r| r.data.get("images").is_some())
            .collect();
        assert_eq!(with.len(), AI_IMAGE_KEEP);
        assert_eq!(k.recent[0].data["images_stripped"], 1);
        assert!(k.recent.back().unwrap().data.get("images").is_some());
    }

    #[test]
    fn append_kind_logs_and_caps() {
        let s = DecoderStore::new();
        s.apply("ft8", serde_json::json!({"call": "AA"}));
        s.apply("ft8", serde_json::json!({"call": "BB"}));
        let k = s.snapshot_kind("ft8").unwrap();
        assert_eq!(k.recent.len(), 2);
        assert!(k.current.is_empty());
        assert_eq!(k.recent[0].data["call"], "AA");
        assert_eq!(k.recent[1].seq, 2);
    }

    #[test]
    fn ai_transcript_is_an_append_log() {
        let s = DecoderStore::new();
        s.apply("ai", serde_json::json!({"role": "user", "text": "hi"}));
        s.apply(
            "ai",
            serde_json::json!({"role": "assistant", "text": "hello"}),
        );
        let k = s.snapshot_kind("ai").unwrap();
        assert_eq!(k.recent.len(), 2, "turns append in order");
        assert!(k.current.is_empty(), "no keyed table for a transcript");
        assert_eq!(k.recent[0].data["role"], "user");
        assert_eq!(k.recent[1].data["text"], "hello");
    }

    #[test]
    fn upsert_kind_keys_by_field_and_replaces() {
        let s = DecoderStore::new();
        s.apply("adsb", serde_json::json!({"icao": "abc", "alt": 1000}));
        s.apply("adsb", serde_json::json!({"icao": "abc", "alt": 2000}));
        s.apply("adsb", serde_json::json!({"icao": "def", "alt": 500}));
        let k = s.snapshot_kind("adsb").unwrap();
        assert_eq!(k.current.len(), 2, "two distinct aircraft");
        assert_eq!(k.current["abc"].data["alt"], 2000, "latest wins");
        assert_eq!(k.current["abc"].key.as_deref(), Some("abc"));
    }

    #[test]
    fn upsert_accumulates_capped_dedup_position_track() {
        let s = DecoderStore::new();
        // Two distinct fixes + a repeat + a position-less (velocity) frame.
        s.apply(
            "adsb",
            serde_json::json!({"icao": "abc", "lon": 1.0, "lat": 2.0}),
        );
        s.apply(
            "adsb",
            serde_json::json!({"icao": "abc", "lon": 1.0, "lat": 2.0}),
        ); // dup
        s.apply(
            "adsb",
            serde_json::json!({"icao": "abc", "lon": 3.0, "lat": 4.0}),
        );
        s.apply("adsb", serde_json::json!({"icao": "abc", "gs": 400})); // no fix
        let k = s.snapshot_kind("adsb").unwrap();
        let track = k.current["abc"].data["track"].as_array().unwrap();
        assert_eq!(track.len(), 2, "dedup + position-less frame don't grow it");
        assert_eq!(track[0], serde_json::json!([1.0, 2.0]));
        assert_eq!(track[1], serde_json::json!([3.0, 4.0]));
        // The carried-forward track rides along on the velocity-only record.
        assert_eq!(k.current["abc"].data["track"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn upsert_track_caps_at_track_max() {
        let s = DecoderStore::new();
        for i in 0..(TRACK_MAX + 5) {
            #[allow(clippy::cast_precision_loss)]
            s.apply(
                "adsb",
                serde_json::json!({"icao": "abc", "lon": i as f64, "lat": 0.0}),
            );
        }
        let k = s.snapshot_kind("adsb").unwrap();
        let track = k.current["abc"].data["track"].as_array().unwrap();
        assert_eq!(track.len(), TRACK_MAX, "oldest fixes dropped");
        assert_eq!(track[0], serde_json::json!([5.0, 0.0]), "first 5 evicted");
    }

    #[test]
    fn replace_kind_keeps_one() {
        let s = DecoderStore::new();
        s.apply("signals", serde_json::json!({"signals": [1]}));
        s.apply("signals", serde_json::json!({"signals": [1, 2]}));
        let k = s.snapshot_kind("signals").unwrap();
        assert_eq!(k.current.len(), 1);
        assert_eq!(
            k.current[REPLACE_KEY].data["signals"],
            serde_json::json!([1, 2])
        );
    }

    #[test]
    fn reset_clears_a_kind() {
        let s = DecoderStore::new();
        s.apply("ft8", serde_json::json!({"x": 1}));
        s.reset_kind("ft8");
        assert!(s.snapshot_kind("ft8").unwrap().recent.is_empty());
    }

    #[test]
    fn expire_drops_stale_table_entries_only() {
        let s = DecoderStore::new();
        s.apply("adsb", serde_json::json!({"icao": "old"}));
        s.apply("aprs", serde_json::json!({"call": "KEEP"})); // ttl 0 = persist
                                                              // Force the adsb entry's timestamp into the deep past.
        {
            let mut g = s.inner.lock().unwrap();
            g.kinds
                .get_mut("adsb")
                .unwrap()
                .current
                .get_mut("old")
                .unwrap()
                .at_ms = 0;
        }
        let n = s.expire(now_ms());
        assert_eq!(n, 1, "only the stale adsb entry expired");
        assert!(s.snapshot_kind("adsb").unwrap().current.is_empty());
        assert_eq!(
            s.snapshot_kind("aprs").unwrap().current.len(),
            1,
            "aprs persists (ttl 0)"
        );
    }

    #[test]
    fn deltas_broadcast_to_subscribers() {
        let s = DecoderStore::new();
        let mut rx = s.subscribe();
        s.apply("adsb", serde_json::json!({"icao": "abc"}));
        let d = rx.try_recv().expect("delta sent");
        assert_eq!(d.kind, "adsb");
        assert!(matches!(d.op, DeltaOp::Upsert { .. }));
    }

    #[test]
    fn emit_via_decodersink_trait() {
        let s = DecoderStore::new();
        DecoderSink::emit(&s, "pager", serde_json::json!({"msg": "hi"}));
        assert_eq!(s.snapshot_kind("pager").unwrap().recent.len(), 1);
    }

    #[test]
    fn records_carry_the_epoch_current_at_fold_time() {
        let s = DecoderStore::new();
        let mut rx = s.subscribe();
        s.apply("pager", serde_json::json!({"msg": "before"}));
        assert_eq!(s.bump_epoch(), 1);
        s.apply("pager", serde_json::json!({"msg": "after"}));
        let recent = &s.snapshot_kind("pager").unwrap().recent;
        assert_eq!(recent[0].epoch, 0, "folded before the bump");
        assert_eq!(recent[1].epoch, 1, "folded after the bump");
        // The deltas say the same thing as the records.
        let d0 = rx.try_recv().unwrap();
        let d1 = rx.try_recv().unwrap();
        assert_eq!((d0.epoch, d1.epoch), (0, 1));
        assert_eq!(s.snapshot().epoch, 1);
    }

    #[test]
    fn epoch_handle_is_shared_with_the_sink_side() {
        // The frame sink stamps frames from the same counter the store
        // stamps records with — one epoch, two observers.
        let s = DecoderStore::new();
        let h = s.epoch_handle();
        s.bump_epoch();
        assert_eq!(h.load(Ordering::Relaxed), 1);
        h.store(7, Ordering::Relaxed);
        assert_eq!(s.epoch(), 7);
        s.apply("ft8", serde_json::json!({"x": 1}));
        assert_eq!(s.snapshot_kind("ft8").unwrap().recent[0].epoch, 7);
    }

    #[test]
    fn reset_delta_carries_the_new_epoch() {
        let s = DecoderStore::new();
        let mut rx = s.subscribe();
        s.bump_epoch();
        s.reset_kind("adsb");
        let d = rx.try_recv().unwrap();
        assert!(matches!(d.op, DeltaOp::Reset));
        assert_eq!(d.epoch, 1);
    }

    #[test]
    fn a_late_alias_relabels_earlier_records() {
        // VU3ITI's first overs were stamped "4040379"; the alias arrived
        // afterwards. The stored history must read VU3ITI.
        let s = DecoderStore::new();
        let call = serde_json::json!({"t":"dsd","event":"call","protocol":"DMR","slot":1,
                                      "tgt":"91","src":"4040379"});
        s.apply("dsd", call);
        s.apply(
            "transcribe",
            serde_json::json!({"text":"cq cq","t0":0.0,"t1":1.0,"lane":"slot1"}),
        );
        s.apply(
            "dsd",
            serde_json::json!({"t":"dsd","event":"call","protocol":"DMR","slot":2,
                               "tgt":"91","src":"3107998"}),
        );
        assert_eq!(
            s.snapshot_kind("dsd").unwrap().recent[0].data["speaker"]["label"],
            "4040379"
        );
        s.apply(
            "dsd",
            serde_json::json!({"t":"dsd","event":"alias","protocol":"DMR","slot":1,
                               "src":"4040379","text":"VU3ITI"}),
        );
        let dsd = s.snapshot_kind("dsd").unwrap();
        assert_eq!(dsd.recent[0].data["speaker"]["label"], "VU3ITI");
        assert_eq!(
            dsd.recent[1].data["speaker"]["label"], "3107998",
            "other talker untouched"
        );
        let line = &s.snapshot_kind("transcribe").unwrap().recent[0].data;
        assert_eq!(line["speaker"]["label"], "VU3ITI");
    }

    #[test]
    fn transcripts_are_stamped_with_the_talker_through_apply() {
        // The store wiring, not just the SpeakerBook policy: a DMR call
        // record then a slot-2 transcript record, both through `apply`
        // as the EventStore blocks do, must come out with one `speaker`.
        let s = DecoderStore::new();
        s.apply(
            "dsd",
            serde_json::json!({"t":"dsd","event":"call","protocol":"DMR","slot":2,
                               "tgt":"31068","src":"3107998"}),
        );
        s.apply(
            "transcribe",
            serde_json::json!({"text":"go ahead","t0":0.0,"t1":1.0,"lane":"slot2"}),
        );
        let call = &s.snapshot_kind("dsd").unwrap().recent[0].data;
        let line = &s.snapshot_kind("transcribe").unwrap().recent[0].data;
        assert_eq!(call["speaker"]["key"], "DMR:3107998");
        assert_eq!(line["speaker"]["key"], "DMR:3107998", "{line}");
        assert_eq!(line["speaker"]["group"], "31068");
        assert_eq!(line["speaker"]["colour"], call["speaker"]["colour"]);
        // Other kinds pass through untouched.
        s.apply("ft8", serde_json::json!({"msg":"CQ K1ABC FN42"}));
        assert!(s.snapshot_kind("ft8").unwrap().recent[0]
            .data
            .get("speaker")
            .is_none());
    }

    #[test]
    fn param_deltas_fold_into_the_blocks_snapshot_quietly() {
        let s = DecoderStore::new();
        s.apply(
            "blocks",
            serde_json::json!({"blocks": [
                {"id": "chan", "values": {"freq_shift_hz": 942_736, "output_rate_hz": 48_000.0}},
                {"id": "demod", "values": {"max_deviation_hz": 5000}}
            ]}),
        );
        let before = s.snapshot().seq;
        let mut rx = s.subscribe();
        let mut d = serde_json::Map::new();
        d.insert("freq_shift_hz".into(), serde_json::json!(480_000));
        s.fold_block_params("chan", &d);
        let b = s.snapshot_kind("blocks").unwrap();
        let rec = &b.current[REPLACE_KEY].data["blocks"];
        assert_eq!(
            rec[0]["values"]["freq_shift_hz"], 480_000,
            "snapshot updated"
        );
        assert_eq!(
            rec[0]["values"]["output_rate_hz"], 48_000.0,
            "other values kept"
        );
        assert_eq!(
            rec[1]["values"]["max_deviation_hz"], 5000,
            "other blocks untouched"
        );
        assert_eq!(s.snapshot().seq, before, "no new seq");
        assert!(rx.try_recv().is_err(), "no broadcast");
        // No `blocks` record yet → a no-op, not a panic.
        DecoderStore::new().fold_block_params("chan", &d);
    }

    #[test]
    fn graph_status_params_and_jobs_policies() {
        let s = DecoderStore::new();
        s.apply("graph", serde_json::json!({"epoch": 1}));
        s.apply("graph", serde_json::json!({"epoch": 2}));
        let g = s.snapshot_kind("graph").unwrap();
        assert_eq!(g.current.len(), 1, "graph is a Replace kind");
        assert_eq!(g.current[REPLACE_KEY].data["epoch"], 2);
        assert!(g.recent.is_empty(), "replace kinds keep no log");

        s.apply("params/demod", serde_json::json!({"params": {"a": 1}}));
        s.apply("params/demod", serde_json::json!({"params": {"a": 2}}));
        let p = s.snapshot_kind("params/demod").unwrap();
        assert_eq!(p.current.len(), 1, "params/<block> is a Replace kind");

        s.apply("jobs", serde_json::json!({"job_id": 1, "state": "running"}));
        s.apply("jobs", serde_json::json!({"job_id": 1, "state": "done"}));
        s.apply("jobs", serde_json::json!({"job_id": 2, "state": "running"}));
        let j = s.snapshot_kind("jobs").unwrap();
        assert_eq!(j.current.len(), 2, "jobs upsert by job_id");
        assert_eq!(j.current["1"].data["state"], "done");
        assert_eq!(s.expire(now_ms() + 1_000_000), 0, "jobs never expire");
    }
}
