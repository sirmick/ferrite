//! Preset-first server state.
//!
//! Ferrited is no longer a session manager; it runs exactly one
//! preset flowgraph at a time, and the source that feeds it lives in
//! its own mutable slot. Three artefacts: the preset JSON (what the
//! user plays back), the [`SourceConfig`] (what hardware the source
//! block talks to), and an optional [`PresetMount`] (the running
//! runtime). `start` composes preset+source, carves for `Node`, and
//! spawns; `stop` tears that back down without disturbing the
//! WebSocket fan-out.
//!
//! The broadcast [`FrameTx`] outlives any one pipeline instance, so
//! `/ws/preset` subscribers stay connected across start/stop cycles.

use std::{sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use ferrite_runtime::{
    compose_source, split_for_environment, Environment, FlowgraphDoc, InventorySpecRegistry,
    ReconfigurePlan, SourceConfig,
};
use tokio::sync::{broadcast, Mutex, RwLock};

use crate::preset_pipeline::{spawn_preset, PresetMount};

/// One UI-terminated sink in the active preset, paired with the
/// `stream_id` `env_split` allocates for it. Returned from
/// [`AppState::ui_sinks`] so the web client can subscribe to the right
/// stream without reimplementing the allocator.
#[derive(Debug, Clone, serde::Serialize)]
pub struct UiSink {
    /// `ui:<name>` — the sentinel name authored in the preset.
    pub name: String,
    /// Allocated by `env_split` from `CROSS_ENV_STREAM_BASE` (1000+) in
    /// doc wire order.
    pub stream_id: u32,
    /// `"IqF32"` or `"FftU8"` — the frame payload type the client
    /// should decode for this stream.
    pub payload_type: &'static str,
}

pub type FrameBytes = Arc<Vec<u8>>;
pub type FrameTx = broadcast::Sender<FrameBytes>;

/// Pipeline lifecycle. The server-side runtime is either running or
/// stopped; transitions go through [`AppState::start`] / [`stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStatus {
    Running,
    Stopped,
}

struct Inner {
    /// Shared broadcast channel. Survives start/stop cycles so a
    /// subscriber connected while stopped picks up frames the moment
    /// the pipeline spins back up.
    frames: FrameTx,
    preset_doc: RwLock<FlowgraphDoc>,
    source_config: RwLock<SourceConfig>,
    /// `Some` while the pipeline is running.
    pipeline: Mutex<Option<PresetMount>>,
    tick_period: Duration,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
    logs: Option<crate::log_stream::LogBroadcast>,
}

impl AppState {
    #[must_use]
    pub fn new(preset: FlowgraphDoc, source: SourceConfig, tick_period: Duration) -> Self {
        let (frames, _) = broadcast::channel(32);
        Self {
            inner: Arc::new(Inner {
                frames,
                preset_doc: RwLock::new(preset),
                source_config: RwLock::new(source),
                pipeline: Mutex::new(None),
                tick_period,
            }),
            logs: None,
        }
    }

    #[must_use]
    pub fn with_logs(mut self, logs: crate::log_stream::LogBroadcast) -> Self {
        self.logs = Some(logs);
        self
    }

    #[must_use]
    pub fn logs(&self) -> Option<&crate::log_stream::LogBroadcast> {
        self.logs.as_ref()
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<FrameBytes> {
        self.inner.frames.subscribe()
    }

    pub async fn status(&self) -> PipelineStatus {
        if self.inner.pipeline.lock().await.is_some() {
            PipelineStatus::Running
        } else {
            PipelineStatus::Stopped
        }
    }

    pub async fn get_flowgraph(&self) -> FlowgraphDoc {
        self.inner.preset_doc.read().await.clone()
    }

    pub async fn get_source(&self) -> SourceConfig {
        self.inner.source_config.read().await.clone()
    }

    /// Walk the current preset+source's node-half split and surface every
    /// `ui:<name>` sink with the `stream_id` env_split allocated for it.
    /// The client uses this to subscribe to the right frame streams
    /// without reimplementing the allocator.
    pub async fn ui_sinks(&self) -> Result<Vec<UiSink>> {
        let preset = self.inner.preset_doc.read().await.clone();
        let source = self.inner.source_config.read().await.clone();
        let composed =
            compose_source(&preset, &source).map_err(|e| anyhow!("compose preset+source: {e}"))?;
        let node_half = split_for_environment(&composed, Environment::Node, &InventorySpecRegistry)
            .map_err(|e| anyhow!("env_split: {e}"))?;
        let mut out = Vec::new();
        for (_, decl) in &node_half.blocks {
            let payload_type = match decl.type_name.as_str() {
                "WsBridgeTx" => "IqF32",
                "WsBridgeTxFftU8" => "FftU8",
                _ => continue,
            };
            let params = decl.params.as_ref().and_then(|p| p.as_object());
            let Some(params) = params else { continue };
            let Some(name) = params.get("ui_name").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(stream_id) = params.get("stream_id").and_then(|v| v.as_u64()) else {
                continue;
            };
            out.push(UiSink {
                name: name.to_string(),
                stream_id: stream_id as u32,
                payload_type,
            });
        }
        out.sort_by_key(|s| s.stream_id);
        Ok(out)
    }

    /// Apply a new preset. If the pipeline is running, reconfigures it
    /// in place (composed with the current source) and returns the
    /// plan; otherwise stores the doc and returns `None`.
    pub async fn patch_flowgraph(&self, new_doc: FlowgraphDoc) -> Result<Option<ReconfigurePlan>> {
        let source = self.inner.source_config.read().await.clone();
        let composed =
            compose_source(&new_doc, &source).map_err(|e| anyhow!("compose preset+source: {e}"))?;
        let mut pipeline = self.inner.pipeline.lock().await;
        let plan = if let Some(mount) = pipeline.as_mut() {
            Some(mount.reconfigure(&composed).await?)
        } else {
            None
        };
        *self.inner.preset_doc.write().await = new_doc;
        Ok(plan)
    }

    /// Apply a new source config. Same rules as [`patch_flowgraph`].
    pub async fn patch_source(&self, new_source: SourceConfig) -> Result<Option<ReconfigurePlan>> {
        let preset = self.inner.preset_doc.read().await.clone();
        let composed = compose_source(&preset, &new_source)
            .map_err(|e| anyhow!("compose preset+source: {e}"))?;
        let mut pipeline = self.inner.pipeline.lock().await;
        let plan = if let Some(mount) = pipeline.as_mut() {
            Some(mount.reconfigure(&composed).await?)
        } else {
            None
        };
        *self.inner.source_config.write().await = new_source;
        Ok(plan)
    }

    /// Start the pipeline if it isn't already running. Idempotent:
    /// re-calling while running is a no-op and returns `Ok(())`.
    pub async fn start(&self) -> Result<()> {
        let mut guard = self.inner.pipeline.lock().await;
        if guard.is_some() {
            return Ok(());
        }
        let preset = self.inner.preset_doc.read().await.clone();
        let source = self.inner.source_config.read().await.clone();
        let composed =
            compose_source(&preset, &source).map_err(|e| anyhow!("compose preset+source: {e}"))?;
        let mount = spawn_preset(&composed, self.inner.frames.clone(), self.inner.tick_period)?;
        *guard = Some(mount);
        Ok(())
    }

    /// Stop the running pipeline. Returns `true` if it was running,
    /// `false` if it was already stopped. Waits for the runtime task
    /// to join before returning so the caller knows the source device
    /// has been released.
    pub async fn stop(&self) -> bool {
        let mut guard = self.inner.pipeline.lock().await;
        let Some(mount) = guard.take() else {
            return false;
        };
        // Destructure to take ownership of the handle for a graceful
        // shutdown; the other fields drop when the block ends.
        let PresetMount { handle, .. } = mount;
        if let Err(err) = handle.shutdown().await {
            tracing::warn!(?err, "preset pipeline shutdown returned error");
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_blocks::frame::Frame;
    use serde_json::json;
    use std::time::Duration;

    fn test_preset() -> FlowgraphDoc {
        serde_json::from_value(json!({
            "name": "t",
            "environments": ["node", "browser"],
            "blocks": {
                "src":  { "type": "Source", "placement": "node",
                          "params": { "center_freq_hz": 0.0, "sample_rate_hz": 1000.0 } },
                "sink": { "type": "Decimator", "placement": "browser",
                          "params": { "factor": 2, "num_taps": 17,
                                      "cutoff_normalized": 0.2 } }
            },
            "wires": [["src.out", "sink.in"]]
        }))
        .unwrap()
    }

    fn test_source() -> SourceConfig {
        SourceConfig {
            type_name: "SineSource".into(),
            params: json!({
                "rate_hz": 1000.0,
                "tone_freq_abs_hz": 100.0,
                "center_freq_hz": 0.0,
                "amplitude": 0.5,
            }),
        }
    }

    #[tokio::test]
    async fn starts_stopped_and_transitions_on_start() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        assert_eq!(state.status().await, PipelineStatus::Stopped);
        state.start().await.unwrap();
        assert_eq!(state.status().await, PipelineStatus::Running);
        assert!(state.stop().await);
        assert_eq!(state.status().await, PipelineStatus::Stopped);
    }

    #[tokio::test]
    async fn start_is_idempotent() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        state.start().await.unwrap();
        state.start().await.unwrap();
        assert_eq!(state.status().await, PipelineStatus::Running);
        state.stop().await;
    }

    #[tokio::test]
    async fn stop_while_stopped_returns_false() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        assert!(!state.stop().await);
    }

    #[tokio::test]
    async fn subscribe_survives_restart() {
        // A subscriber obtained before start() must still receive
        // frames from a later start() — the broadcast channel outlives
        // any one pipeline instance.
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        let mut rx = state.subscribe();
        state.start().await.unwrap();
        let bytes = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("frame within 1s")
            .expect("recv ok");
        assert!(matches!(
            Frame::from_postcard(&bytes).unwrap(),
            Frame::IqF32 { .. }
        ));
        state.stop().await;
    }

    #[tokio::test]
    async fn patch_source_while_stopped_updates_without_reconfigure() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        let mut new_source = test_source();
        new_source.params =
            json!({ "rate_hz": 1000.0, "tone_freq_abs_hz": 200.0, "amplitude": 0.25 });
        let plan = state.patch_source(new_source.clone()).await.unwrap();
        assert!(plan.is_none(), "no pipeline to reconfigure");
        assert_eq!(state.get_source().await, new_source);
    }

    #[tokio::test]
    async fn patch_source_while_running_hot_reconfigures() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        state.start().await.unwrap();
        let mut new_source = test_source();
        new_source.params = json!({
            "rate_hz": 1000.0,
            "tone_freq_abs_hz": 250.0,
            "center_freq_hz": 0.0,
            "amplitude": 0.1,
        });
        let plan = state
            .patch_source(new_source)
            .await
            .expect("reconfigure ok");
        assert!(plan.is_some(), "running pipeline must return a plan");
        state.stop().await;
    }

    #[tokio::test]
    async fn ui_sinks_enumerates_fft_tap_stream_id() {
        // Preset: Source(node) → FFT(node) → LogMag(node) → ui:fft.
        // env_split allocates the ui:fft wire on crossing_index=0 →
        // stream_id=1000, payload_type=FftU8.
        let preset: FlowgraphDoc = serde_json::from_value(json!({
            "name": "t",
            "environments": ["node", "browser"],
            "blocks": {
                "src":    { "type": "Source", "placement": "node",
                            "params": { "center_freq_hz": 0.0, "sample_rate_hz": 1000.0 } },
                "fft":    { "type": "FFT", "placement": "node",
                            "params": { "size": 64, "window": "hann" } },
                "logmag": { "type": "LogMagU8", "placement": "node",
                            "params": { "size": 64, "floor_dbfs": -100.0,
                                        "ceil_dbfs": 0.0, "alpha": 0.3 } }
            },
            "wires": [
                ["src.out",    "fft.in"],
                ["fft.out",    "logmag.in"],
                ["logmag.out", "ui:fft"]
            ]
        }))
        .unwrap();
        let state = AppState::new(preset, test_source(), Duration::from_millis(5));
        let sinks = state.ui_sinks().await.unwrap();
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].name, "fft");
        assert_eq!(sinks[0].stream_id, 1000);
        assert_eq!(sinks[0].payload_type, "FftU8");
    }

    #[tokio::test]
    async fn patch_source_with_bad_source_leaves_config_unchanged() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        state.start().await.unwrap();
        let bad_source = SourceConfig {
            type_name: "NotARealBlock".into(),
            params: json!({}),
        };
        assert!(state.patch_source(bad_source).await.is_err());
        // Stored source is still the original.
        assert_eq!(state.get_source().await.type_name, "SineSource");
        state.stop().await;
    }
}
