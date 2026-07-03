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

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use ferrite_blocks::DecoderSink;
use serde::Serialize;
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

/// Per-kind fold policy. Block owns the schema; this owns the folding.
fn policy_for(kind: &str) -> Policy {
    match kind {
        // Keyed tables — decoders that emit a structured entity id.
        "adsb" => Policy::Upsert {
            key_field: "icao",
            ttl_ms: 60_000,
            track: true,
        },
        // APRS stations persist for the session (no TTL expiry).
        "aprs" => Policy::Upsert {
            key_field: "call",
            ttl_ms: 0,
            track: true,
        },
        // RDS emits partial groups → accumulate into one record.
        "rds" => Policy::Merge,
        // Single-current snapshot (whole payload each emit). `readback`
        // (live SoapySource driver state) and `flowdiag` (per-block
        // throughput/health) are folded by the pipeline diag tick — the
        // store is the one home for all live state, sliced by MCP and
        // mirrored to the UI over WS.
        "rssi" | "signals" | "readback" | "flowdiag" => Policy::Replace,
        // AI transcript — append log of conversation turns (one record per
        // user/assistant turn), folded by ferrited's `/ws/chat` proxy +
        // `chat-inject` path. Read by MCP via `decodes ai`.
        "ai" => Policy::Append { cap: LOG_CAP },
        // Everything else is an append log — ft8, wspr, pager, eas, dtmf,
        // cw, ais, rtl_433, transcribe, fldigi, …. (ais/rtl_433 can become
        // keyed tables once they emit a parsed mmsi / device id.)
        _ => Policy::Append { cap: LOG_CAP },
    }
}

/// One decoder record. `data` is the emitting block's JSON verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub seq: u64,
    pub at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub data: Value,
}

/// Per-kind state. `recent` for append logs; `current` for keyed tables /
/// replace. Both serialize so the browser mirror is a 1:1 copy.
#[derive(Debug, Clone, Default, Serialize)]
pub struct KindState {
    pub recent: VecDeque<Record>,
    pub current: BTreeMap<String, Record>,
}

/// Whole-store snapshot for the REST/MCP read path.
#[derive(Debug, Clone, Serialize)]
pub struct StoreSnapshot {
    pub seq: u64,
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
}

impl Default for DecoderStore {
    fn default() -> Self {
        Self::new()
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
        }
    }

    /// Subscribe to the live delta stream (the WS mirror feed).
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<StateDelta> {
        self.tx.subscribe()
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
            kinds: g.kinds.clone(),
        }
    }

    /// One kind's state. The HTTP read path is the whole-store
    /// `GET /api/state` (MCP slices the kind it wants), so this has no
    /// in-crate caller outside tests — kept as a public accessor.
    #[must_use]
    #[allow(dead_code)]
    pub fn snapshot_kind(&self, kind: &str) -> Option<KindState> {
        self.guard().kinds.get(kind).cloned()
    }

    /// Fold one record into `kind` per its policy; broadcasts the delta.
    pub fn apply(&self, kind: &str, data: Value) {
        let policy = policy_for(kind);
        let mut g = self.guard();
        g.seq += 1;
        let seq = g.seq;
        let entry = g.kinds.entry(kind.to_string()).or_default();

        let op = match policy {
            Policy::Append { cap } => {
                let record = Record {
                    seq,
                    at_ms: now_ms(),
                    key: None,
                    data,
                };
                entry.recent.push_back(record.clone());
                while entry.recent.len() > cap {
                    entry.recent.pop_front();
                }
                DeltaOp::Add { record }
            }
            Policy::Upsert {
                key_field, track, ..
            } => {
                let key = extract_key(&data, key_field);
                let mut data = data;
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
                    at_ms: now_ms(),
                    key: Some(key.clone()),
                    data,
                };
                entry.current.insert(key, record.clone());
                // Tables also keep a capped frame log in `recent`, so
                // consumers can derive history the latest-per-key `current`
                // can't (APRS packet console, ADS-B trails, per-key counts).
                entry.recent.push_back(record.clone());
                while entry.recent.len() > LOG_CAP {
                    entry.recent.pop_front();
                }
                DeltaOp::Upsert { record }
            }
            Policy::Replace => {
                let record = Record {
                    seq,
                    at_ms: now_ms(),
                    key: None,
                    data,
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
                    at_ms: now_ms(),
                    key: None,
                    data: merged,
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
            op,
        });
    }

    /// Clear a kind (fresh decode session / explicit reset).
    pub fn reset_kind(&self, kind: &str) {
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
            op: DeltaOp::Reset,
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
        for (kind, key) in expired {
            g.seq += 1;
            let seq = g.seq;
            let _ = self.tx.send(StateDelta {
                kind,
                seq,
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
}
