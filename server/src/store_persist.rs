//! Persist the state store across daemon restarts.
//!
//! The store is POJO + serde, so persistence is a JSON snapshot of the
//! kinds that outlive the process (decodes, transcripts, the AI
//! transcript, activity, jobs, keyed tables) — never the live kinds
//! (`graph`, `status`, `readback`, …; see
//! [`crate::decoder_store::is_live_kind`]).
//!
//! - **Load** once at boot, before the event plane starts, via
//!   [`DecoderStore::restore`].
//! - **Save** from a background task: any delta on a persisted kind marks
//!   the store dirty; at most every [`SAVE_INTERVAL`] a dirty store is
//!   written. A final save runs on graceful shutdown.
//! - Writes are atomic (temp file + rename), so a crash mid-write leaves
//!   the previous file intact.
//!
//! Off unless the operator passes `--state-dir` / `FERRITE_STATE_DIR`:
//! test harnesses spawn throwaway daemons and must not inherit records.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::broadcast;

use crate::decoder_store::{is_live_kind, DecoderStore, StoreSnapshot};

/// Longest a dirty store waits before it is written.
pub const SAVE_INTERVAL: Duration = Duration::from_secs(5);

/// File name inside the state dir.
const STORE_FILE: &str = "store.json";

/// Where the store lives inside `dir`.
#[must_use]
pub fn store_path(dir: &Path) -> PathBuf {
    dir.join(STORE_FILE)
}

/// Read a persisted snapshot. `Ok(None)` when there is no file yet; a
/// corrupt file is an error the caller logs and moves past (it is
/// renamed aside so the next save doesn't silently discard evidence).
pub fn load(path: &Path) -> Result<Option<StoreSnapshot>> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    match serde_json::from_slice(&bytes) {
        Ok(snap) => Ok(Some(snap)),
        Err(e) => {
            let aside = path.with_extension("json.corrupt");
            let _ = std::fs::rename(path, &aside);
            Err(anyhow::anyhow!(
                "parse {}: {e} (moved aside to {})",
                path.display(),
                aside.display()
            ))
        }
    }
}

/// Atomically write the store's persistable kinds to `path`.
pub fn save(store: &DecoderStore, path: &Path) -> Result<()> {
    let snap = store.persistable_snapshot();
    let json = serde_json::to_vec(&snap).context("serialize store")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}

/// Load `dir/store.json` into `store`. Logs what happened; never fails
/// the boot (a daemon with an empty store beats no daemon).
pub fn restore_from(store: &DecoderStore, dir: &Path) {
    let path = store_path(dir);
    match load(&path) {
        Ok(Some(snap)) => {
            let seq = snap.seq;
            let kinds = store.restore(snap);
            tracing::info!(path = %path.display(), kinds, seq, "state store restored");
        }
        Ok(None) => tracing::info!(path = %path.display(), "no persisted state store yet"),
        Err(e) => tracing::warn!(error = %e, "persisted state store unreadable; starting empty"),
    }
}

/// Spawn the periodic saver. Subscribe happens here (not in the task)
/// so nothing folded right after boot is missed.
pub fn spawn_saver(store: Arc<DecoderStore>, path: PathBuf) {
    let mut rx = store.subscribe();
    tokio::spawn(async move {
        let mut dirty = false;
        let mut tick = tokio::time::interval(SAVE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                d = rx.recv() => match d {
                    Ok(delta) => dirty |= !is_live_kind(&delta.kind),
                    // Missed deltas may have touched persisted kinds.
                    Err(broadcast::error::RecvError::Lagged(_)) => dirty = true,
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = tick.tick() => {
                    if dirty {
                        dirty = false;
                        let (s, p) = (Arc::clone(&store), path.clone());
                        let res = tokio::task::spawn_blocking(move || save(&s, &p)).await;
                        match res {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => tracing::warn!(error = %e, "state store save failed"),
                            Err(e) => tracing::warn!(error = %e, "state store save task panicked"),
                        }
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ferrite-persist-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip_keeps_history_and_drops_live_kinds() {
        let dir = tmpdir("rt");
        let a = DecoderStore::new();
        a.bump_epoch();
        a.bump_epoch();
        a.apply("ft8", json!({"call": "K6ABC", "snr": -12}));
        a.apply(
            "aprs",
            json!({"call": "WR6ABD", "lat": 37.0, "lon": -122.0}),
        );
        a.apply("ai", json!({"role": "user", "text": "hi"}));
        a.apply("graph", json!({"epoch": 2}));
        a.apply("status", json!({"status": "running"}));
        a.apply("params/chan", json!({"freq_shift_hz": 1}));
        let seq = a.snapshot().seq;
        save(&a, &store_path(&dir)).unwrap();

        let b = DecoderStore::new();
        restore_from(&b, &dir);
        let snap = b.snapshot();
        assert_eq!(snap.seq, seq, "seq continues from the saved value");
        assert_eq!(snap.epoch, 2, "epoch continues from the saved value");
        assert_eq!(snap.kinds["ft8"].recent[0].data["call"], "K6ABC");
        assert!(snap.kinds["aprs"].current.contains_key("WR6ABD"));
        assert_eq!(snap.kinds["ai"].recent.len(), 1);
        for live in ["graph", "status", "params/chan"] {
            assert!(!snap.kinds.contains_key(live), "{live} must not persist");
        }
        // New records keep counting up from the restored seq.
        b.apply("ft8", json!({"call": "N0CALL"}));
        assert_eq!(b.snapshot().kinds["ft8"].recent[1].seq, seq + 1);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn running_jobs_come_back_interrupted() {
        let dir = tmpdir("jobs");
        let a = DecoderStore::new();
        a.apply("jobs", json!({"job_id": "cap-1", "status": "done"}));
        a.apply("jobs", json!({"job_id": "cap-2", "status": "running"}));
        save(&a, &store_path(&dir)).unwrap();

        let b = DecoderStore::new();
        restore_from(&b, &dir);
        let jobs = &b.snapshot().kinds["jobs"];
        assert_eq!(jobs.current["cap-1"].data["status"], "done");
        assert_eq!(jobs.current["cap-2"].data["status"], "interrupted");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn missing_file_is_empty_and_corrupt_file_is_moved_aside() {
        let dir = tmpdir("bad");
        let path = store_path(&dir);
        assert!(load(&path).unwrap().is_none());
        std::fs::write(&path, b"{not json").unwrap();
        assert!(load(&path).is_err());
        assert!(!path.exists(), "corrupt file renamed out of the way");
        assert!(path.with_extension("json.corrupt").exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
