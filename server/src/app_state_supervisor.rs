//! Radio supervisor and the `health` record.
//!
//! One task per daemon, ticking at 1 Hz:
//!
//! - **Health.** Folds a `health` record (Replace kind, `ferrite/state/
//!   health`) that answers "is what I asked for what I'm getting?": radio
//!   liveness, requested vs actual LO, channel shift and listen frequency,
//!   preset-declared vs configured vs actual sample rate, and the warn
//!   lines decoders/drivers logged since the graph loaded (rate
//!   mismatches, snapped rates, stalls). `issues` lists, in plain words,
//!   everything that isn't as requested; `ok` is `issues.is_empty()`.
//! - **Recovery.** A `SoapySource` whose reader died, or that hasn't
//!   delivered for [`LOST_STALL_MS`], is a lost radio. While the operator
//!   still wants the pipeline running, the supervisor cycles it — stop
//!   (which releases a remote handle), back off, start — until samples
//!   flow again. Every step lands on the activity log, so the UI and an
//!   agent see "radio lost … reconnecting … recovered" without polling.
//!
//! The supervisor never calls into the driver itself: liveness comes from
//! the reader's lock-free counters and driver state from the `readback`
//! record the diag tick already folds, so a dead remote can't stall it.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::Duration;

use ferrite_blocks::{soapy_retry, SourceHealth};
use ferrite_runtime::SOURCE_ID;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::AppState;
use crate::log_stream::LogEntry;

/// Supervisor tick.
const TICK: Duration = Duration::from_secs(1);
/// No delivery for this long on a running radio ⇒ lost.
pub const LOST_STALL_MS: u64 = 5_000;
/// A freshly started radio that never delivers within this ⇒ lost.
const NEVER_DELIVERED_MS: u64 = 10_000;
/// After a recovery start, how long samples get to show up.
const VERIFY_WINDOW: Duration = Duration::from_secs(8);
/// Cap on warn lines kept in the record.
const WARN_CAP: usize = 10;

/// Backoff before recovery attempt `n` (1-based): 1, 2, 4, 8, then 10 s
/// for as long as the operator wants the radio. A refused connect is
/// cheap, and a sweep shouldn't sit idle long after the host is back.
fn backoff(attempt: u32) -> Duration {
    const STEPS: [u64; 5] = [1, 2, 4, 8, 10];
    let i = usize::try_from(attempt.saturating_sub(1)).unwrap_or(usize::MAX);
    Duration::from_secs(STEPS[i.min(STEPS.len() - 1)])
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RadioPhase {
    /// No hardware source running.
    #[default]
    Idle,
    /// Delivering samples.
    Ok,
    /// Detected dead; recovery about to start.
    Lost,
    /// Cycling the pipeline to get it back.
    Recovering,
}

/// The `radio` section of the `health` record.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RadioState {
    pub state: RadioPhase,
    /// Unix ms of the last phase change.
    pub since_ms: u64,
    /// Recovery attempts in the current outage.
    pub attempts: u32,
    /// Why the radio was declared lost, or the last attempt's failure.
    pub last_error: Option<String>,
    /// Outages recovered since the daemon started.
    pub recoveries: u32,
}

#[derive(Debug, Clone, Serialize)]
struct WarnItem {
    target: String,
    message: String,
    count: u32,
    last_at_ms: u64,
    #[serde(skip)]
    key: String,
}

/// Warn/error lines from decoders, the flow diagnostics and the driver,
/// deduplicated (digits folded so "stalled 2.3 s" / "stalled 2.6 s" are
/// one entry) and cleared when the graph epoch moves.
#[derive(Default)]
pub(super) struct Warnings {
    epoch: u64,
    items: VecDeque<WarnItem>,
    /// Consecutive ticks the operator wanted a hardware pipeline that
    /// wasn't there. One tick can be a `start` mid-flight; three is a
    /// start that failed on an unreachable remote.
    wanted_absent_ticks: u32,
    /// Unix ms the current epoch was first seen. Lines logged before it
    /// belong to the previous graph even if they're read late (the log
    /// receiver isn't drained while a recovery runs).
    epoch_since_ms: u64,
}

impl Warnings {
    /// Driver chatter that is normal operation, not a problem: the
    /// kernel capping SoapyRemote's socket buffers, Avahi/IP discovery
    /// noise at enumerate.
    const BENIGN: &'static [&'static str] = &["resize socket buffer", "Unable to scan", "Avahi"];

    /// The supervisor's own lines go to the `radio` target (not
    /// collected here): they describe the outage `radio` already
    /// reports, and they'd be read after recovery bumps the epoch.
    fn wanted(e: &LogEntry) -> bool {
        matches!(e.level.as_str(), "WARN" | "ERROR")
            && (e.target.starts_with("decoder::")
                || e.target.starts_with("flowdiag")
                || e.target == "driver")
            && !Self::BENIGN.iter().any(|b| e.message.contains(b))
    }

    fn key(e: &LogEntry) -> String {
        let folded: String = e
            .message
            .chars()
            .map(|c| if c.is_ascii_digit() { '#' } else { c })
            .collect();
        format!("{}|{folded}", e.target)
    }

    fn sync_epoch(&mut self, epoch: u64) {
        if epoch != self.epoch {
            self.epoch = epoch;
            self.epoch_since_ms = now_ms();
            self.items.clear();
        }
    }

    fn push(&mut self, e: &LogEntry, epoch: u64) {
        self.sync_epoch(epoch);
        if !Self::wanted(e) || e.at_ms < self.epoch_since_ms {
            return;
        }
        let key = Self::key(e);
        if let Some(pos) = self.items.iter().position(|w| w.key == key) {
            if let Some(mut w) = self.items.remove(pos) {
                w.count += 1;
                w.last_at_ms = e.at_ms;
                w.message.clone_from(&e.message);
                self.items.push_back(w);
            }
            return;
        }
        self.items.push_back(WarnItem {
            target: e.target.clone(),
            message: e.message.clone(),
            count: 1,
            last_at_ms: e.at_ms,
            key,
        });
        while self.items.len() > WARN_CAP {
            self.items.pop_front();
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

async fn recv_log(rx: &mut Option<broadcast::Receiver<LogEntry>>) -> Option<LogEntry> {
    match rx {
        Some(r) => loop {
            match r.recv().await {
                Ok(e) => return Some(e),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        },
        None => std::future::pending().await,
    }
}

/// Why `h` means the radio is gone, if it does.
#[must_use]
pub fn lost_reason(h: &SourceHealth) -> Option<String> {
    if h.reader_ended {
        let why = h.reader_error.as_deref().unwrap_or("read failed");
        return Some(format!("radio stream ended: {why}"));
    }
    match h.stalled_ms {
        Some(ms) if ms > LOST_STALL_MS => {
            #[allow(clippy::cast_precision_loss)]
            let s = ms as f64 / 1000.0;
            Some(format!("no samples from the radio for {s:.1} s"))
        }
        None if h.age_ms > NEVER_DELIVERED_MS => {
            Some("the radio has not delivered a sample since the pipeline started".into())
        }
        _ => None,
    }
}

/// Turn a raw start/patch failure into something an operator can act
/// on. Remote transport failures name the endpoint.
#[must_use]
pub fn describe_radio_error(raw: &str, args: &str) -> String {
    if soapy_retry::is_remote_transport_error(raw) {
        let at = soapy_retry::remote_endpoint(args).unwrap_or_else(|| "the remote host".into());
        format!("remote radio at {at} isn't answering (SoapyRemote): {raw}")
    } else {
        raw.to_string()
    }
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|x| x.is_finite())
}

impl AppState {
    fn set_radio(&self, f: impl FnOnce(&mut RadioState)) {
        let mut g = self.inner.radio.lock().unwrap_or_else(|e| e.into_inner());
        let before = g.state;
        f(&mut g);
        if g.state != before {
            g.since_ms = now_ms();
        }
    }

    fn radio_snapshot(&self) -> RadioState {
        self.inner
            .radio
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn wants_running(&self) -> bool {
        self.inner.run_intent.load(Ordering::Relaxed)
    }

    /// One line on the activity log (the AI panel transcript +
    /// `ferrite/state/activity`) and the `ai::activity` tracing target.
    fn note_activity(&self, summary: &str) {
        tracing::info!(target: "ai::activity", command = "radio", "{summary}");
        self.inner.decoder_store.apply(
            "activity",
            json!({
                "summary": summary,
                "command": "radio",
                "method": "",
                "path": "",
                "status": 0,
                "note": "",
            }),
        );
    }

    async fn mount_health(&self) -> (bool, Option<SourceHealth>, Option<f64>) {
        let pipeline = self.inner.pipeline.lock().await;
        match pipeline.as_ref() {
            Some(m) => (true, m.source_health().await, m.chan_shift_hz().await),
            None => (false, None, None),
        }
    }

    /// Recompute the RF context stamped on decoder records: where the
    /// radio is listening right now (requested LO + live channel shift)
    /// and which preset. `None` while stopped. Called on every tune /
    /// source patch / VFO move and once a second by the supervisor.
    pub(crate) async fn refresh_rf_ctx(&self) {
        self.update_rf_ctx().await;
        // Every external caller is a mutation: let the health record
        // catch up now rather than on the next tick.
        self.inner.health_kick.notify_one();
    }

    async fn update_rf_ctx(&self) {
        let (running, _, shift) = self.mount_health().await;
        if !running {
            self.inner.decoder_store.set_ctx(None);
            return;
        }
        let cfg_center = num(self
            .inner
            .source_config
            .read()
            .await
            .params
            .get("center_freq_hz"));
        let lo = cfg_center.or_else(|| {
            num(self
                .inner
                .decoder_store
                .snapshot_kind("readback")
                .and_then(|k| k.current.get("current").map(|r| r.data.clone()))
                .as_ref()
                .and_then(|r| r.get("center_freq_hz")))
        });
        let preset = self.inner.preset_doc.read().await.name.clone();
        self.inner.decoder_store.set_ctx(Some(json!({
            "listen_hz": lo.map(|c| c + shift.unwrap_or(0.0)),
            "lo_hz": lo,
            "chan_shift_hz": shift,
            "preset": preset,
        })));
    }

    /// Build and fold the `health` record. Returns why the radio is lost
    /// when it is (and the operator still wants it running).
    pub(super) async fn publish_health(&self, warnings: &mut Warnings) -> Option<String> {
        warnings.sync_epoch(self.epoch());
        self.update_rf_ctx().await;
        let (running, src, shift) = self.mount_health().await;
        let cfg = self.inner.source_config.read().await.clone();
        let declared = num(self
            .inner
            .preset_doc
            .read()
            .await
            .blocks
            .get(SOURCE_ID)
            .and_then(|b| b.params.as_ref())
            .and_then(|p| p.get("sample_rate_hz")));
        let readback = self
            .inner
            .decoder_store
            .snapshot_kind("readback")
            .and_then(|k| k.current.get("current").map(|r| r.data.clone()));

        let hw = cfg.type_name == "SoapySource";
        let lost = match &src {
            Some(h) if self.wants_running() => lost_reason(h),
            // Wanted running, nothing running, hardware source: a start
            // failed on an unreachable remote (only those keep the
            // intent) — recover it like a mid-stream loss.
            None if self.wants_running() && !running && hw => {
                warnings.wanted_absent_ticks += 1;
                (warnings.wanted_absent_ticks >= 3)
                    .then(|| "the radio was unreachable when the pipeline started".into())
            }
            _ => None,
        };
        if running || !self.wants_running() {
            warnings.wanted_absent_ticks = 0;
        }
        // Phase bookkeeping for the steady states; recovery drives the rest.
        let phase = self.radio_snapshot().state;
        if !matches!(phase, RadioPhase::Lost | RadioPhase::Recovering) {
            let next = match (&src, &lost) {
                (Some(_), None) => RadioPhase::Ok,
                (Some(_), Some(_)) => RadioPhase::Lost,
                (None, _) => RadioPhase::Idle,
            };
            self.set_radio(|r| r.state = next);
        }
        let radio = self.radio_snapshot();

        let args = cfg
            .params
            .get("args")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let cfg_center = num(cfg.params.get("center_freq_hz"));
        let cfg_rate = num(cfg.params.get("sample_rate_hz"));
        let rb_center = num(readback.as_ref().and_then(|r| r.get("center_freq_hz")));
        let rb_rate = num(readback.as_ref().and_then(|r| r.get("sample_rate_hz")));
        let lo = rb_center.or(cfg_center);
        let listen = lo.map(|c| c + shift.unwrap_or(0.0));

        let mut issues: Vec<String> = Vec::new();
        match radio.state {
            RadioPhase::Lost | RadioPhase::Recovering => issues.push(format!(
                "radio {}: {}",
                if radio.state == RadioPhase::Lost {
                    "lost"
                } else {
                    "recovering"
                },
                radio.last_error.clone().unwrap_or_default()
            )),
            _ => {}
        }
        if let Some(why) = &lost {
            if radio.state == RadioPhase::Ok || radio.state == RadioPhase::Idle {
                issues.push(why.clone());
            }
        }
        if hw {
            if let (Some(d), Some(c)) = (declared, cfg_rate) {
                if (d - c).abs() > 0.5 {
                    issues.push(format!(
                        "the preset declares {d:.0} S/s but the source is configured for {c:.0} S/s"
                    ));
                }
            }
            if running {
                if let (Some(a), Some(c)) = (rb_rate, cfg_rate) {
                    if (a - c).abs() > 1.0 {
                        issues.push(format!(
                            "the radio runs at {a:.0} S/s, not the configured {c:.0} S/s (driver snapped it)"
                        ));
                    }
                }
                if let (Some(a), Some(c)) = (rb_center, cfg_center) {
                    if (a - c).abs() > 1.0 {
                        issues.push(format!(
                            "the radio's LO is at {a:.0} Hz, not the requested {c:.0} Hz"
                        ));
                    }
                }
            }
        }
        if !warnings.items.is_empty() {
            issues.push(format!(
                "{} decoder/driver warning(s) since this graph loaded — see `warnings`",
                warnings.items.len()
            ));
        }

        let pipeline = if running {
            "running"
        } else if radio.state == RadioPhase::Recovering {
            "recovering"
        } else {
            "stopped"
        };
        self.inner.decoder_store.apply(
            "health",
            json!({
                "ok": issues.is_empty(),
                "issues": issues,
                "pipeline": pipeline,
                "epoch": self.epoch(),
                "radio": {
                    "state": radio.state,
                    "since_ms": radio.since_ms,
                    "attempts": radio.attempts,
                    "last_error": radio.last_error,
                    "recoveries": radio.recoveries,
                    "source_type": cfg.type_name,
                    "remote": soapy_retry::remote_endpoint(&args),
                    "stream": src,
                },
                "tuning": {
                    "lo_requested_hz": cfg_center,
                    "lo_actual_hz": rb_center,
                    "chan_shift_hz": shift,
                    "listen_hz": listen,
                },
                "rate": {
                    "declared_hz": declared,
                    "configured_hz": cfg_rate,
                    "actual_hz": rb_rate,
                },
                "warnings": warnings.items.iter().collect::<Vec<_>>(),
            }),
        );
        lost
    }

    /// Start the supervisor task. Call once, after the event plane is up.
    pub fn spawn_supervisor(&self) {
        let me = self.clone();
        let mut logs = self.logs().map(crate::log_stream::LogBroadcast::subscribe);
        tokio::spawn(async move {
            let mut warnings = Warnings::default();
            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    e = recv_log(&mut logs) => match e {
                        Some(e) => warnings.push(&e, me.epoch()),
                        None => logs = None,
                    },
                    _ = tick.tick() => {
                        if let Some(reason) = me.publish_health(&mut warnings).await {
                            me.recover(reason, &mut warnings).await;
                        }
                    }
                    () = me.inner.health_kick.notified() => {
                        // A mutation landed: refresh `health` now. Loss
                        // detection stays on the steady tick.
                        let _ = me.publish_health(&mut warnings).await;
                    }
                }
            }
        });
    }

    /// Cycle a lost radio until it delivers again or the operator stops.
    async fn recover(&self, reason: String, warnings: &mut Warnings) {
        warnings.wanted_absent_ticks = 0;
        self.set_radio(|r| {
            r.state = RadioPhase::Lost;
            r.attempts = 0;
            r.last_error = Some(reason.clone());
        });
        tracing::warn!(target: "radio", %reason, "radio lost; recovering");
        self.note_activity(&format!("radio lost ({reason}) — reconnecting"));
        self.publish_health(warnings).await;

        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            if !self.wants_running() {
                self.end_recovery_stopped(warnings).await;
                return;
            }
            self.set_radio(|r| {
                r.state = RadioPhase::Recovering;
                r.attempts = attempt;
            });
            {
                // Serialise with source patches / preset loads.
                let _gate = self.inner.patch_gate.lock().await;
                self.stop_with_status("recovering").await;
            }
            self.publish_health(warnings).await;

            // Back off in slices so an operator stop ends this promptly.
            let wait = backoff(attempt);
            let deadline = tokio::time::Instant::now() + wait;
            while tokio::time::Instant::now() < deadline {
                if !self.wants_running() {
                    self.end_recovery_stopped(warnings).await;
                    return;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }

            let args = self
                .inner
                .source_config
                .read()
                .await
                .params
                .get("args")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let started = {
                let _gate = self.inner.patch_gate.lock().await;
                if !self.wants_running() {
                    self.end_recovery_stopped(warnings).await;
                    return;
                }
                self.start_inner().await
            };
            let failure = match started {
                Ok(()) => {
                    if self.verify_delivering().await {
                        self.set_radio(|r| {
                            r.state = RadioPhase::Ok;
                            r.attempts = 0;
                            r.last_error = None;
                            r.recoveries += 1;
                        });
                        let msg = format!(
                            "radio recovered after {attempt} attempt{}",
                            if attempt == 1 { "" } else { "s" }
                        );
                        tracing::info!(target: "radio", "{msg}");
                        self.note_activity(&msg);
                        self.publish_health(warnings).await;
                        return;
                    }
                    "reopened, but no samples arrived".to_string()
                }
                Err(e) => describe_radio_error(&format!("{e:#}"), &args),
            };
            tracing::warn!(target: "radio", attempt, error = %failure, "radio recovery attempt failed");
            if attempt == 1 || attempt.is_multiple_of(5) {
                self.note_activity(&format!(
                    "radio still unreachable after {attempt} attempt(s): {failure} — next try in {} s",
                    backoff(attempt + 1).as_secs()
                ));
            }
            self.set_radio(|r| r.last_error = Some(failure));
            self.publish_health(warnings).await;
        }
    }

    async fn end_recovery_stopped(&self, warnings: &mut Warnings) {
        self.set_radio(|r| {
            r.state = RadioPhase::Idle;
            r.attempts = 0;
        });
        if self.inner.pipeline.lock().await.is_none() {
            self.publish_status("stopped");
        }
        self.note_activity("radio recovery ended: the pipeline was stopped");
        self.publish_health(warnings).await;
    }

    /// After a recovery start: wait for real samples (a remote can accept
    /// the open and still never stream). Software sources pass at once.
    async fn verify_delivering(&self) -> bool {
        let deadline = tokio::time::Instant::now() + VERIFY_WINDOW;
        loop {
            let (running, src, _) = self.mount_health().await;
            match src {
                None => return running,
                Some(h) if h.reader_ended => return false,
                Some(h) if h.samples_pushed > 0 => return true,
                Some(_) => {}
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health(reader_ended: bool, stalled_ms: Option<u64>, age_ms: u64) -> SourceHealth {
        SourceHealth {
            remote: true,
            hung: false,
            reader_ended,
            reader_error: reader_ended.then(|| "SoapyRPCPacker::send() FAIL: Broken pipe".into()),
            stalled_ms,
            age_ms,
            samples_pushed: 1,
            overflow_drops: 0,
            ring_drops: 0,
            ring_gaps: 0,
        }
    }

    #[test]
    fn lost_on_reader_end_long_stall_or_never_delivering() {
        assert!(lost_reason(&health(true, Some(10), 60_000))
            .unwrap()
            .contains("Broken pipe"));
        assert!(lost_reason(&health(false, Some(LOST_STALL_MS + 1), 60_000)).is_some());
        assert!(lost_reason(&health(false, Some(200), 60_000)).is_none());
        assert!(
            lost_reason(&health(false, None, 2_000)).is_none(),
            "still starting"
        );
        assert!(lost_reason(&health(false, None, NEVER_DELIVERED_MS + 1)).is_some());
    }

    #[test]
    fn backoff_grows_then_caps() {
        let s: Vec<u64> = (1..=7).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(s, vec![1, 2, 4, 8, 10, 10, 10]);
    }

    #[test]
    fn remote_failures_name_the_endpoint() {
        let args = "driver=remote,remote=tcp://172.16.16.159:55132,remote:driver=sdrplay";
        let d = describe_radio_error("activate: SoapyRPCUnpacker: Connection refused", args);
        assert!(d.contains("tcp://172.16.16.159:55132"), "{d}");
        assert_eq!(describe_radio_error("bad gain", args), "bad gain");
    }

    fn entry(target: &str, level: &str, msg: &str) -> LogEntry {
        LogEntry {
            target: target.into(),
            level: level.into(),
            message: msg.into(),
            at_ms: now_ms() + 1_000,
        }
    }

    #[test]
    fn warnings_dedup_fold_digits_and_clear_on_epoch() {
        let mut w = Warnings::default();
        w.push(&entry("flowdiag::node", "WARN", "stalled 2.3 s"), 1);
        w.push(&entry("flowdiag::node", "WARN", "stalled 2.6 s"), 1);
        w.push(
            &entry("decoder::ft8", "WARN", "input rate doesn't match"),
            1,
        );
        w.push(&entry("decoder::ft8", "INFO", "slot drained"), 1);
        w.push(&entry("ferrited::routes", "WARN", "unrelated"), 1);
        w.push(
            &entry(
                "driver",
                "WARN",
                "StreamEndpoint resize socket buffer: set 1, got 2",
            ),
            1,
        );
        assert_eq!(w.items.len(), 2);
        assert_eq!(w.items[0].count, 2);
        assert_eq!(w.items[0].message, "stalled 2.6 s");
        w.push(&entry("decoder::ft8", "WARN", "x"), 2);
        assert_eq!(w.items.len(), 1, "a new epoch starts a fresh list");
        let mut late = entry("decoder::ft8", "WARN", "logged under the old graph");
        late.at_ms = 1;
        w.push(&late, 2);
        assert_eq!(w.items.len(), 1, "a line older than the epoch is dropped");
    }
}
