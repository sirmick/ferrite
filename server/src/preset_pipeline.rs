//! Preset-driven pipeline — drives the Rust `Runtime` loaded from a
//! flowgraph doc, bridging its auto-inserted `WsBridgeTx` blocks onto
//! the shared server-side [`FrameBus`].
//!
//! Flow: parse doc → `split_for_environment(.., Node)` → load runtime
//! → find every bridge-Tx in the split doc and attach a shared
//! [`BroadcastSink`] → `init` → `start` → tick on an interval until
//! the shutdown signal fires. Cross-env `stream_id`s are chosen by
//! `env_split` from `CROSS_ENV_STREAM_BASE` (1000+); the node half
//! and browser half agree on those ids without any negotiation.
//!
//! [`PresetMount`] bundles the shared [`FrameBus`] with the
//! [`PresetHandle`] so [`AppState`] can own both in one slot — the
//! handle keeps the runtime alive, the bus is what `/ws/preset`
//! subscribes to.
//!
//! [`AppState`]: crate::app_state::AppState
//! [`FrameBus`]: crate::frame_bus::FrameBus

use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Context, Result};
use ferrite_blocks::ws_bridge::{BridgeSink, WsBridgeTx, WsBridgeTxEvents, WsBridgeTxFftU8};
use ferrite_blocks::{SoapyReadback, SoapySource};
use ferrite_runtime::SOURCE_ID;
use ferrite_runtime::{
    split_for_environment, Environment, FlowgraphDoc, InventorySpecRegistry, ReconfigurePlan,
    Runtime, DEFAULT_FRAMES_HINT,
};
use tokio::{
    sync::{oneshot, Mutex},
    task::JoinHandle,
};

use crate::{bridge_sink::BroadcastSink, frame_bus::FrameBus};

/// Handle to a running preset pipeline. Callers can either drop the
/// handle (which cancels the runtime task via the `oneshot` sender
/// closing) or call [`PresetHandle::shutdown`] for a graceful signal
/// and join that surfaces any tick error. The binary today relies on
/// the drop path via `PresetMount`; the explicit `shutdown` is
/// reserved for an axum graceful-shutdown hook.
#[allow(dead_code)] // fields held for drop semantics
pub struct PresetHandle {
    shutdown: Option<oneshot::Sender<()>>,
    join: JoinHandle<Result<()>>,
}

/// Preset-mode slot stored inside [`AppState`]. Owns the runtime task
/// via [`PresetHandle`] and keeps a handle on the shared bridge sink
/// so `reconfigure` can re-attach it to fresh bridge-Tx instances
/// after a rebuild.
///
/// [`AppState`]: crate::app_state::AppState
pub struct PresetMount {
    pub handle: PresetHandle,
    /// Shared with the driver task so HTTP handlers can call
    /// [`Self::reconfigure`] between ticks. The mutex is held only for
    /// the duration of a tick (or a reconfigure), so contention is low.
    runtime: Arc<Mutex<Runtime>>,
    /// Held so [`Self::reconfigure`] can re-attach it to the fresh
    /// bridge-Tx instances that a rebuild produces — the sink on the
    /// old instances doesn't carry over. One sink serves every Tx
    /// block regardless of port type; the sink discriminates by the
    /// `Frame` variant it receives on each push.
    bridge_sink: Arc<dyn BridgeSink>,
}

impl PresetMount {
    /// Apply a new cross-env preset in place. The doc is carved for
    /// [`Environment::Node`] (matching the original load path), diffed
    /// against the applied doc, and swapped in atomically — with the
    /// same rollback guarantees as [`Runtime::reconfigure`].
    ///
    /// After a successful swap the shared broadcast sink is
    /// re-attached to every bridge-Tx in the new graph so frames keep
    /// flowing to `/ws/preset` without a client reconnect.
    pub async fn reconfigure(&self, new_doc: &FlowgraphDoc) -> Result<ReconfigurePlan> {
        let node_half = split_for_environment(new_doc, Environment::Node, &InventorySpecRegistry)
            .map_err(|e| anyhow!("env_split: {e}"))?;
        let mut rt = self.runtime.lock().await;
        let plan = rt.reconfigure(&node_half).context("runtime reconfigure")?;
        if plan.is_noop() {
            return Ok(plan);
        }
        attach_bridge_sinks(&mut rt, &node_half, &self.bridge_sink)?;
        Ok(plan)
    }

    /// Try to apply `delta` to block `id` via the live path
    /// (`Block::apply_live_params`). On `Ok(false)` from the block, the
    /// runtime falls back to a full rebuild — which here means we have
    /// to attach bridge sinks again, same as [`Self::reconfigure`].
    pub async fn live_reconfigure_block(
        &self,
        id: &str,
        delta: serde_json::Value,
    ) -> Result<ReconfigurePlan> {
        let mut rt = self.runtime.lock().await;
        let plan = rt
            .live_reconfigure_block(id, delta)
            .context("runtime live_reconfigure_block")?;
        // The fallback path (full rebuild) leaves bridges detached; the
        // live path doesn't touch them. Re-attach unconditionally — it's
        // idempotent and keeps the call simple.
        if let Some(doc) = rt.applied_doc().cloned() {
            attach_bridge_sinks(&mut rt, &doc, &self.bridge_sink)?;
        }
        Ok(plan)
    }

    /// Query the live driver state for the `src` block. Returns `None`
    /// when the source is not a `SoapySource` (software sources like
    /// `SineSource` have no hardware state to read back) or when the
    /// block is absent. Used by the `/api/source` route to return the
    /// post-apply device state so the UI's `params` reflect reality
    /// rather than the user's last optimistic write.
    pub async fn source_readback(&self) -> Option<SoapyReadback> {
        let mut rt = self.runtime.lock().await;
        rt.block_typed::<SoapySource>(SOURCE_ID)
            .map(|b| b.readback())
    }
}

impl PresetHandle {
    /// Signal the pipeline task to stop, then await its join. Returns
    /// the task's final `Result` so tick errors surface to the caller.
    #[allow(dead_code)] // public API reserved for graceful shutdown hook
    pub async fn shutdown(mut self) -> Result<()> {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        match self.join.await {
            Ok(inner) => inner,
            Err(join_err) => Err(anyhow!("preset task join: {join_err}")),
        }
    }
}

/// Build a runtime from `doc` (treated as a cross-env preset) and
/// spawn its tick loop. The doc is carved for [`Environment::Node`]
/// before load — blocks pinned to browser are stripped and their
/// inbound wires get an auto-inserted `WsBridgeTx`.
///
/// `tick_period` controls how often `Runtime::tick` is driven. A
/// reasonable value is `frames_hint / sample_rate_hz`; callers that
/// don't have a source rate can pass a short fixed interval and rely
/// on the runtime's internal back-pressure (idle ticks are cheap).
pub fn spawn_preset(
    doc: &FlowgraphDoc,
    frames: FrameBus,
    tick_period: Duration,
) -> Result<PresetMount> {
    let node_half = split_for_environment(doc, Environment::Node, &InventorySpecRegistry)
        .map_err(|e| anyhow!("env_split: {e}"))?;

    let mut runtime = Runtime::load_doc(
        &node_half,
        Environment::Node,
        DEFAULT_FRAMES_HINT,
        tick_period,
    )
    .context("runtime load_doc")?;

    let bridge_sink: Arc<dyn BridgeSink> = Arc::new(BroadcastSink::new(frames));
    attach_bridge_sinks(&mut runtime, &node_half, &bridge_sink)?;

    runtime.init().context("runtime init")?;
    runtime.start().context("runtime start")?;

    let runtime = Arc::new(Mutex::new(runtime));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let join = tokio::spawn(drive(Arc::clone(&runtime), tick_period, shutdown_rx));
    Ok(PresetMount {
        handle: PresetHandle {
            shutdown: Some(shutdown_tx),
            join,
        },
        runtime,
        bridge_sink,
    })
}

/// Walk `node_half` and attach the shared broadcast sink to every
/// bridge-Tx block. Auto-inserted bridges from `env_split` and
/// author-written bridges are treated the same — both are just blocks
/// whose `type_name` matches. Every bridge-Tx type is listed here so
/// adding a new one (e.g. `WsBridgeTxRealF32`) is a single arm.
fn attach_bridge_sinks(
    runtime: &mut Runtime,
    node_half: &FlowgraphDoc,
    sink: &Arc<dyn BridgeSink>,
) -> Result<()> {
    for (id, decl) in &node_half.blocks {
        match decl.type_name.as_str() {
            "WsBridgeTx" => {
                let tx = runtime.block_typed::<WsBridgeTx>(id).ok_or_else(|| {
                    anyhow!("runtime is missing expected WsBridgeTx {id:?} after load")
                })?;
                tx.attach_sink(Arc::clone(sink));
            }
            "WsBridgeTxFftU8" => {
                let tx = runtime.block_typed::<WsBridgeTxFftU8>(id).ok_or_else(|| {
                    anyhow!("runtime is missing expected WsBridgeTxFftU8 {id:?} after load")
                })?;
                tx.attach_sink(Arc::clone(sink));
            }
            "WsBridgeTxEvents" => {
                let tx = runtime.block_typed::<WsBridgeTxEvents>(id).ok_or_else(|| {
                    anyhow!("runtime is missing expected WsBridgeTxEvents {id:?} after load")
                })?;
                tx.attach_sink(Arc::clone(sink));
            }
            _ => {}
        }
    }
    Ok(())
}

async fn drive(
    runtime: Arc<Mutex<Runtime>>,
    tick_period: Duration,
    shutdown: oneshot::Receiver<()>,
) -> Result<()> {
    let mut interval = tokio::time::interval(tick_period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    // 1 Hz flow-table probe — dumps one JSON-encoded `DiagSnapshot` via
    // tracing so an operator tailing ferrited stdout (or the browser-side
    // Flow tab) can see per-block sample throughput, process-time, and
    // ring fill without instrumenting anything else. Logged at INFO
    // with target="flowdiag" so `RUST_LOG=flowdiag=info` isolates it.
    let mut diag_interval = tokio::time::interval(Duration::from_secs(1));
    diag_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate tick so the first sample covers a real 1-sec
    // window rather than the sliver between `start` and now.
    diag_interval.tick().await;
    // Previous cumulative `process_ns` per block, keyed by block id.
    // Lets the reporter emit a per-block "% of wall-clock spent in
    // process()" alongside the raw JSON — the same thing the UI will
    // compute from successive JSON snapshots, but pre-baked into the log
    // line so an operator doesn't have to diff two JSON blobs by hand.
    let mut prev_ns: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut prev_instant = std::time::Instant::now();

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::debug!("preset pipeline shutdown");
                let mut rt = runtime.lock().await;
                let _ = rt.stop();
                return Ok(());
            }
            _ = interval.tick() => {
                let mut rt = runtime.lock().await;
                if let Err(err) = rt.tick() {
                    tracing::error!(?err, "preset tick failed");
                    let _ = rt.stop();
                    return Err(err);
                }
            }
            _ = diag_interval.tick() => {
                // Refresh rate propagation before snapshotting so a
                // live source-rate change reflects in downstream
                // blocks' counters this tick. Cheap — rate-stable
                // blocks no-op in their `update_rates`.
                {
                    let mut rt = runtime.lock().await;
                    if let Err(err) = rt.refresh_rates() {
                        tracing::warn!(?err, "refresh_rates failed");
                    }
                    // SoapySource stall watchdog: if the reader thread
                    // hasn't pushed samples in > 2 s the driver is
                    // likely hung (common with RTL-SDR USB glitches /
                    // HackRF resets). Log once per second we're over
                    // the threshold so operators see it immediately
                    // without rooting through flowdiag counters.
                    if let Some(src) =
                        rt.block_typed::<ferrite_blocks::SoapySource>(SOURCE_ID)
                    {
                        if let Some(stalled) = src.stalled_ns() {
                            if stalled > 2_000_000_000 {
                                #[allow(clippy::cast_precision_loss)]
                                let secs = stalled as f64 / 1e9;
                                tracing::warn!(
                                    target: "flowdiag::node",
                                    stalled_secs = secs,
                                    "SoapySource reader hasn't delivered in {:.1}s — driver likely hung",
                                    secs,
                                );
                            }
                        }
                    }
                }
                let rt = runtime.lock().await;
                let snap = rt.diag_snapshot();
                drop(rt);
                let now = std::time::Instant::now();
                let window_ns = now.duration_since(prev_instant).as_nanos() as u64;
                prev_instant = now;
                // Per-block delta process_ns → % of wall-clock in a
                // short summary line. Skip on the very first sample
                // where prev is empty (no delta yet).
                if window_ns > 0 {
                    let mut parts = Vec::with_capacity(snap.blocks.len());
                    for b in &snap.blocks {
                        let prev = prev_ns.get(&b.id).copied().unwrap_or(b.process_ns_cum);
                        let delta = b.process_ns_cum.saturating_sub(prev);
                        prev_ns.insert(b.id.clone(), b.process_ns_cum);
                        #[allow(clippy::cast_precision_loss)]
                        let pct = (delta as f64) * 100.0 / (window_ns as f64);
                        if pct >= 0.1 {
                            parts.push(format!("{}={:.1}%", b.id, pct));
                        }
                    }
                    if !parts.is_empty() {
                        tracing::info!(target: "flowdiag::node", "flowcpu side=node {}", parts.join(" "));
                    }
                }
                // Canonical flow snapshot line. Same format the browser
                // runner emits via `postDiag`; the browser-side
                // `parseFlowdiagLine` picks either up with one regex.
                // Tagged `flowdiag::node` so it can be muted separately
                // from browser-side `flowdiag::browser`.
                match serde_json::to_string(&snap) {
                    Ok(json) => tracing::info!(target: "flowdiag::node", "flowdiag side=node {json}"),
                    Err(err) => tracing::warn!(?err, "flowdiag serialize"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::spawn_preset;
    use crate::frame_bus::FrameBus;
    use ferrite_blocks::frame::Frame;
    use ferrite_runtime::FlowgraphDoc;
    use std::time::Duration;

    const CROSS_ENV_DOC: &str = r#"{
        "name": "test_cross_env",
        "environments": ["node", "browser"],
        "blocks": {
            "src":  { "type": "SineSource", "placement": "node",
                      "params": { "rate_hz": 1000.0, "tone_freq_abs_hz": 100.0,
                                  "center_freq_hz": 0.0, "amplitude": 0.5 } },
            "sink": { "type": "Decimator", "placement": "browser",
                      "params": { "factor": 2, "num_taps": 17,
                                  "cutoff_normalized": 0.2 } }
        },
        "wires": [["src.out", "sink.in"]]
    }"#;

    #[tokio::test]
    async fn crossing_emits_iq_f32_on_allocated_stream_id() {
        let doc: FlowgraphDoc = serde_json::from_str(CROSS_ENV_DOC).unwrap();
        let frames = FrameBus::new();
        let mut rx = frames.subscribe(32);
        let mount = spawn_preset(&doc, frames, Duration::from_millis(5)).unwrap();
        // First crossing gets CROSS_ENV_STREAM_BASE = 1000.
        let bytes = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("a frame within 1s")
            .expect("recv ok");
        match Frame::from_postcard(&bytes).unwrap() {
            Frame::IqF32 {
                stream_id, payload, ..
            } => {
                assert_eq!(stream_id, 1000);
                // env_split now sets `min_samples_per_frame: 4096` on
                // auto-inserted bridges — the scheduler ticks at kHz
                // rates and one frame per tick saturates the browser's
                // WS decoder. The batch flushes on the first tick whose
                // accumulator reaches the threshold, which for a
                // 1024-sample ticker lands at 4×1024 = 4096 samples.
                assert_eq!(payload.len(), 4096 * 8);
            }
            other => panic!("expected IqF32 frame, got {other:?}"),
        }
        mount.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn node_only_preset_runs_without_bridges() {
        // No cross-env crossings → no bridge → no frames on the tx,
        // but the runtime must still init + tick cleanly.
        let doc: FlowgraphDoc = serde_json::from_str(
            r#"{
                "name": "t",
                "environments": ["node"],
                "blocks": {
                    "src": { "type": "SineSource", "placement": "node" }
                },
                "wires": []
            }"#,
        )
        .unwrap();
        let frames = FrameBus::new();
        let mut rx = frames.subscribe(4);
        let mount = spawn_preset(&doc, frames, Duration::from_millis(5)).unwrap();
        // Give the task a couple of ticks to run.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            rx.try_recv().is_err(),
            "no bridges → no WS frames should be emitted",
        );
        mount.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_joins_cleanly() {
        let doc: FlowgraphDoc = serde_json::from_str(CROSS_ENV_DOC).unwrap();
        let frames = FrameBus::new();
        let _rx = frames.subscribe(8);
        let mount = spawn_preset(&doc, frames, Duration::from_millis(5)).unwrap();
        let t0 = std::time::Instant::now();
        mount.handle.shutdown().await.unwrap();
        // A clean shutdown returns promptly — well under the per-tick
        // period × spurious-drift margin. 500ms is lots of headroom.
        assert!(t0.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn reconfigure_swaps_doc_and_keeps_bridge_flowing() {
        // Initial doc: SineSource(0.5) → Decimator (browser). After
        // reconfigure, amplitude drops to 0.1 — frames keep arriving on
        // the same stream id with no reconnect.
        let doc: FlowgraphDoc = serde_json::from_str(CROSS_ENV_DOC).unwrap();
        let frames = FrameBus::new();
        let mut rx = frames.subscribe(32);
        let mount = spawn_preset(&doc, frames, Duration::from_millis(5)).unwrap();

        // Drain at least one frame so we know the bridge is alive.
        let _ = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("pre-reconfigure frame within 1s")
            .expect("recv ok");

        let new_doc: FlowgraphDoc = serde_json::from_str(
            r#"{
                "name": "test_cross_env",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":  { "type": "SineSource", "placement": "node",
                              "params": { "rate_hz": 1000.0, "tone_freq_abs_hz": 100.0,
                                          "center_freq_hz": 0.0, "amplitude": 0.1 } },
                    "sink": { "type": "Decimator", "placement": "browser",
                              "params": { "factor": 2, "num_taps": 17,
                                          "cutoff_normalized": 0.2 } }
                },
                "wires": [["src.out", "sink.in"]]
            }"#,
        )
        .unwrap();
        let plan = mount.reconfigure(&new_doc).await.unwrap();
        assert!(!plan.is_noop());

        // Drain any in-flight pre-reconfigure frames so the next recv is
        // guaranteed post-swap.
        while rx.try_recv().is_ok() {}

        let bytes = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("post-reconfigure frame within 1s")
            .expect("recv ok");
        match Frame::from_postcard(&bytes).unwrap() {
            Frame::IqF32 { stream_id, .. } => {
                // Same bridge pair — same stream id.
                assert_eq!(stream_id, 1000);
            }
            other => panic!("expected IqF32 frame, got {other:?}"),
        }
        mount.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reconfigure_noop_returns_empty_plan() {
        let doc: FlowgraphDoc = serde_json::from_str(CROSS_ENV_DOC).unwrap();
        let frames = FrameBus::new();
        let _rx = frames.subscribe(8);
        let mount = spawn_preset(&doc, frames, Duration::from_millis(5)).unwrap();
        let plan = mount.reconfigure(&doc).await.unwrap();
        assert!(plan.is_noop());
        mount.handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn reconfigure_rollback_preserves_prior_stream() {
        // Apply a new doc that will fail to load (unknown block type).
        // After the failed reconfigure, frames must still flow on the
        // original pipeline — rollback is end-to-end.
        let doc: FlowgraphDoc = serde_json::from_str(CROSS_ENV_DOC).unwrap();
        let frames = FrameBus::new();
        let mut rx = frames.subscribe(32);
        let mount = spawn_preset(&doc, frames, Duration::from_millis(5)).unwrap();
        // Wait for one frame first.
        let _ = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("pre-reconfigure frame within 1s")
            .expect("recv ok");
        // Unknown type on the *node* side so `env_split` doesn't have
        // a chance to strip it before instantiate — the node half mount
        // is what we're testing rollback on, and env_split now tolerates
        // unknown types with explicit non-env placements (browser-side
        // NativeOnly blocks get pruned without the inventory lookup).
        let bad_doc: FlowgraphDoc = serde_json::from_str(
            r#"{
                "name": "test_cross_env",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":  { "type": "NotAThing",   "placement": "node" },
                    "sink": { "type": "Decimator",   "placement": "browser" }
                },
                "wires": [["src.out", "sink.in"]]
            }"#,
        )
        .unwrap();
        let err = mount.reconfigure(&bad_doc).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("NotAThing") || msg.contains("env_split"),
            "unexpected err: {msg}"
        );
        // Flush + wait for a fresh frame — the original graph should be
        // unaffected.
        while rx.try_recv().is_ok() {}
        let _ = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("post-rollback frame within 1s")
            .expect("recv ok");
        mount.handle.shutdown().await.unwrap();
    }
}
