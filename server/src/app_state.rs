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
//! The shared [`FrameBus`] outlives any one pipeline instance, so
//! `/ws/preset` subscribers stay connected across start/stop cycles.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{anyhow, Result};
use ferrite_blocks::{registry, SoapyReadback};
use ferrite_runtime::{
    apply_profile, compose_source, inject_narrow_fft_taps, split_for_environment, Environment,
    FlowgraphDoc, InventorySpecRegistry, Profile, ReconfigurePlan, SourceConfig, SOURCE_ID,
};
use tokio::sync::{mpsc, Mutex, RwLock};

use crate::{
    block_schema::BlockSchemaDto,
    device_cache::DeviceCache,
    frame_bus::{FrameBus, DEFAULT_SUBSCRIBER_CAPACITY},
    preset_pipeline::{spawn_preset, PresetMount},
};

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

/// One entry in the `GET /api/presets` response — a browsable preset
/// file living under [`AppState::presets_dir`]. `name` is the on-disk
/// basename without `.json`; it's what the client echoes back in
/// `POST /api/preset`. `label`/`description` come straight from the doc.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PresetEntry {
    pub name: String,
    pub label: Option<String>,
    pub description: Option<String>,
}

/// One replayable capture under the `samples/` tree, surfaced by
/// `GET /api/captures` so the UI's File source tab can list fixtures
/// instead of making the user type a server path. `path` is the
/// absolute server path to hand back in `PATCH /api/source`; `kind`
/// chooses the source block (`iq` → `FileIqSource`, `audio` →
/// `FileAudioSource`). Rate/centre/modulation come from the `*.json`
/// sidecar when present (absent for raw clips with no sidecar — the
/// UI then prompts for `rate_hz_hint`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct CaptureEntry {
    /// Absolute server filesystem path (what the source block opens).
    pub path: String,
    /// `samples/`-relative path — the human-friendly label.
    pub rel: String,
    /// Sidecar `name`, if any.
    pub name: Option<String>,
    /// `"iq"` or `"audio"` — picks `FileIqSource` / `FileAudioSource`.
    pub kind: &'static str,
    pub sample_rate_hz: Option<f64>,
    pub center_freq_hz: Option<f64>,
    pub format: Option<String>,
    pub modulation: Option<String>,
}

/// One block in the currently-loaded preset (post-compose, pre-split).
/// Surfaced by `GET /api/pipeline/blocks` so the UI can render controls
/// for every param on every block without reading preset files directly.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PipelineBlock {
    pub id: String,
    pub type_name: String,
    /// `"node"` or `"browser"` — where the block will run after env_split.
    /// Sourced from the preset's explicit `placement` override, falling
    /// back to the block spec's intrinsic `Placement`.
    pub placement: &'static str,
    /// Full capability schema for the block type — same shape as
    /// `GET /api/blocks` entries.
    pub spec: BlockSchemaDto,
    /// Current param values as stored in the preset doc (post-compose).
    /// `Null` when the preset omits params entirely (block uses defaults).
    pub values: serde_json::Value,
}

pub type FrameBytes = Arc<Vec<u8>>;

/// Pipeline lifecycle. The server-side runtime is either running or
/// stopped; transitions go through [`AppState::start`] / [`stop`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelineStatus {
    Running,
    Stopped,
}

struct Inner {
    /// Shared frame bus. Survives start/stop cycles so a subscriber
    /// connected while stopped picks up frames the moment the pipeline
    /// spins back up.
    frames: FrameBus,
    preset_doc: RwLock<FlowgraphDoc>,
    source_config: RwLock<SourceConfig>,
    /// Runtime profile (audio toggle, demod placement override) applied
    /// to the composed doc before [`split_for_environment`] runs. Lives
    /// here so every compose call site picks up the current value
    /// without threading it through the call chain.
    profile: RwLock<Profile>,
    /// `Some` while the pipeline is running.
    pipeline: Mutex<Option<PresetMount>>,
    tick_period: Duration,
    /// Per-process cache of device capabilities. Populated by
    /// `/api/devices` and `/api/source/capabilities`; pruned when a
    /// device disappears from `enumerate`.
    device_cache: DeviceCache,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
    logs: Option<crate::log_stream::LogBroadcast>,
    /// Directory scanned by [`Self::list_presets`] + resolved by
    /// [`Self::load_preset_by_name`]. `None` means the browse/swap
    /// endpoints return an empty list / 501 respectively.
    presets_dir: Option<Arc<PathBuf>>,
    /// Root scanned by [`Self::list_captures`] (`GET /api/captures`) —
    /// the curated `samples/` tree of replayable IQ/audio fixtures.
    /// `None` means the captures browser returns an empty list.
    captures_dir: Option<Arc<PathBuf>>,
    /// Bridge between `GET /api/ui-views/:pane` (the ferrite-ctl
    /// surface) and the browser's `ViewBridge.svelte` (the canvas
    /// snapshotter). Lives in `AppState` so the two route handlers
    /// (HTTP + WS) share the same broker instance.
    view_bridge: crate::view_bridge::ViewBridge,
}

impl AppState {
    #[must_use]
    pub fn new(preset: FlowgraphDoc, source: SourceConfig, tick_period: Duration) -> Self {
        let frames = FrameBus::new();
        Self {
            inner: Arc::new(Inner {
                frames,
                preset_doc: RwLock::new(preset),
                source_config: RwLock::new(source),
                profile: RwLock::new(Profile::default()),
                pipeline: Mutex::new(None),
                tick_period,
                device_cache: DeviceCache::new(),
            }),
            logs: None,
            presets_dir: None,
            captures_dir: None,
            view_bridge: crate::view_bridge::ViewBridge::default(),
        }
    }

    #[must_use]
    pub fn device_cache(&self) -> &DeviceCache {
        &self.inner.device_cache
    }

    /// Shared `/api/ui-views/:pane` ↔ `/ws/ui-views` broker.
    #[must_use]
    pub fn view_bridge(&self) -> &crate::view_bridge::ViewBridge {
        &self.view_bridge
    }

    #[must_use]
    pub fn with_logs(mut self, logs: crate::log_stream::LogBroadcast) -> Self {
        self.logs = Some(logs);
        self
    }

    /// Point the browse/swap endpoints at a filesystem directory of
    /// preset JSON files.
    /// Point `GET /api/captures` at the curated `samples/` tree.
    #[must_use]
    pub fn with_captures_dir(mut self, dir: PathBuf) -> Self {
        self.captures_dir = Some(Arc::new(dir));
        self
    }

    #[must_use]
    pub fn with_presets_dir(mut self, dir: PathBuf) -> Self {
        self.presets_dir = Some(Arc::new(dir));
        self
    }

    #[must_use]
    pub fn logs(&self) -> Option<&crate::log_stream::LogBroadcast> {
        self.logs.as_ref()
    }

    #[must_use]
    pub fn subscribe(&self) -> mpsc::Receiver<FrameBytes> {
        self.inner.frames.subscribe(DEFAULT_SUBSCRIBER_CAPACITY)
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

    /// Snapshot of the runtime [`Profile`]. Returned by `GET /api/profile`
    /// and read every time the pipeline is (re)composed; defaults to
    /// audio-enabled + no placement override.
    pub async fn get_profile(&self) -> Profile {
        self.inner.profile.read().await.clone()
    }

    /// Replace the profile. Subsequent preset loads / patches /
    /// reconfigures apply the new value. The currently-running
    /// pipeline keeps the old doc until a reconfigure or restart.
    pub async fn set_profile(&self, new_profile: Profile) {
        *self.inner.profile.write().await = new_profile;
    }

    /// Walk the current preset+source's node-half split and surface every
    /// `ui:<name>` sink with the `stream_id` env_split allocated for it.
    /// The client uses this to subscribe to the right frame streams
    /// without reimplementing the allocator.
    pub async fn ui_sinks(&self) -> Result<Vec<UiSink>> {
        let preset = self.inner.preset_doc.read().await.clone();
        let source = self.inner.source_config.read().await.clone();
        let mut composed =
            compose_source(&preset, &source).map_err(|e| anyhow!("compose preset+source: {e}"))?;
        inject_narrow_fft_taps(&mut composed);
        apply_profile(&mut composed, &*self.inner.profile.read().await);
        let node_half = split_for_environment(&composed, Environment::Node, &InventorySpecRegistry)
            .map_err(|e| anyhow!("env_split: {e}"))?;
        let mut out = Vec::new();
        for decl in node_half.blocks.values() {
            let payload_type = match decl.type_name.as_str() {
                "WsBridgeTx" => "IqF32",
                "WsBridgeTxF32" => "F32",
                "WsBridgeTxFftU8" => "FftU8",
                "WsBridgeTxEvents" => "JsonEvent",
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

    /// Query the live driver state for the `src` block. Returns `None`
    /// when the pipeline is stopped or the source is not a SoapySource.
    /// Used by `PATCH /api/source` to include the post-apply snapshot in
    /// the response so the UI can reconcile its optimistic params.
    pub async fn source_readback(&self) -> Option<SoapyReadback> {
        let pipeline = self.inner.pipeline.lock().await;
        if let Some(mount) = pipeline.as_ref() {
            mount.source_readback().await
        } else {
            None
        }
    }

    /// Walk the current composed preset (preset + source merged) and
    /// surface every block with its full spec and current param values.
    /// The list is pre-split, so both node and browser blocks appear —
    /// the UI renders controls for all of them via one dispatcher.
    ///
    /// Iteration order follows `FlowgraphDoc::blocks` (a `BTreeMap`), so
    /// the response is deterministic across calls.
    pub async fn list_blocks(&self) -> Result<Vec<PipelineBlock>> {
        let preset = self.inner.preset_doc.read().await.clone();
        let source = self.inner.source_config.read().await.clone();
        let mut composed =
            compose_source(&preset, &source).map_err(|e| anyhow!("compose preset+source: {e}"))?;
        inject_narrow_fft_taps(&mut composed);
        apply_profile(&mut composed, &*self.inner.profile.read().await);
        let mut out = Vec::with_capacity(composed.blocks.len());
        for (id, decl) in &composed.blocks {
            let Some(entry) = registry::find(&decl.type_name) else {
                // Unregistered type — preset authoring error. Skip rather
                // than fail; the `/api/pipeline` endpoint already reports
                // compose/start errors through its own path.
                tracing::warn!(block_id = %id, type_name = %decl.type_name,
                    "block type not in registry — omitting from /api/pipeline/blocks");
                continue;
            };
            let spec: BlockSchemaDto = entry.spec().into();
            let placement = match decl.placement {
                Some(Environment::Node) => "node",
                Some(Environment::Browser) => "browser",
                None => spec.placement,
            };
            out.push(PipelineBlock {
                id: id.clone(),
                type_name: decl.type_name.clone(),
                placement,
                spec,
                values: decl.params.clone().unwrap_or(serde_json::Value::Null),
            });
        }
        Ok(out)
    }

    /// Apply a params delta to one block by id. Dispatches by id:
    ///
    /// - `id == "src"` — merge delta into `SourceConfig.params` and
    ///   delegate to [`patch_source`], which picks hot-apply vs rebuild.
    /// - any other id — merge delta into `preset_doc.blocks[id].params`
    ///   and, if the pipeline is running, try the block's
    ///   `apply_live_params` hot path via
    ///   [`PresetMount::live_reconfigure_block`]. Only fall back to a
    ///   full [`patch_flowgraph`] when the hot path can't apply.
    ///
    /// The delta is a JSON object; keys present replace, keys absent
    /// stay. Returns the same reconfigure plan shape as the other patch
    /// paths so `POST /api/pipeline/blocks/{id}/params` can share its
    /// response type.
    ///
    /// The hot-path branch for non-source blocks is what keeps
    /// interactive controls from rebuilding the world on every tick —
    /// e.g. channelizer `freq_shift_hz` is declared `SelfBlock`, so
    /// dragging the VFO retunes the baseband-shift without restarting
    /// the source.
    pub async fn apply_block_params(
        &self,
        id: &str,
        delta: serde_json::Value,
    ) -> Result<Option<ReconfigurePlan>> {
        let delta_obj = delta
            .as_object()
            .ok_or_else(|| anyhow!("params delta must be a JSON object"))?
            .clone();

        if id == SOURCE_ID {
            let merged = {
                let current = self.inner.source_config.read().await.clone();
                merge_into_params(current, delta_obj)
            };
            return self.patch_source(merged).await;
        }

        // Is this block on the browser half? Cross-env presets place
        // demod/decim/audio-sink on `placement: "browser"` — `env_split`
        // strips them from the node runtime, so `live_reconfigure_block`
        // against the node half would always error with "no block X in
        // runtime". For browser-placed blocks we just mirror the delta
        // into preset_doc and return a no-op plan; the browser runtime
        // picks up the change the next time it's loaded with a fresh
        // composed doc. (Live-update of browser blocks without a reload
        // is a follow-up — it needs a new reconfigureBlock message on
        // the runner protocol.)
        let is_browser_block = self
            .inner
            .preset_doc
            .read()
            .await
            .blocks
            .get(id)
            .and_then(|b| b.placement)
            .map(|p| matches!(p, ferrite_runtime::Environment::Browser))
            .unwrap_or(false);

        // Fast path: when the pipeline is running and the block lives
        // on the node side, try the block's live update. The runtime's
        // live_reconfigure_block internally falls back to a block-scoped
        // reconfigure_block on Ok(false) from apply_live_params, so a
        // non-live key still lands correctly — we only need
        // patch_flowgraph when there's no running pipeline at all.
        if !is_browser_block {
            let pipeline = self.inner.pipeline.lock().await;
            if let Some(mount) = pipeline.as_ref() {
                let plan = mount
                    .live_reconfigure_block(id, serde_json::Value::Object(delta_obj.clone()))
                    .await?;
                drop(pipeline);
                // Mirror the delta back into preset_doc so subsequent
                // reads of /api/flowgraph and list_blocks see the new
                // values — live_reconfigure_block updates the runtime's
                // applied_doc, but the canonical preset_doc is AppState's.
                let mut new_doc = self.inner.preset_doc.write().await;
                let block = new_doc
                    .blocks
                    .get_mut(id)
                    .ok_or_else(|| anyhow!("no block {id:?} in preset"))?;
                let mut merged = match block.params.take() {
                    Some(serde_json::Value::Object(m)) => m,
                    _ => serde_json::Map::new(),
                };
                for (k, v) in delta_obj {
                    merged.insert(k, v);
                }
                block.params = Some(serde_json::Value::Object(merged));
                return Ok(Some(plan));
            }
        }

        // Pipeline stopped — just stage the edit in the preset doc; the
        // next start() will compose with the new value.
        let mut new_doc = self.inner.preset_doc.read().await.clone();
        let block = new_doc
            .blocks
            .get_mut(id)
            .ok_or_else(|| anyhow!("no block {id:?} in preset"))?;
        let mut merged = match block.params.take() {
            Some(serde_json::Value::Object(m)) => m,
            _ => serde_json::Map::new(),
        };
        for (k, v) in delta_obj {
            merged.insert(k, v);
        }
        block.params = Some(serde_json::Value::Object(merged));
        self.patch_flowgraph(new_doc).await
    }

    /// Enumerate every `*.json` preset file in [`Self::presets_dir`].
    /// Each entry carries the on-disk basename (minus `.json`) as
    /// `name` plus the doc's `label`/`description`. Files that fail to
    /// parse are skipped with a warning — one bad file shouldn't hide
    /// the rest. Returns an empty list when no presets dir is set.
    pub async fn list_presets(&self) -> Result<Vec<PresetEntry>> {
        let Some(dir) = self.presets_dir.as_ref().map(|a| a.as_ref().clone()) else {
            return Ok(Vec::new());
        };
        let entries = tokio::task::spawn_blocking(move || scan_presets(&dir))
            .await
            .map_err(|e| anyhow!("presets scan task panicked: {e}"))??;
        Ok(entries)
    }

    /// Enumerate replayable captures under [`Self::captures_dir`].
    /// Empty when no captures dir is configured.
    pub async fn list_captures(&self) -> Result<Vec<CaptureEntry>> {
        let Some(dir) = self.captures_dir.as_ref().map(|a| a.as_ref().clone()) else {
            return Ok(Vec::new());
        };
        let entries = tokio::task::spawn_blocking(move || scan_captures(&dir))
            .await
            .map_err(|e| anyhow!("captures scan task panicked: {e}"))??;
        Ok(entries)
    }

    /// Load preset `name` from [`Self::presets_dir`] and swap it in via
    /// [`Self::patch_flowgraph`]. Rejects names that aren't plain
    /// basenames (anything containing path separators or `..`).
    pub async fn load_preset_by_name(
        &self,
        name: &str,
    ) -> Result<(FlowgraphDoc, Option<ReconfigurePlan>)> {
        let dir = self
            .presets_dir
            .as_ref()
            .ok_or_else(|| anyhow!("presets browser not configured"))?
            .as_ref()
            .clone();
        if !is_valid_preset_name(name) {
            return Err(anyhow!(
                "invalid preset name {name:?}: must match [A-Za-z0-9_-]+"
            ));
        }
        let file = dir.join(format!("{name}.json"));
        let bytes = tokio::task::spawn_blocking(move || std::fs::read(&file))
            .await
            .map_err(|e| anyhow!("preset read task panicked: {e}"))?
            .map_err(|e| anyhow!("read preset {name:?}: {e}"))?;
        let doc = FlowgraphDoc::from_json(&bytes).map_err(|e| anyhow!("parse preset: {e:#}"))?;
        let plan = self.patch_flowgraph(doc.clone()).await?;
        Ok((doc, plan))
    }

    /// Apply a new preset. If the pipeline is running, reconfigures it
    /// in place (composed with the current source) and returns the
    /// plan; otherwise stores the doc and returns `None`.
    pub async fn patch_flowgraph(&self, new_doc: FlowgraphDoc) -> Result<Option<ReconfigurePlan>> {
        let source = self.inner.source_config.read().await.clone();
        let mut composed =
            compose_source(&new_doc, &source).map_err(|e| anyhow!("compose preset+source: {e}"))?;
        inject_narrow_fft_taps(&mut composed);
        apply_profile(&mut composed, &*self.inner.profile.read().await);
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
    ///
    /// Fast path: when the new config has the same `type` as the current
    /// one and the params delta is a shallow top-level change, try
    /// [`PresetMount::live_reconfigure_block`] on the `src` block first.
    /// The block's `apply_live_params` decides whether it can honour the
    /// delta without a stream restart; if not, the runtime falls back to
    /// a full rebuild. This keeps the `PATCH /api/source` entry point
    /// behaviourally identical to `apply_block_params("src", delta)` —
    /// the frontend can send partial source-param edits through either.
    pub async fn patch_source(&self, new_source: SourceConfig) -> Result<Option<ReconfigurePlan>> {
        // Compute the delta in its own scope so the read guard drops
        // before we try to take the pipeline mutex or the source_config
        // write — Rust extends temporary lifetimes across an `if let`
        // body, which would otherwise deadlock with the write below.
        let maybe_delta = {
            let current = self.inner.source_config.read().await;
            shallow_source_delta(&current, &new_source)
        };
        if let Some(delta) = maybe_delta {
            let pipeline = self.inner.pipeline.lock().await;
            if let Some(mount) = pipeline.as_ref() {
                let plan = mount
                    .live_reconfigure_block(SOURCE_ID, serde_json::Value::Object(delta))
                    .await?;
                drop(pipeline);
                *self.inner.source_config.write().await = new_source;
                return Ok(Some(plan));
            }
        }

        let preset = self.inner.preset_doc.read().await.clone();
        let mut composed = compose_source(&preset, &new_source)
            .map_err(|e| anyhow!("compose preset+source: {e}"))?;
        inject_narrow_fft_taps(&mut composed);
        apply_profile(&mut composed, &*self.inner.profile.read().await);
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
        let mut composed =
            compose_source(&preset, &source).map_err(|e| anyhow!("compose preset+source: {e}"))?;
        inject_narrow_fft_taps(&mut composed);
        apply_profile(&mut composed, &*self.inner.profile.read().await);
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

/// Merge a partial params delta into `cfg`, producing a new config.
/// Top-level keys in `delta` replace keys in `cfg.params`; keys absent
/// from `delta` are kept. Used by [`AppState::apply_block_params`] to
/// turn a per-block delta on `"src"` into a full [`SourceConfig`] for
/// [`AppState::patch_source`] to apply.
fn merge_into_params(
    mut cfg: SourceConfig,
    delta: serde_json::Map<String, serde_json::Value>,
) -> SourceConfig {
    let mut merged = match std::mem::replace(&mut cfg.params, serde_json::Value::Null) {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    for (k, v) in delta {
        merged.insert(k, v);
    }
    cfg.params = serde_json::Value::Object(merged);
    cfg
}

/// Compute a shallow top-level delta between two source configs. Returns
/// the changed keys when a hot-apply attempt is meaningful:
///
/// - types match (type change always rebuilds — it's a different block),
/// - at least one top-level param key differs.
///
/// Callers hand the delta to `live_reconfigure_block("src", …)` where the
/// block's `apply_live_params` whitelist decides rebuild-vs-hot. Nested
/// objects (e.g. the `settings` sub-map for driver `writeSetting` keys)
/// are compared as opaque values, so any change there surfaces as a
/// single `"settings"` key on the delta and the hot path (which doesn't
/// understand `settings`) will correctly fall back to rebuild.
fn shallow_source_delta(
    current: &SourceConfig,
    next: &SourceConfig,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if current.type_name != next.type_name {
        return None;
    }
    let current_obj = current.params.as_object();
    let next_obj = next.params.as_object()?;
    let mut delta = serde_json::Map::new();
    for (k, v) in next_obj {
        let same = current_obj
            .and_then(|c| c.get(k))
            .is_some_and(|cur| cur == v);
        if !same {
            delta.insert(k.clone(), v.clone());
        }
    }
    if let Some(c) = current_obj {
        // Keys removed in `next` aren't expressible as a shallow delta —
        // fall through to full rebuild by bailing out.
        for k in c.keys() {
            if !next_obj.contains_key(k) {
                return None;
            }
        }
    }
    Some(delta)
}

/// Preset filenames come in via untrusted HTTP bodies. Accept only
/// simple basenames — letters, digits, underscore, dash — so a value
/// like `../../../etc/passwd` can never escape the presets dir.
fn is_valid_preset_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn scan_presets(dir: &std::path::Path) -> Result<Vec<PresetEntry>> {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(anyhow!("read presets dir {}: {e}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in read {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(?err, "presets dir iteration error");
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if !is_valid_preset_name(stem) {
            continue;
        }
        match std::fs::read(&path).and_then(|b| {
            FlowgraphDoc::from_json(&b)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:#}")))
        }) {
            Ok(doc) => out.push(PresetEntry {
                name: stem.to_string(),
                label: doc.label,
                description: doc.description,
            }),
            Err(err) => tracing::warn!(path = %path.display(), ?err, "skipping bad preset"),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// `iq` (→ `FileIqSource`) / `audio` (→ `FileAudioSource`) / `None`
/// (not replayable by either — skip). Sidecar `format` wins; else the
/// filename convention (`…_iq-…` / `…_audio-…`) / extension.
fn capture_kind(file_name: &str, ext: &str, fmt: Option<&str>) -> Option<&'static str> {
    let f = fmt.unwrap_or("").to_ascii_lowercase();
    let n = file_name.to_ascii_lowercase();
    match ext {
        // Headerless raw float IQ — always FileIqSource.
        "cf32" | "iq" => Some("iq"),
        "wav" => {
            if f.contains("iq") || n.contains("_iq-") || n.contains("-iq") {
                Some("iq")
            } else {
                // mono audio is the common case (the fldigi / audio
                // fixtures); FileAudioSource rejects stereo cleanly if
                // a convention slips through.
                Some("audio")
            }
        }
        // cu8 / bin / mp3 / json / images: not FileIq/AudioSource-able.
        _ => None,
    }
}

/// Look for `<file>.json` then `<stem>.json` beside a capture and pull
/// the few fields the picker shows. Missing/!json → all `None`.
fn read_sidecar(
    path: &std::path::Path,
) -> (
    Option<String>,
    Option<String>,
    Option<f64>,
    Option<f64>,
    Option<String>,
) {
    let stem_json = path.with_extension("json");
    let full_json = {
        let mut s = path.as_os_str().to_owned();
        s.push(".json");
        std::path::PathBuf::from(s)
    };
    let bytes = std::fs::read(&full_json)
        .or_else(|_| std::fs::read(&stem_json))
        .ok();
    let Some(v) = bytes.and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) else {
        return (None, None, None, None, None);
    };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_f64);
    (
        s("name"),
        s("format"),
        n("sample_rate_hz"),
        n("center_freq_hz"),
        s("modulation"),
    )
}

/// Recursively enumerate replayable captures under `root`.
fn scan_captures(root: &std::path::Path) -> Result<Vec<CaptureEntry>> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(anyhow!("read captures dir {}: {e}", dir.display())),
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let (name, format, sample_rate_hz, center_freq_hz, modulation) = read_sidecar(&path);
            let Some(kind) = capture_kind(&file_name, &ext, format.as_deref()) else {
                continue;
            };
            let abs = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            out.push(CaptureEntry {
                path: abs.to_string_lossy().into_owned(),
                rel,
                name,
                kind,
                sample_rate_hz,
                center_freq_hz,
                format,
                modulation,
            });
        }
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrite_blocks::frame::Frame;
    use serde_json::json;
    use std::time::Duration;
    use tempfile::tempdir;

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

    #[test]
    fn capture_kind_classifies() {
        // sidecar format wins
        assert_eq!(
            capture_kind("x.wav", "wav", Some("wav-pcm-s16-stereo-iq")),
            Some("iq")
        );
        assert_eq!(
            capture_kind("x.wav", "wav", Some("wav-pcm-s16-mono-audio")),
            Some("audio")
        );
        // raw float is always IQ; filename convention as fallback
        assert_eq!(capture_kind("a.cf32", "cf32", None), Some("iq"));
        assert_eq!(capture_kind("aprs_145_iq-s16.wav", "wav", None), Some("iq"));
        assert_eq!(capture_kind("Olivia_8-500.wav", "wav", None), Some("audio"));
        // non-replayable extensions are skipped
        assert_eq!(capture_kind("x.cu8", "cu8", None), None);
        assert_eq!(capture_kind("x.mp3", "mp3", None), None);
    }

    #[test]
    fn scan_captures_reads_sidecars_and_recurses() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("vhf")).unwrap();
        // IQ wav + stem.json sidecar in a subdir
        std::fs::write(dir.path().join("vhf/cap_iq-s16.wav"), b"RIFFfake").unwrap();
        std::fs::write(
            dir.path().join("vhf/cap_iq-s16.json"),
            br#"{"name":"Cap","format":"wav-pcm-s16-stereo-iq",
                 "sample_rate_hz":39062,"center_freq_hz":145070000,
                 "modulation":"AFSK"}"#,
        )
        .unwrap();
        // raw .iq + full-name `.iq.json` sidecar at the root
        std::fs::write(dir.path().join("ref.iq"), b"\0\0\0\0").unwrap();
        std::fs::write(
            dir.path().join("ref.iq.json"),
            br#"{"format":"f32","sample_rate_hz":375}"#,
        )
        .unwrap();
        // a sidecar-less audio wav + a non-replayable file (ignored)
        std::fs::write(dir.path().join("Olivia_8-500.wav"), b"RIFFfake").unwrap();
        std::fs::write(dir.path().join("notes.md"), b"x").unwrap();

        let mut got = scan_captures(dir.path()).unwrap();
        got.sort_by(|a, b| a.rel.cmp(&b.rel));
        assert_eq!(got.len(), 3, "got {got:?}");

        let olivia = got.iter().find(|c| c.rel == "Olivia_8-500.wav").unwrap();
        assert_eq!(olivia.kind, "audio");
        assert!(olivia.sample_rate_hz.is_none()); // no sidecar

        let refiq = got.iter().find(|c| c.rel == "ref.iq").unwrap();
        assert_eq!(refiq.kind, "iq");
        assert_eq!(refiq.sample_rate_hz, Some(375.0)); // `<file>.iq.json`

        let cap = got
            .iter()
            .find(|c| c.rel.ends_with("cap_iq-s16.wav"))
            .unwrap();
        assert_eq!(cap.kind, "iq");
        assert_eq!(cap.name.as_deref(), Some("Cap"));
        assert_eq!(cap.sample_rate_hz, Some(39062.0));
        assert_eq!(cap.center_freq_hz, Some(145_070_000.0));
        assert!(cap.path.ends_with("cap_iq-s16.wav") && cap.path.starts_with('/'));
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
    async fn ui_sinks_reports_json_event_payload_for_events_stream() {
        // A decoder produces `Events`; a `ui:events` terminus must surface
        // in the API as `payload_type: "JsonEvent"` so the browser knows
        // to decode the payload as JSON instead of IQ or FFT bytes. The
        // `src` placeholder is present but unused here — compose_source
        // requires it to exist; validate_doc tolerates isolated blocks.
        let preset: FlowgraphDoc = serde_json::from_value(json!({
            "name": "ev",
            "environments": ["node", "browser"],
            "blocks": {
                "src":     { "type": "Source", "placement": "node",
                             "params": { "center_freq_hz": 0.0, "sample_rate_hz": 8000.0 } },
                "audio":   { "type": "DtmfAudioSource", "placement": "node",
                             "params": { "digits": "1", "sample_rate_hz": 8000.0 } },
                "decoder": { "type": "DtmfDecoder", "placement": "node",
                             "params": { "sample_rate_hz": 8000.0 } }
            },
            "wires": [
                ["audio.out",   "decoder.in"],
                ["decoder.out", "ui:events"]
            ]
        }))
        .unwrap();
        let state = AppState::new(preset, test_source(), Duration::from_millis(5));
        let sinks = state.ui_sinks().await.unwrap();
        assert_eq!(sinks.len(), 1);
        assert_eq!(sinks[0].name, "events");
        assert_eq!(sinks[0].payload_type, "JsonEvent");
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

    #[tokio::test]
    async fn list_blocks_surfaces_every_composed_block_with_spec_and_values() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        let blocks = state.list_blocks().await.unwrap();
        // test_preset has src + sink; compose_source replaces the Source
        // placeholder with SineSource (as per SourceConfig.type_name) but
        // keeps the id.
        let ids: Vec<_> = blocks.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["sink", "src"]);

        let src = blocks.iter().find(|b| b.id == "src").unwrap();
        assert_eq!(src.type_name, "SineSource");
        assert_eq!(src.placement, "node");
        assert_eq!(src.spec.type_name, "SineSource");
        // SourceConfig values should have flowed through compose_source.
        assert_eq!(src.values["tone_freq_abs_hz"].as_f64(), Some(100.0));
        assert_eq!(src.values["amplitude"].as_f64(), Some(0.5));

        let sink = blocks.iter().find(|b| b.id == "sink").unwrap();
        assert_eq!(sink.type_name, "Decimator");
        assert_eq!(sink.placement, "browser");
        assert_eq!(sink.values["factor"].as_i64(), Some(2));
        // Spec is the full schema — pick a param to confirm it came
        // through.
        assert!(sink.spec.params.iter().any(|p| p.key == "factor"));
    }

    #[tokio::test]
    async fn apply_block_params_on_src_routes_to_source_config() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        let plan = state
            .apply_block_params("src", json!({ "tone_freq_abs_hz": 250.0 }))
            .await
            .unwrap();
        assert!(plan.is_none(), "no pipeline to reconfigure while stopped");
        // Delta merged into SourceConfig.params — the original
        // amplitude and center_freq_hz are preserved.
        let source = state.get_source().await;
        assert_eq!(source.params["tone_freq_abs_hz"].as_f64(), Some(250.0));
        assert_eq!(source.params["amplitude"].as_f64(), Some(0.5));
        assert_eq!(source.params["center_freq_hz"].as_f64(), Some(0.0));
    }

    #[tokio::test]
    async fn apply_block_params_on_non_source_merges_into_preset_doc() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        let plan = state
            .apply_block_params("sink", json!({ "factor": 4 }))
            .await
            .unwrap();
        assert!(plan.is_none(), "no pipeline to reconfigure while stopped");
        let doc = state.get_flowgraph().await;
        let sink = doc.blocks.get("sink").unwrap();
        let params = sink.params.as_ref().unwrap();
        // Delta merged — factor updated, other params preserved.
        assert_eq!(params["factor"].as_i64(), Some(4));
        assert_eq!(params["num_taps"].as_i64(), Some(17));
        assert_eq!(params["cutoff_normalized"].as_f64(), Some(0.2));
    }

    #[tokio::test]
    async fn apply_block_params_hot_reconfigures_running_pipeline() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        state.start().await.unwrap();
        let plan = state
            .apply_block_params("src", json!({ "tone_freq_abs_hz": 300.0 }))
            .await
            .expect("reconfigure ok");
        assert!(plan.is_some(), "running pipeline must return a plan");
        state.stop().await;
    }

    #[tokio::test]
    async fn apply_block_params_rejects_unknown_block_id() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        let err = state
            .apply_block_params("not_a_block", json!({ "x": 1 }))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not_a_block"));
    }

    #[tokio::test]
    async fn apply_block_params_rejects_non_object_delta() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        let err = state
            .apply_block_params("sink", json!(42))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("JSON object"));
    }

    fn write_preset(dir: &std::path::Path, name: &str, label: &str) {
        let doc = json!({
            "name": name,
            "label": label,
            "description": format!("{name} preset"),
            "environments": ["node", "browser"],
            "blocks": {
                "src":  { "type": "Source", "placement": "node",
                          "params": { "center_freq_hz": 0.0, "sample_rate_hz": 1000.0 } },
                "sink": { "type": "Decimator", "placement": "browser",
                          "params": { "factor": 2, "num_taps": 17,
                                      "cutoff_normalized": 0.2 } }
            },
            "wires": [["src.out", "sink.in"]]
        });
        std::fs::write(
            dir.join(format!("{name}.json")),
            serde_json::to_vec_pretty(&doc).unwrap(),
        )
        .unwrap();
    }

    #[tokio::test]
    async fn list_presets_returns_empty_when_dir_unset() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        assert!(state.list_presets().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_presets_scans_dir_and_sorts_by_name() {
        let dir = tempdir().unwrap();
        write_preset(dir.path(), "alpha", "Alpha label");
        write_preset(dir.path(), "beta", "Beta label");
        std::fs::write(dir.path().join("notjson.txt"), b"ignored").unwrap();
        std::fs::write(dir.path().join("bad.json"), b"not json").unwrap();
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5))
            .with_presets_dir(dir.path().to_path_buf());
        let entries = state.list_presets().await.unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(entries[0].label.as_deref(), Some("Alpha label"));
    }

    #[tokio::test]
    async fn load_preset_by_name_swaps_active_doc() {
        let dir = tempdir().unwrap();
        write_preset(dir.path(), "wbfm_test", "FM test");
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5))
            .with_presets_dir(dir.path().to_path_buf());
        let (doc, plan) = state.load_preset_by_name("wbfm_test").await.unwrap();
        assert_eq!(doc.name, "wbfm_test");
        assert!(plan.is_none(), "no pipeline to reconfigure while stopped");
        let current = state.get_flowgraph().await;
        assert_eq!(current.name, "wbfm_test");
    }

    #[tokio::test]
    async fn load_preset_by_name_rejects_path_traversal() {
        let dir = tempdir().unwrap();
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5))
            .with_presets_dir(dir.path().to_path_buf());
        for bad in ["..", "../etc", "foo/bar", "a.b"] {
            let err = state.load_preset_by_name(bad).await.unwrap_err();
            assert!(
                format!("{err:#}").contains("invalid preset name"),
                "name {bad:?} should be rejected, got: {err:#}"
            );
        }
    }

    #[tokio::test]
    async fn load_preset_by_name_errors_when_dir_unset() {
        let state = AppState::new(test_preset(), test_source(), Duration::from_millis(5));
        let err = state.load_preset_by_name("wbfm").await.unwrap_err();
        assert!(format!("{err:#}").contains("not configured"));
    }
}
