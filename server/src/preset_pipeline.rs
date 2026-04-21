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
use ferrite_blocks::ws_bridge::{BridgeSink, WsBridgeTx, WsBridgeTxFftU8};
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

    let mut runtime = Runtime::load_doc(&node_half, Environment::Node, DEFAULT_FRAMES_HINT)
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
                // SineSource defaults to DEFAULT_FRAMES_HINT (1024)
                // samples per tick; each complex sample is 8 bytes.
                assert_eq!(payload.len(), 1024 * 8);
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
        let bad_doc: FlowgraphDoc = serde_json::from_str(
            r#"{
                "name": "test_cross_env",
                "environments": ["node", "browser"],
                "blocks": {
                    "src":  { "type": "SineSource", "placement": "node" },
                    "sink": { "type": "NotAThing",   "placement": "browser" }
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
