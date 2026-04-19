//! Preset-driven pipeline — drives the Rust `Runtime` loaded from a
//! flowgraph doc, bridging its auto-inserted `WsBridgeTx` blocks onto
//! the session's WebSocket `FrameTx`.
//!
//! Flow: parse doc → `split_for_environment(.., Node)` → load runtime
//! → find every `WsBridgeTx` in the split doc and attach a shared
//! [`BroadcastIqSink`] → `init` → `start` → tick on an interval until
//! the shutdown signal fires. Cross-env `stream_id`s are chosen by
//! `env_split` from `CROSS_ENV_STREAM_BASE` (1000+); the node half
//! and browser half agree on those ids without any negotiation.
//!
//! The CLI wiring (`--flowgraph <path>`) that instantiates this module
//! from `main.rs` lands in the next commit — this module stays
//! focused on the runtime-driving core so it can be exercised in
//! isolation.

// CLI hook (`--flowgraph <path>` in `main.rs`) lands in the next
// commit; until then the module's tests are the only live callers.
#![allow(dead_code)]

use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Context, Result};
use ferrite_blocks::ws_bridge::{IqBridgeSink, WsBridgeTx};
use ferrite_runtime::{
    split_for_environment, Environment, FlowgraphDoc, InventorySpecRegistry, Runtime,
    DEFAULT_FRAMES_HINT,
};
use tokio::{sync::oneshot, task::JoinHandle};

use crate::{bridge_sink::BroadcastIqSink, session::FrameTx};

/// Handle to a running preset pipeline. Drop-to-shut-down is
/// intentionally *not* wired — callers must call [`PresetHandle::shutdown`]
/// and await it to join the task cleanly.
pub struct PresetHandle {
    shutdown: Option<oneshot::Sender<()>>,
    join: JoinHandle<Result<()>>,
}

impl PresetHandle {
    /// Signal the pipeline task to stop, then await its join. Returns
    /// the task's final `Result` so tick errors surface to the caller.
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
    frames: FrameTx,
    tick_period: Duration,
) -> Result<PresetHandle> {
    let node_half = split_for_environment(doc, Environment::Node, &InventorySpecRegistry)
        .map_err(|e| anyhow!("env_split: {e}"))?;

    let mut runtime = Runtime::load_doc(&node_half, Environment::Node, DEFAULT_FRAMES_HINT)
        .context("runtime load_doc")?;

    let sink: Arc<dyn IqBridgeSink> = Arc::new(BroadcastIqSink::new(frames));
    // Attach the shared sink to every auto-inserted `WsBridgeTx`.
    // `env_split` names them `__bridge_tx_<sid>`; we filter by type so
    // the logic survives a future rename of the id convention.
    let bridge_ids: Vec<String> = node_half
        .blocks
        .iter()
        .filter(|(_, decl)| decl.type_name == "WsBridgeTx")
        .map(|(id, _)| id.clone())
        .collect();
    for id in &bridge_ids {
        let tx = runtime
            .block_typed::<WsBridgeTx>(id)
            .ok_or_else(|| anyhow!("runtime is missing expected WsBridgeTx {id:?} after load"))?;
        tx.attach_sink(Arc::clone(&sink));
    }

    runtime.init().context("runtime init")?;
    runtime.start().context("runtime start")?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let join = tokio::spawn(drive(runtime, tick_period, shutdown_rx));
    Ok(PresetHandle {
        shutdown: Some(shutdown_tx),
        join,
    })
}

async fn drive(
    mut runtime: Runtime,
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
                let _ = runtime.stop();
                return Ok(());
            }
            _ = interval.tick() => {
                if let Err(err) = runtime.tick() {
                    tracing::error!(?err, "preset tick failed");
                    let _ = runtime.stop();
                    return Err(err);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::spawn_preset;
    use crate::ws_frame::{decode, PayloadType};
    use ferrite_runtime::FlowgraphDoc;
    use std::time::Duration;
    use tokio::sync::broadcast;

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
        let (frames, mut rx) = broadcast::channel(32);
        let handle = spawn_preset(&doc, frames, Duration::from_millis(5)).unwrap();
        // First crossing gets CROSS_ENV_STREAM_BASE = 1000.
        let bytes = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("a frame within 1s")
            .expect("broadcast ok");
        let (header, payload) = decode(&bytes).unwrap();
        assert_eq!(header.payload_type, PayloadType::IqF32);
        assert_eq!(header.stream_id, 1000);
        // SineSource defaults to DEFAULT_FRAMES_HINT (1024) samples per
        // tick; each complex sample is 8 bytes.
        assert_eq!(payload.len(), 1024 * 8);
        handle.shutdown().await.unwrap();
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
        let (frames, mut rx) = broadcast::channel(4);
        let handle = spawn_preset(&doc, frames, Duration::from_millis(5)).unwrap();
        // Give the task a couple of ticks to run.
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            rx.try_recv().is_err(),
            "no bridges → no WS frames should be emitted",
        );
        handle.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_joins_cleanly() {
        let doc: FlowgraphDoc = serde_json::from_str(CROSS_ENV_DOC).unwrap();
        let (frames, _rx) = broadcast::channel(8);
        let handle = spawn_preset(&doc, frames, Duration::from_millis(5)).unwrap();
        let t0 = std::time::Instant::now();
        handle.shutdown().await.unwrap();
        // A clean shutdown returns promptly — well under the per-tick
        // period × spurious-drift margin. 500ms is lots of headroom.
        assert!(t0.elapsed() < Duration::from_millis(500));
    }
}
