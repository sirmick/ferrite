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
    apply_profile, compose_source, inject_narrow_fft_taps, inject_voice_transcribe,
    split_for_environment, Environment, FlowgraphDoc, InventorySpecRegistry, Profile,
    ReconfigurePlan, SourceConfig, SOURCE_ID,
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
    /// Latest node-side flow-diagnostics snapshot, refreshed ~1 Hz by
    /// the running pipeline's diag loop. Served verbatim by
    /// `GET /api/flowdiag` — a dedicated channel so flowdiag is always
    /// available regardless of `RUST_LOG` and never clutters the log
    /// stream. `None` until the first snapshot (or while stopped).
    latest_diag: Arc<RwLock<Option<ferrite_runtime::DiagSnapshot>>>,
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
                latest_diag: Arc::new(RwLock::new(None)),
            }),
            logs: None,
            presets_dir: None,
            captures_dir: None,
            view_bridge: crate::view_bridge::ViewBridge::default(),
        }
    }

    /// Latest node-side [`ferrite_runtime::DiagSnapshot`], or `None`
    /// when the pipeline is stopped / hasn't produced one yet. Backs
    /// `GET /api/flowdiag`.
    pub async fn flowdiag(&self) -> Option<ferrite_runtime::DiagSnapshot> {
        self.inner.latest_diag.read().await.clone()
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

    /// The doc the running runtime is actually executing, or `None`
    /// when stopped. The single reader-of-record while live — callers
    /// overlay these params via [`overlay_live_params`] so reads
    /// reflect the runtime, not the staged `preset_doc`.
    async fn applied_doc_if_running(&self) -> Option<FlowgraphDoc> {
        let guard = self.inner.pipeline.lock().await;
        match guard.as_ref() {
            Some(mount) => mount.applied_doc_snapshot().await,
            None => None,
        }
    }

    /// Compose `preset + source` into a runnable flowgraph with every
    /// stage of the standard pipeline applied: source merge, narrow-FFT
    /// tap injection, profile overlay, voice-transcribe overlay. Five
    /// callers used to hand-roll this dance; skipping any one of the
    /// injections silently diverged the running pipeline from what
    /// /api/pipeline/blocks reports. Callers handle their own
    /// preset/source acquisition (some need overrides) — this helper
    /// owns the *order* and the lock on `profile`.
    async fn compose_full(
        &self,
        preset: &FlowgraphDoc,
        source: &SourceConfig,
    ) -> Result<FlowgraphDoc> {
        let mut composed =
            compose_source(preset, source).map_err(|e| anyhow!("compose preset+source: {e}"))?;
        inject_narrow_fft_taps(&mut composed);
        let profile = self.inner.profile.read().await;
        apply_profile(&mut composed, &profile);
        inject_voice_transcribe(&mut composed, &profile);
        Ok(composed)
    }

    pub async fn get_flowgraph(&self) -> FlowgraphDoc {
        // Drop the preset_doc read guard before touching the pipeline
        // lock — canonical order is pipeline → preset_doc, never the
        // reverse.
        let mut doc = self.inner.preset_doc.read().await.clone();
        if let Some(applied) = self.applied_doc_if_running().await {
            overlay_live_params(&mut doc, &applied);
        }
        doc
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
        let composed = self.compose_full(&preset, &source).await?;
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

    /// 1 Hz-polled snapshot of the source driver's state, populated by
    /// the pipeline's diag tick. Cheap (no runtime mutex hit) and stays
    /// fresh while the pipeline runs — surfaces AGC-driven gain motion
    /// to the UI/AI between explicit `PATCH /api/source` writes.
    /// `None` when the pipeline is stopped or hasn't ticked once yet.
    pub async fn cached_source_readback(&self) -> Option<SoapyReadback> {
        let pipeline = self.inner.pipeline.lock().await;
        if let Some(mount) = pipeline.as_ref() {
            mount.cached_source_readback().await
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
        let mut composed = self.compose_full(&preset, &source).await?;
        // While the pipeline is live the runtime is the reader-of-
        // record: overlay its applied node-block params so an
        // interactive edit shows here without a preset_doc mirror-back.
        if let Some(applied) = self.applied_doc_if_running().await {
            overlay_live_params(&mut composed, &applied);
        }
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
    /// - any other id — while the pipeline is **running**, apply the
    ///   delta to the runtime only via
    ///   [`PresetMount::live_reconfigure_block`] (which itself falls
    ///   back to a block-scoped rebuild when the block can't hot-apply).
    ///   The runtime's `applied_doc` is the single writer while live;
    ///   there is no `preset_doc` mirror-back (reads go through
    ///   `applied_doc` — see [`Self::list_blocks`] / [`get_flowgraph`]
    ///   — and [`Self::stop`] folds the final live state back into
    ///   `preset_doc` once). While **stopped**, stage the delta into
    ///   `preset_doc` via [`patch_flowgraph`] so the next start composes
    ///   with it.
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
                // Single writer while live: the runtime's applied_doc.
                // The old per-edit preset_doc mirror-back (a nested
                // write held under the pipeline lock) was the desync
                // window / lock-order hazard the cleanup audit hit —
                // it's gone. Reads go through applied_doc; stop() folds
                // the final live state back into preset_doc once.
                let plan = mount
                    .live_reconfigure_block(id, serde_json::Value::Object(delta_obj.clone()))
                    .await?;
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

    /// Read the named preset's full JSON from [`Self::presets_dir`].
    /// `name` must match `[A-Za-z0-9_-]+` (same rule as
    /// [`Self::load_preset_by_name`]) — anything with path separators
    /// or `..` is rejected without touching disk. Returns the raw
    /// `serde_json::Value` so callers can surface every field the
    /// preset declares (label, description, ai_notes, blocks, wires,
    /// signal_wiki_url, …) without re-typing the schema.
    pub async fn read_preset_doc(&self, name: &str) -> Result<serde_json::Value> {
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
        let path = dir.join(format!("{name}.json"));
        let bytes = tokio::fs::read(&path)
            .await
            .map_err(|e| anyhow!("read {}: {e}", path.display()))?;
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|e| anyhow!("parse {}: {e}", path.display()))?;
        Ok(value)
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
        let slug = name.to_string();
        let doc = tokio::task::spawn_blocking(move || resolve_preset_doc(&dir, &slug))
            .await
            .map_err(|e| anyhow!("preset resolve task panicked: {e}"))??;
        let plan = self.patch_flowgraph(doc.clone()).await?;
        Ok((doc, plan))
    }

    /// Apply a new preset. If the pipeline is running, reconfigures it
    /// in place (composed with the current source) and returns the
    /// plan; otherwise stores the doc and returns `None`.
    pub async fn patch_flowgraph(&self, new_doc: FlowgraphDoc) -> Result<Option<ReconfigurePlan>> {
        let source = self.inner.source_config.read().await.clone();
        let composed = self.compose_full(&new_doc, &source).await?;
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
    pub async fn patch_source(
        &self,
        mut new_source: SourceConfig,
    ) -> Result<Option<ReconfigurePlan>> {
        // Compute the delta in its own scope so the read guard drops
        // before we try to take the pipeline mutex or the source_config
        // write — Rust extends temporary lifetimes across an `if let`
        // body, which would otherwise deadlock with the write below.
        let maybe_delta = {
            // Clone + drop the guard so the (async, cached) caps probe in
            // apply_source_policy never holds the source_config read lock.
            let current = self.inner.source_config.read().await.clone();
            // Fill the daemon-owned derived settings — anti-alias bandwidth
            // from the sample rate, SDRplay broadcast notches from the tuned
            // span — BEFORE the delta is computed, so every path (UI mouse,
            // /api/tune span-raise, ferrite-ctl, headless AI) gets them.
            self.apply_source_policy(&current, &mut new_source).await;
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
        let composed = self.compose_full(&preset, &new_source).await?;
        let mut pipeline = self.inner.pipeline.lock().await;
        let plan = if let Some(mount) = pipeline.as_mut() {
            Some(mount.reconfigure(&composed).await?)
        } else {
            None
        };
        *self.inner.source_config.write().await = new_source;
        Ok(plan)
    }

    /// Fill the derived source settings the daemon owns: anti-alias
    /// `bandwidth_hz` (largest device-advertised filter ≤ the sample rate)
    /// and the SDRplay broadcast-notch `settings` (off inside the band
    /// you're receiving, on outside). Applied at the [`patch_source`]
    /// choke point so the UI, ferrite-ctl, and a headless AI all behave
    /// identically — none of them has to set BW or notches by hand.
    ///
    /// Only fills what the caller left implicit: a value the caller
    /// explicitly changed in *this* reconfigure (differs from `current`)
    /// wins. Writing an unchanged value is a no-op that `shallow_source_delta`
    /// suppresses, so a center-only retune stays a hot apply and the notch
    /// only forces a rebuild when the tune crosses a broadcast-band edge.
    ///
    /// [`patch_source`]: AppState::patch_source
    async fn apply_source_policy(&self, current: &SourceConfig, next: &mut SourceConfig) {
        if next.type_name != "SoapySource" {
            return; // software sources (Sine/File) have no caps to key off
        }
        let args = next
            .params
            .as_object()
            .and_then(|p| p.get("args"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        if args.is_empty() {
            return;
        }
        // Cached after warm-up; a probe error/timeout must NOT block a
        // tune — skip the policy and keep the caller's params.
        let caps = match self.inner.device_cache.ensure_args(&args).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = ?e,
                    "apply_source_policy: caps probe failed; skipping derived BW/notch"
                );
                return;
            }
        };
        // Pure decision logic (exhaustively unit-tested in `source_policy`).
        crate::source_policy::apply(current, next, &caps);
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
        let composed = self.compose_full(&preset, &source).await?;
        let mount = spawn_preset(
            &composed,
            self.inner.frames.clone(),
            self.inner.tick_period,
            Arc::clone(&self.inner.latest_diag),
        )?;
        *guard = Some(mount);
        Ok(())
    }

    /// Tear down every loaded SoapySDR driver module and re-load them
    /// from the search path. Recovery path for when a driver wedges
    /// in-process (external `SoapySDRUtil --find` works but our
    /// enumerate hangs) and a full ferrited restart would be too
    /// disruptive.
    ///
    /// Refuses with an error if the pipeline is running, because a
    /// live source-block holds a `SoapySDRDevice` whose backing
    /// memory would dangle past `SoapySDR_unloadModules`. The
    /// device-capability cache is cleared first so the next
    /// enumerate re-probes against fresh driver state.
    ///
    /// Note: service-process drivers (SDRplay's `sdrplay_apiService`)
    /// often wedge on the service side, not in the driver module —
    /// `systemctl restart sdrplay` is the correct hammer there. This
    /// path is most useful for pure-library drivers (HackRF, RTL-SDR,
    /// etc.) whose state lives inside ferrited.
    pub async fn reload_devices(&self) -> Result<()> {
        if self.inner.pipeline.lock().await.is_some() {
            return Err(anyhow!(
                "cannot reload SoapySDR drivers while the pipeline is running — stop it first"
            ));
        }
        self.inner.device_cache.clear().await;
        // dlclose/dlopen are blocking and contend on the C runtime's
        // mutex; off-runtime keeps the axum worker free.
        tokio::task::spawn_blocking(crate::device::reload_modules)
            .await
            .map_err(|e| anyhow!("SoapySDR module reload task panicked: {e}"))?;
        tracing::info!("SoapySDR modules reloaded");
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
        // Drop the stale snapshot so `GET /api/flowdiag` reports
        // "stopped" (empty) rather than a frozen last frame.
        *self.inner.latest_diag.write().await = None;
        // Fold the runtime's live param edits back into preset_doc —
        // the single deliberate writer that replaces the per-edit
        // mirror-back removed from apply_block_params. Snapshot before
        // teardown drops the runtime. Canonical lock order is preserved
        // (pipeline guard held, then preset_doc write, same as
        // patch_flowgraph), and this is the stop transition, not the
        // hot interactive path, so there is no lock-order hazard.
        let applied = mount.applied_doc_snapshot().await;
        // Destructure to take ownership of the handle for a graceful
        // shutdown; the other fields drop when the block ends.
        let PresetMount { handle, .. } = mount;
        if let Err(err) = handle.shutdown().await {
            tracing::warn!(?err, "preset pipeline shutdown returned error");
        }
        if let Some(applied) = applied {
            let mut doc = self.inner.preset_doc.write().await;
            overlay_live_params(&mut doc, &applied);
        }
        true
    }

    /// Single tuning intent — "listen at `freq_hz`". Encodes the
    /// DC-spike dodge (zero-IF radios produce a bright line at the
    /// source's centre; HackRF has nothing to remove it) and the
    /// channelizer-aware "already in range" check, so every caller (UI
    /// tuner-Enter, band-preset rx/tune, ferrite-ctl tune, AI tune)
    /// gets identical behaviour without re-implementing the math.
    ///
    /// Behaviour:
    /// 1. If `span_hz` is given and exceeds the current sample rate,
    ///    raise the source rate to span. Drivers clamp/snap to their
    ///    own ladders inside `SoapySource::apply_live_params`.
    /// 2. Find the first `Channelizer` in the composed preset and read
    ///    its `output_rate_hz` (channel passband). If none exists the
    ///    dodge is skipped and `src.center_freq_hz` is just set to the
    ///    target directly.
    /// 3. Keepout = `0.4 × output_rate_hz`. If `|freq_hz - src_center|
    ///    ≤ keepout`, keep `src_center` parked and only retune
    ///    `chan.freq_shift_hz`. Otherwise snap `src_center` so the
    ///    target sits `offset_ratio × output_rate_hz` off centre — i.e.
    ///    the spike lands outside the demodulated channel.
    ///
    /// `offset_ratio` is supplied by the caller from per-driver config
    /// (HackRF: ~0.4 — see `web/src/lib/controls/sdr-presets/hackrf.json`;
    /// most drivers: 0). When 0, the dodge collapses to "just retune".
    ///
    /// Two reconfigures (source delta + channelizer live param) — the
    /// channelizer hot-applies, the source hot-applies via the
    /// `apply_live_params` whitelist when only `center_freq_hz` changes,
    /// or rebuilds when `sample_rate_hz` changes too. The brief window
    /// where the spike is at the requested freq before the shift lands
    /// is acceptable (a tune is an explicit operator action, not a
    /// continuous control loop). Returns the channelizer-side plan when
    /// it applied, else the source plan, else `None`.
    pub async fn tune(
        &self,
        freq_hz: f64,
        span_hz: Option<f64>,
        offset_ratio: f64,
    ) -> Result<Option<ReconfigurePlan>> {
        if !freq_hz.is_finite() {
            return Err(anyhow!("tune: freq_hz must be finite, got {freq_hz}"));
        }
        let preset = self.inner.preset_doc.read().await.clone();
        let source = self.inner.source_config.read().await.clone();
        let composed = self.compose_full(&preset, &source).await?;

        // First channelizer in the composed graph (preset convention:
        // there's exactly one; AFC's `first_id_typed::<Channelizer>` makes
        // the same assumption). Without a channelizer the dodge has no
        // landing zone — fall back to writing src.center_freq_hz directly.
        let chan: Option<(String, f64)> = composed
            .blocks
            .iter()
            .find(|(_, b)| b.type_name == "Channelizer")
            .and_then(|(id, b)| {
                let rate = b
                    .params
                    .as_ref()
                    .and_then(|p| p.get("output_rate_hz"))
                    .and_then(serde_json::Value::as_f64)?;
                Some((id.clone(), rate))
            });

        // The source's *known* current centre. `None` means it's never been
        // written (fresh device-select / preset default) — crucially NOT the
        // same as "happens to equal the target": defaulting an unknown centre
        // to `freq_hz` made the dodge below think the source was already on
        // target, go sticky, and skip the write — leaving the device parked
        // at its block default (100 MHz) while the operator thinks it tuned.
        let cur_src_center = source
            .params
            .get("center_freq_hz")
            .and_then(serde_json::Value::as_f64);
        let cur_rate = source
            .params
            .get("sample_rate_hz")
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);

        // Compute new src_center + chan.freq_shift_hz from the dodge math.
        let (new_src_center, new_freq_shift) = match &chan {
            Some((_, chan_rate)) => {
                let keepout = 0.4 * chan_rate;
                let dodge = offset_ratio * chan_rate;
                match cur_src_center {
                    // Sticky: source already parked within keepout of target.
                    Some(cur) if (freq_hz - cur).abs() <= keepout => (cur, freq_hz - cur),
                    // Snap (also the first-tune / unknown-centre case): park
                    // the source low of target by the dodge offset and let the
                    // channelizer recover the carrier.
                    _ => (freq_hz - dodge, dodge),
                }
            }
            None => (freq_hz, 0.0),
        };

        // Build source-side delta. 0.5 Hz floor on the centre delta so a
        // round-trip readback noise doesn't trigger a reconfigure — but
        // always write on the first tune (unknown centre), so an unset
        // source centre actually gets established instead of silently
        // staying at the block default.
        let mut src_delta = serde_json::Map::new();
        let center_changed = cur_src_center.is_none_or(|cur| (new_src_center - cur).abs() > 0.5);
        if center_changed {
            src_delta.insert("center_freq_hz".into(), serde_json::json!(new_src_center));
        }
        if let Some(span) = span_hz {
            if span > cur_rate {
                src_delta.insert("sample_rate_hz".into(), serde_json::json!(span));
            }
        }

        let mut last_plan: Option<ReconfigurePlan> = None;
        if !src_delta.is_empty() {
            let merged = merge_into_params(source, src_delta);
            last_plan = self.patch_source(merged).await?;
        }

        // Channelizer hot-retune. apply_block_params routes non-`src` ids
        // through PresetMount::live_reconfigure_block, which the
        // Channelizer's `apply_live_params` honours for freq_shift_hz.
        if let Some((chan_id, _)) = chan {
            let chan_delta = serde_json::json!({ "freq_shift_hz": new_freq_shift });
            if let Some(plan) = self.apply_block_params(&chan_id, chan_delta).await? {
                last_plan = Some(plan);
            }
        }
        Ok(last_plan)
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

/// Overlay the live param values from a running runtime's `applied`
/// doc onto `base`, for every block id present in both. This is how
/// the running pipeline becomes the single reader-of-record: `base`
/// (composed from `preset_doc`) supplies structure + browser-side
/// params, while the runtime supplies the live truth for the node
/// blocks it actually executes — so a reader sees what's running, not
/// the last value staged in `preset_doc`.
///
/// `SOURCE_ID` is skipped: the source's persisted authority is
/// `source_config` (via [`AppState::patch_source`]), and `base`'s
/// `src` is the `Source` placeholder whose `type` the browser's
/// client-side `composeSource` keys off. Synthetic node blocks the
/// env-split injected (bridge Tx, FFT taps) aren't in `base`, so they
/// drop out naturally; browser blocks aren't in `applied`, so they
/// keep their authored params.
fn overlay_live_params(base: &mut FlowgraphDoc, applied: &FlowgraphDoc) {
    for (id, decl) in &mut base.blocks {
        if id == SOURCE_ID {
            continue;
        }
        if let Some(live) = applied.blocks.get(id) {
            decl.params.clone_from(&live.params);
        }
    }
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

/// Resolve a catalog `slug` to its `FlowgraphDoc` (the runtime mirror
/// of the browser catalog expansion; see
/// `docs/14-fldigi-catalog-variants.md`).
///
///  * `slug.json` exists *and* declares no variants → that singleton.
///  * `slug.json` exists *and* declares variants → error: a base with
///    variants is never addressable bare (hard cut, D-V4).
///  * else `slug == "${base}-${variantId}"` for some `base.json` whose
///    `variants` contains `variantId` → base with that variant's
///    `patch` overlaid. Global slug uniqueness (validator D-V4) makes
///    the split unambiguous in practice; the first base file that both
///    exists and declares the suffix wins.
fn resolve_preset_doc(dir: &std::path::Path, slug: &str) -> Result<FlowgraphDoc> {
    let read = |stem: &str| -> Result<Option<FlowgraphDoc>> {
        let f = dir.join(format!("{stem}.json"));
        match std::fs::read(&f) {
            Ok(b) => Ok(Some(
                FlowgraphDoc::from_json(&b).map_err(|e| anyhow!("parse preset {stem:?}: {e:#}"))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow!("read preset {stem:?}: {e}")),
        }
    };

    if let Some(doc) = read(slug)? {
        if doc.variants.is_empty() {
            return Ok(doc);
        }
        return Err(anyhow!(
            "preset {slug:?} has variants — address a specific variant as \
             {slug}-<id> (a base with variants is not loadable bare)"
        ));
    }

    // `slug` must be `${base}-${variantId}`. Try every `-` split.
    for (i, _) in slug.match_indices('-') {
        let (base, rest) = (&slug[..i], &slug[i + 1..]);
        if base.is_empty() || rest.is_empty() {
            continue;
        }
        let Some(mut doc) = read(base)? else { continue };
        if let Some(v) = doc.variants.iter().find(|v| v.id == rest).cloned() {
            doc.apply_variant_patch(&v.patch);
            return Ok(doc);
        }
    }

    Err(anyhow!("unknown preset {slug:?}"))
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

/// The handful of sidecar fields the capture picker surfaces.
#[derive(Default)]
struct Sidecar {
    name: Option<String>,
    format: Option<String>,
    sample_rate_hz: Option<f64>,
    center_freq_hz: Option<f64>,
    modulation: Option<String>,
}

/// Look for `<file>.json` then `<stem>.json` beside a capture and pull
/// the few fields the picker shows. Missing/!json → all `None`.
fn read_sidecar(path: &std::path::Path) -> Sidecar {
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
        return Sidecar::default();
    };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let n = |k: &str| v.get(k).and_then(serde_json::Value::as_f64);
    Sidecar {
        name: s("name"),
        format: s("format"),
        sample_rate_hz: n("sample_rate_hz"),
        center_freq_hz: n("center_freq_hz"),
        modulation: s("modulation"),
    }
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
            let Sidecar {
                name,
                format,
                sample_rate_hz,
                center_freq_hz,
                modulation,
            } = read_sidecar(&path);
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

    /// Preset with a node-side DSP block (`dec`) between the source and
    /// a browser sink — gives `apply_block_params` a non-source node
    /// block to drive the live-reconfigure path against.
    fn node_dsp_preset() -> FlowgraphDoc {
        serde_json::from_value(json!({
            "name": "t",
            "environments": ["node", "browser"],
            "blocks": {
                "src":  { "type": "Source", "placement": "node",
                          "params": { "center_freq_hz": 0.0, "sample_rate_hz": 1000.0 } },
                "dec":  { "type": "Decimator", "placement": "node",
                          "params": { "factor": 2, "num_taps": 17,
                                      "cutoff_normalized": 0.2 } },
                "sink": { "type": "Decimator", "placement": "browser",
                          "params": { "factor": 2, "num_taps": 17,
                                      "cutoff_normalized": 0.2 } }
            },
            "wires": [["src.out", "dec.in"], ["dec.out", "sink.in"]]
        }))
        .unwrap()
    }

    fn block_factor(blocks: &[PipelineBlock], id: &str) -> i64 {
        blocks
            .iter()
            .find(|b| b.id == id)
            .unwrap_or_else(|| panic!("block {id} missing"))
            .values["factor"]
            .as_i64()
            .unwrap_or_else(|| panic!("block {id} factor not an int"))
    }

    /// The running pipeline is the single doc authority: a live
    /// non-source node edit is visible through `list_blocks` /
    /// `get_flowgraph` via the runtime's `applied_doc` (no per-edit
    /// preset_doc mirror-back), and `stop()` folds it back so it
    /// survives a restart. Browser blocks keep their authored params
    /// throughout (they have no runtime authority).
    #[tokio::test]
    async fn running_pipeline_is_single_doc_authority() {
        let state = AppState::new(node_dsp_preset(), test_source(), Duration::from_millis(5));
        state.start().await.unwrap();

        let plan = state
            .apply_block_params("dec", json!({ "factor": 4 }))
            .await
            .expect("live reconfigure ok");
        assert!(plan.is_some(), "running pipeline must return a plan");

        // Reader-of-record while live = the runtime's applied_doc.
        let blocks = state.list_blocks().await.unwrap();
        assert_eq!(
            block_factor(&blocks, "dec"),
            4,
            "list_blocks sees live edit"
        );
        assert_eq!(
            block_factor(&blocks, "sink"),
            2,
            "browser block keeps authored params (no runtime authority)"
        );
        let fg = state.get_flowgraph().await;
        assert_eq!(
            fg.blocks["dec"].params.as_ref().unwrap()["factor"].as_i64(),
            Some(4),
            "get_flowgraph reads through applied_doc while running"
        );

        // stop() is the single deliberate writer: it folds the live
        // state back into preset_doc (proven by the *stopped* read,
        // which has no overlay).
        assert!(state.stop().await);
        let fg = state.get_flowgraph().await;
        assert_eq!(
            fg.blocks["dec"].params.as_ref().unwrap()["factor"].as_i64(),
            Some(4),
            "stop() folded the live edit into preset_doc"
        );

        // Survives a restart — start() recomposes from preset_doc.
        state.start().await.unwrap();
        let blocks = state.list_blocks().await.unwrap();
        assert_eq!(
            block_factor(&blocks, "dec"),
            4,
            "live edit persists across stop→start"
        );
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

    #[test]
    fn resolve_preset_doc_singleton_base_variant_and_errors() {
        let dir = tempdir().unwrap();
        let write = |name: &str, json: serde_json::Value| {
            std::fs::write(
                dir.path().join(format!("{name}.json")),
                serde_json::to_vec(&json).unwrap(),
            )
            .unwrap();
        };

        // Singleton — and one whose own name contains '-' (must resolve
        // directly, not be mistaken for a `${base}-${id}` split).
        write(
            "rtl433-433",
            json!({ "name": "rtl433-433", "environments": ["node"],
                    "blocks": { "a": { "type": "X" } }, "wires": [] }),
        );
        // Base with variants: default keeps base params, `wide` patches.
        write(
            "olivia",
            json!({
                "name": "olivia", "environments": ["node"],
                "blocks": { "demod": { "type": "OliviaDemod",
                                       "params": { "variant": "olivia-8-500", "afc": true } } },
                "wires": [],
                "variants": [
                    { "id": "8-500", "default": true, "patch": {} },
                    { "id": "16-500", "patch": { "demod": { "variant": "olivia-16-500" } } }
                ]
            }),
        );

        let d = resolve_preset_doc(dir.path(), "rtl433-433").unwrap();
        assert_eq!(d.name, "rtl433-433");

        // Bare base with variants is not loadable (hard cut, D-V4).
        let err = resolve_preset_doc(dir.path(), "olivia").unwrap_err();
        assert!(format!("{err:#}").contains("has variants"), "{err:#}");

        // Variant slug resolves and applies the patch.
        let d = resolve_preset_doc(dir.path(), "olivia-16-500").unwrap();
        let v = d.blocks["demod"].params.as_ref().unwrap();
        assert_eq!(v["variant"], "olivia-16-500");
        assert_eq!(v["afc"], true, "base params preserved under merge");

        // Default variant slug resolves to base params unchanged.
        let d = resolve_preset_doc(dir.path(), "olivia-8-500").unwrap();
        assert_eq!(
            d.blocks["demod"].params.as_ref().unwrap()["variant"],
            "olivia-8-500"
        );

        assert!(resolve_preset_doc(dir.path(), "nope").is_err());
        assert!(resolve_preset_doc(dir.path(), "olivia-9999").is_err());
    }
}
