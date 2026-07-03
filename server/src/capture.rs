//! Server-side capture orchestration.
//!
//! ferrited owns the capture job registry and the two capture state
//! machines. `POST /api/capture/{iq,fft,audio}` register a job, return
//! it immediately, and spawn a background task that drives the running
//! pipeline; `GET /api/capture/jobs[/:id]` polls status. ferrite-ctl's
//! MCP verbs are thin wrappers over these endpoints.
//!
//! This placement is deliberate. The orchestration used to live in
//! ferrite-ctl (a *separate process*), which drove captures by looping
//! HTTP calls back into ferrited and then reading the recording block's
//! sidecar JSON off ferrited's local disk. Two problems fell out of
//! that:
//!
//!   1. **The antenna-inherit trap** ([`capture_source_config`]): the
//!      preset-swap path rebuilt the capture source from scratch
//!      (freq/rate/bw/gain), silently dropping the operator's live
//!      antenna port + broadcast notch. Server-side we reuse the live
//!      [`SourceConfig`] verbatim and override only the tuning knobs, so
//!      the capture reads the same front-end the operator was on.
//!   2. **The sidecar-filesystem coupling**: ferrite-ctl read the
//!      sidecar over its own filesystem view (broken the moment it runs
//!      off-box) and, worse, guessed the wrong sidecar name — it looked
//!      for `<path>.<ext>.json` while every recording block writes
//!      `<path>.json` ([`sidecar_path_for`]). The `sidecar` field of a
//!      finished job was therefore always `null`. ferrited reads its own
//!      sidecar with the block's naming and folds it into the job.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use ferrite_runtime::SourceConfig;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::app_state::AppState;

/// LRU cap on the per-process job registry.
const JOBS_CAP: usize = 20;

/// What a capture job is recording. `Iq`/`Fft` use the non-disruptive
/// live-record tee (`chan`/`logmag`); `Audio` and wideband `Iq` swap in
/// a recording preset and run a `Source → FileSink` slice.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CaptureKind {
    Iq,
    Fft,
    Audio,
}

impl CaptureKind {
    /// Live-record block id for the tee path. Only meaningful for the
    /// live `Iq`/`Fft` kinds (`Audio` never tees).
    fn live_block_id(self) -> &'static str {
        match self {
            Self::Iq => "chan",
            Self::Fft => "logmag",
            Self::Audio => "audio",
        }
    }
    fn ext(self) -> &'static str {
        match self {
            Self::Iq => "cf32",
            Self::Fft => "bin",
            Self::Audio => "wav",
        }
    }
}

/// State of one async capture. Registered `Running`; the spawned task
/// transitions it to `Done` (path + sidecar ready) or `Failed`.
#[derive(Debug, Clone)]
enum JobStatus {
    Running,
    Done,
    Failed { error: String },
}

/// One capture job in the registry.
#[derive(Debug, Clone)]
pub struct CaptureJob {
    job_id: String,
    kind: CaptureKind,
    status: JobStatus,
    output_path: String,
    duration_s: f64,
    /// Unix ms — set when the job is registered.
    started_at: u128,
    /// Unix ms — set when the task leaves `Running`.
    finished_at: Option<u128>,
    /// Sidecar JSON the recording block wrote next to `output_path`;
    /// populated on success.
    sidecar: Option<Value>,
}

// Manual `Serialize` so `status` is a flat string ("running"/"done"/
// "failed") with a sibling `error`, matching what `capture_status`
// promises and what the ferrite-ctl poll loop reads.
impl Serialize for CaptureJob {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let (status, error) = match &self.status {
            JobStatus::Running => ("running", None),
            JobStatus::Done => ("done", None),
            JobStatus::Failed { error } => ("failed", Some(error.as_str())),
        };
        let mut st = s.serialize_struct("CaptureJob", 9)?;
        st.serialize_field("job_id", &self.job_id)?;
        st.serialize_field("kind", &self.kind)?;
        st.serialize_field("status", status)?;
        st.serialize_field("error", &error)?;
        st.serialize_field("output_path", &self.output_path)?;
        st.serialize_field("duration_s", &self.duration_s)?;
        st.serialize_field("started_at", &self.started_at)?;
        st.serialize_field("finished_at", &self.finished_at)?;
        st.serialize_field("sidecar", &self.sidecar)?;
        st.end()
    }
}

/// Per-process capture job registry (jobs vec + monotonic id counter).
/// Lives in [`AppState`]; survives start/stop like the frame bus.
#[derive(Debug)]
pub struct CaptureRegistry {
    jobs: RwLock<Vec<CaptureJob>>,
    next_job: AtomicU64,
}

impl CaptureRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            jobs: RwLock::new(Vec::new()),
            next_job: AtomicU64::new(1),
        }
    }

    async fn upsert(&self, job: CaptureJob) {
        let mut guard = self.jobs.write().await;
        if let Some(pos) = guard.iter().position(|j| j.job_id == job.job_id) {
            guard[pos] = job;
        } else {
            guard.insert(0, job);
            guard.truncate(JOBS_CAP);
        }
    }

    /// Register a fresh `Running` job and return it.
    async fn register(&self, kind: CaptureKind, out: &Path, duration_s: f64) -> CaptureJob {
        let job = CaptureJob {
            job_id: format!("cap-{}", self.next_job.fetch_add(1, Ordering::SeqCst)),
            kind,
            status: JobStatus::Running,
            output_path: out.display().to_string(),
            duration_s,
            started_at: now_ms(),
            finished_at: None,
            sidecar: None,
        };
        self.upsert(job.clone()).await;
        job
    }

    /// Mark a job `Done`/`Failed`, attach the sidecar, stamp
    /// `finished_at`. Shared tail of every capture task.
    async fn finish(&self, job_id: &str, result: Result<(), String>, sidecar: Option<Value>) {
        let status = match &result {
            Ok(()) => JobStatus::Done,
            Err(e) => JobStatus::Failed { error: e.clone() },
        };
        let existing = {
            let g = self.jobs.read().await;
            g.iter().find(|j| j.job_id == job_id).cloned()
        };
        if let Some(mut j) = existing {
            j.status = status;
            j.finished_at = Some(now_ms());
            j.sidecar = sidecar;
            self.upsert(j).await;
        }
    }

    /// Every known job, newest first (already in insert order).
    pub async fn snapshot(&self) -> Vec<CaptureJob> {
        self.jobs.read().await.clone()
    }

    /// One job by id.
    pub async fn get(&self, job_id: &str) -> Option<CaptureJob> {
        self.jobs
            .read()
            .await
            .iter()
            .find(|j| j.job_id == job_id)
            .cloned()
    }
}

impl Default for CaptureRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Why a `POST /api/capture/*` was rejected *before* a job was spawned.
/// (Failures once the job is running land in `job.status = failed`, not
/// here.)
#[derive(Debug)]
pub enum CaptureError {
    /// Bad args or a refused precondition → 400.
    Invalid(String),
    /// Server-side setup failure (e.g. couldn't create the output dir)
    /// → 500.
    Internal(String),
}

// ─── request bodies ─────────────────────────────────────────────────────
//
// The wire contract with ferrite-ctl's `StartCapture*Args`. Field names
// match; every optional field is `#[serde(default)]` so a `null` (what
// ferrite-ctl serializes for an unset `Option`) deserializes to `None`.

#[derive(Debug, Default, Deserialize)]
pub struct CaptureIqReq {
    pub duration_s: f64,
    #[serde(default)]
    pub freq_hz: Option<f64>,
    #[serde(default)]
    pub out: Option<String>,
    /// IQ only. `false` = non-disruptive live narrowband tee; `true` =
    /// full-rate wideband `Source → FileIqSink` (needs `freq_hz`).
    #[serde(default)]
    pub wideband: bool,
    #[serde(default)]
    pub sample_rate_hz: Option<f64>,
    #[serde(default)]
    pub bandwidth_hz: Option<f64>,
    #[serde(default)]
    pub gain_db: Option<f64>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Deserialize)]
pub struct CaptureAudioReq {
    pub freq_hz: f64,
    #[serde(default)]
    pub duration_s: Option<f64>,
    #[serde(default)]
    pub sample_rate_hz: Option<f64>,
    #[serde(default)]
    pub bandwidth_hz: Option<f64>,
    #[serde(default)]
    pub gain_db: Option<f64>,
    #[serde(default)]
    pub preset: Option<String>,
    #[serde(default)]
    pub out: Option<String>,
    #[serde(default)]
    pub force: bool,
}

// ─── pure helpers ───────────────────────────────────────────────────────

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn default_capture_path(kind: CaptureKind, freq_hz: f64) -> PathBuf {
    let mhz = (freq_hz / 1e6).round();
    let stem = format!("{kind:?}-{}-{mhz:.0}mhz", now_ms()).to_lowercase();
    PathBuf::from("/tmp/ferrite-captures").join(format!("{stem}.{}", kind.ext()))
}

/// The sidecar path a recording block writes for `bin_path`: replace the
/// final extension with `json`, or append `.json` when there is none.
/// **Must match** the block-side `sidecar_path_for` in
/// `blocks/src/{channelizer,log_mag_u8,file_sink}.rs` — a capture writes
/// `foo.cf32` and its sidecar lands at `foo.json`, not `foo.cf32.json`.
fn sidecar_path_for(bin_path: &Path) -> PathBuf {
    let mut p = bin_path.to_path_buf();
    if p.extension().is_some() {
        p.set_extension("json");
    } else {
        let mut s = p.into_os_string();
        s.push(".json");
        p = PathBuf::from(s);
    }
    p
}

/// Read the sidecar JSON the recording block wrote next to `out_path`.
/// Best-effort — `None` if absent / unparseable.
async fn read_sidecar(out_path: &Path) -> Option<Value> {
    let side = sidecar_path_for(out_path);
    match tokio::fs::read(&side).await {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes).ok(),
        Err(_) => None,
    }
}

/// Build the source config for a preset-swap capture by **reusing the
/// live source** — antenna port, SDRplay broadcast-notch `settings`,
/// driver args, everything — and overriding only the tuning knobs the
/// capture pins. This is the structural fix for the capture-antenna-
/// inherit bug: the old path rebuilt a source from `{center, rate, bw,
/// gain}` alone, so a wideband capture silently dropped the operator's
/// Antenna C + MW notch and came back as noise. `gain_db = None` keeps
/// the live gain.
#[must_use]
pub fn capture_source_config(
    live: &SourceConfig,
    freq_hz: f64,
    sample_rate_hz: f64,
    bandwidth_hz: f64,
    gain_db: Option<f64>,
) -> SourceConfig {
    let mut src = live.clone();
    // SourceConfig.params is a free-form Value; normalise to an object so
    // the overrides always land even if the live source had `null` params.
    let mut params = src.params.as_object().cloned().unwrap_or_default();
    params.insert("center_freq_hz".into(), json!(freq_hz));
    params.insert("sample_rate_hz".into(), json!(sample_rate_hz));
    params.insert("bandwidth_hz".into(), json!(bandwidth_hz));
    if let Some(g) = gain_db {
        params.insert("gain_db".into(), json!(g));
    }
    src.params = Value::Object(params);
    src
}

async fn ensure_parent(out: &Path) -> Result<(), CaptureError> {
    if let Some(parent) = out.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| CaptureError::Internal(format!("create {}: {e}", parent.display())))?;
    }
    Ok(())
}

fn validate_duration(d: f64) -> Result<(), CaptureError> {
    if d.is_finite() && d > 0.0 {
        Ok(())
    } else {
        Err(CaptureError::Invalid(format!(
            "duration_s must be a finite positive number, got {d}"
        )))
    }
}

// ─── task runners ───────────────────────────────────────────────────────

/// Live-record tee (`Iq`/`Fft`): patch the block's `record_path` +
/// `record_max_seconds`, sleep, clear it. Non-disruptive — the UI
/// session keeps running. Auto-starts a stopped pipeline first (the live
/// patch hits a block instance, which only exists while sampling).
async fn run_live_capture(
    state: AppState,
    job_id: String,
    kind: CaptureKind,
    duration_s: f64,
    freq_hz: Option<f64>,
    out_path: PathBuf,
) {
    let block = kind.live_block_id();
    let path_str = out_path.display().to_string();
    let result: Result<(), String> = async {
        if let Some(f) = freq_hz {
            // offset_ratio 0: the live tee rides whatever the pipeline is
            // already doing, so land the retune on-centre.
            state
                .tune(f, None, Some(0.0), false)
                .await
                .map_err(|e| format!("retune: {e:#}"))?;
        }
        // The live patch needs a running pipeline — auto-start if stopped.
        if state.status().await != crate::app_state::PipelineStatus::Running {
            state
                .start()
                .await
                .map_err(|e| format!("auto-start pipeline: {e:#}"))?;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        state
            .apply_block_params(
                block,
                json!({ "record_path": path_str, "record_max_seconds": duration_s }),
            )
            .await
            .map_err(|e| format!("start recording on `{block}`: {e:#}"))?;
        tokio::time::sleep(Duration::from_secs_f64(duration_s + 0.5)).await;
        // Belt-and-braces stop in case a missed tick left it recording.
        let _ = state
            .apply_block_params(block, json!({ "record_path": "" }))
            .await;
        Ok(())
    }
    .await;
    let sidecar = if result.is_ok() {
        read_sidecar(&out_path).await
    } else {
        None
    };
    state.captures().finish(&job_id, result, sidecar).await;
}

/// Preset-swap (`Audio`, wideband `Iq`): load a recording preset, apply
/// the live-source-derived capture source + the sink params, start,
/// sleep, stop. Disruptive — clobbers the live session.
#[allow(clippy::too_many_arguments)]
async fn run_preset_capture(
    state: AppState,
    job_id: String,
    kind: CaptureKind,
    preset: String,
    cap_source: SourceConfig,
    block_id: String,
    block_patch: Value,
    duration_s: f64,
    out_path: PathBuf,
) {
    let result: Result<(), String> = async {
        state
            .load_preset_by_name(&preset)
            .await
            .map_err(|e| format!("load preset `{preset}`: {e:#}"))?;
        state
            .patch_source(cap_source)
            .await
            .map_err(|e| format!("patch source: {e:#}"))?;
        state
            .apply_block_params(&block_id, block_patch)
            .await
            .map_err(|e| format!("patch `{block_id}`: {e:#}"))?;
        state
            .start()
            .await
            .map_err(|e| format!("start pipeline: {e:#}"))?;
        tokio::time::sleep(Duration::from_secs_f64(duration_s + 0.5)).await;
        state.stop().await;
        Ok(())
    }
    .await;
    let sidecar = if result.is_ok() {
        read_sidecar(&out_path).await
    } else {
        None
    };
    let _ = kind; // kind is carried for symmetry / future per-kind sidecar hints
    state.captures().finish(&job_id, result, sidecar).await;
}

// ─── orchestration entry points (called by routes) ──────────────────────

impl AppState {
    /// Every known capture job, newest first.
    pub async fn capture_jobs(&self) -> Vec<CaptureJob> {
        self.captures().snapshot().await
    }

    /// One capture job by id.
    pub async fn capture_job(&self, job_id: &str) -> Option<CaptureJob> {
        self.captures().get(job_id).await
    }

    /// `POST /api/capture/iq`. Non-wideband tees the live post-channelizer
    /// stream; wideband swaps in `capture_fm` for a full-rate slice.
    pub async fn start_capture_iq(&self, req: CaptureIqReq) -> Result<CaptureJob, CaptureError> {
        validate_duration(req.duration_s)?;
        if req.wideband {
            return self.start_wideband_iq(req).await;
        }
        let out = self
            .resolve_out(req.out, CaptureKind::Iq, req.freq_hz)
            .await;
        ensure_parent(&out).await?;
        let job = self
            .captures()
            .register(CaptureKind::Iq, &out, req.duration_s)
            .await;
        let (state, job_id) = (self.clone(), job.job_id.clone());
        let duration_s = req.duration_s;
        let freq_hz = req.freq_hz;
        tokio::spawn(async move {
            run_live_capture(state, job_id, CaptureKind::Iq, duration_s, freq_hz, out).await;
        });
        Ok(job)
    }

    /// `POST /api/capture/fft`. Always the non-disruptive `logmag` tee.
    pub async fn start_capture_fft(&self, req: CaptureIqReq) -> Result<CaptureJob, CaptureError> {
        validate_duration(req.duration_s)?;
        let out = self
            .resolve_out(req.out, CaptureKind::Fft, req.freq_hz)
            .await;
        ensure_parent(&out).await?;
        let job = self
            .captures()
            .register(CaptureKind::Fft, &out, req.duration_s)
            .await;
        let (state, job_id) = (self.clone(), job.job_id.clone());
        let duration_s = req.duration_s;
        let freq_hz = req.freq_hz;
        tokio::spawn(async move {
            run_live_capture(state, job_id, CaptureKind::Fft, duration_s, freq_hz, out).await;
        });
        Ok(job)
    }

    /// `POST /api/capture/audio`. Preset-swap recording to a WAV.
    pub async fn start_capture_audio(
        &self,
        req: CaptureAudioReq,
    ) -> Result<CaptureJob, CaptureError> {
        let duration = req.duration_s.unwrap_or(10.0);
        validate_duration(duration)?;
        self.guard_active_audio_profile(req.force, "start_capture_audio")
            .await?;
        self.guard_hardware_source(req.force, "audio capture")
            .await?;
        let rate = req.sample_rate_hz.unwrap_or(2_400_000.0);
        let bw = req.bandwidth_hz.unwrap_or(rate);
        let preset = req.preset.unwrap_or_else(|| "fm-audio-record".into());
        let out = match req.out {
            Some(s) => PathBuf::from(s),
            None => default_capture_path(CaptureKind::Audio, req.freq_hz),
        };
        ensure_parent(&out).await?;
        let cap_source =
            capture_source_config(&self.get_source().await, req.freq_hz, rate, bw, req.gain_db);
        let block_patch = json!({ "path": out.display().to_string(), "max_seconds": duration });
        let job = self
            .captures()
            .register(CaptureKind::Audio, &out, duration)
            .await;
        let (state, job_id) = (self.clone(), job.job_id.clone());
        tokio::spawn(async move {
            run_preset_capture(
                state,
                job_id,
                CaptureKind::Audio,
                preset,
                cap_source,
                "audio".into(),
                block_patch,
                duration,
                out,
            )
            .await;
        });
        Ok(job)
    }

    /// Wideband IQ: swap in `capture_fm`, run a `Source → FileIqSink`
    /// slice off the (live-inherited) source. Needs `freq_hz`.
    async fn start_wideband_iq(&self, req: CaptureIqReq) -> Result<CaptureJob, CaptureError> {
        let freq = req.freq_hz.ok_or_else(|| {
            CaptureError::Invalid("wideband IQ capture requires `freq_hz`".into())
        })?;
        self.guard_active_audio_profile(req.force, "wideband IQ capture")
            .await?;
        self.guard_hardware_source(req.force, "wideband IQ capture")
            .await?;
        let rate = req.sample_rate_hz.unwrap_or(2_000_000.0);
        let bw = req.bandwidth_hz.unwrap_or(rate);
        let format = req.format.unwrap_or_else(|| "cf32".into());
        let out = match req.out {
            Some(s) => PathBuf::from(s),
            None => default_capture_path(CaptureKind::Iq, freq),
        };
        ensure_parent(&out).await?;
        let cap_source =
            capture_source_config(&self.get_source().await, freq, rate, bw, req.gain_db);
        let block_patch = json!({
            "path": out.display().to_string(),
            "format": format,
            "rate_hz": rate,
            "center_freq_hz": freq,
            "max_seconds": req.duration_s,
            "write_sidecar": true,
        });
        let job = self
            .captures()
            .register(CaptureKind::Iq, &out, req.duration_s)
            .await;
        let (state, job_id) = (self.clone(), job.job_id.clone());
        let duration = req.duration_s;
        tokio::spawn(async move {
            run_preset_capture(
                state,
                job_id,
                CaptureKind::Iq,
                "capture_fm".into(),
                cap_source,
                "cap".into(),
                block_patch,
                duration,
                out,
            )
            .await;
        });
        Ok(job)
    }

    /// Resolve a capture output path, defaulting to the live source freq
    /// (so the filename carries a useful hint) when none is supplied.
    async fn resolve_out(
        &self,
        out: Option<String>,
        kind: CaptureKind,
        freq_hz: Option<f64>,
    ) -> PathBuf {
        if let Some(s) = out {
            return PathBuf::from(s);
        }
        let freq = if let Some(f) = freq_hz {
            f
        } else {
            self.get_source()
                .await
                .params
                .get("center_freq_hz")
                .and_then(Value::as_f64)
                .unwrap_or(0.0)
        };
        default_capture_path(kind, freq)
    }

    /// Refuse a preset-swapping capture that would silently tear down an
    /// active transcription session (the recording preset replaces the
    /// whole running graph, dropping the injected `VoiceTranscribe` tap).
    async fn guard_active_audio_profile(
        &self,
        force: bool,
        what: &str,
    ) -> Result<(), CaptureError> {
        if force {
            return Ok(());
        }
        if self.get_profile().await.transcribe {
            return Err(CaptureError::Invalid(format!(
                "{what} loads a recording preset, which would tear down the active \
                 transcription session (the running graph is replaced). Stop transcription \
                 first (`transcribe` enabled=false) or pass `force: true` to proceed anyway."
            )));
        }
        Ok(())
    }

    /// Refuse a capture against a software/test source — a preset-swap
    /// capture inherits the live source, and a SineSource yields
    /// plausible-looking noise, not RF.
    async fn guard_hardware_source(&self, force: bool, what: &str) -> Result<(), CaptureError> {
        if force {
            return Ok(());
        }
        let type_name = self.get_source().await.type_name;
        if type_name != "SoapySource" {
            let what_src = if type_name.is_empty() {
                "a non-hardware source".to_string()
            } else {
                format!("a {type_name}")
            };
            return Err(CaptureError::Invalid(format!(
                "{what} would run against {what_src} — a software/test source, not real RF, \
                 so you'd record noise that looks like a signal. Select a device first \
                 (`device select <args>`) or pass `force: true` to capture the synthetic source."
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_rspdx() -> SourceConfig {
        SourceConfig {
            type_name: "SoapySource".into(),
            params: json!({
                "args": "driver=sdrplay",
                "center_freq_hz": 151_000.0,
                "sample_rate_hz": 2_000_000.0,
                "bandwidth_hz": 1_536_000.0,
                "gain_db": 45.0,
                "antenna": "Antenna C",
                "settings": { "rfnotch_ctrl": true, "hdr_ctrl": false },
            }),
        }
    }

    // The regression that pins the antenna-inherit fix: a preset-swap
    // capture must carry the operator's antenna + notch forward, and
    // override only the tuning knobs. Rebuild-from-scratch (the old bug)
    // drops `antenna`/`settings` and this fails.
    #[test]
    fn capture_source_inherits_antenna_and_notch() {
        let live = live_rspdx();
        let cap = capture_source_config(&live, 100_100_000.0, 2_000_000.0, 1_536_000.0, None);
        let p = cap.params.as_object().unwrap();

        // Preserved from the live front-end.
        assert_eq!(cap.type_name, "SoapySource");
        assert_eq!(p.get("antenna").and_then(Value::as_str), Some("Antenna C"));
        assert_eq!(
            p.get("settings").and_then(|s| s.get("rfnotch_ctrl")),
            Some(&json!(true))
        );
        assert_eq!(
            p.get("args").and_then(Value::as_str),
            Some("driver=sdrplay")
        );
        // gain_db=None keeps the live gain.
        assert_eq!(p.get("gain_db").and_then(Value::as_f64), Some(45.0));

        // Overridden by the capture.
        assert_eq!(
            p.get("center_freq_hz").and_then(Value::as_f64),
            Some(100_100_000.0)
        );
    }

    #[test]
    fn capture_source_gain_override_wins() {
        let cap = capture_source_config(&live_rspdx(), 100e6, 2e6, 2e6, Some(20.0));
        let p = cap.params.as_object().unwrap();
        assert_eq!(p.get("gain_db").and_then(Value::as_f64), Some(20.0));
        // still inherits antenna
        assert_eq!(p.get("antenna").and_then(Value::as_str), Some("Antenna C"));
    }

    #[test]
    fn capture_source_normalises_null_params() {
        let live = SourceConfig {
            type_name: "SoapySource".into(),
            params: Value::Null,
        };
        let cap = capture_source_config(&live, 100e6, 2e6, 2e6, Some(30.0));
        let p = cap.params.as_object().unwrap();
        assert_eq!(p.get("center_freq_hz").and_then(Value::as_f64), Some(100e6));
        assert_eq!(p.get("gain_db").and_then(Value::as_f64), Some(30.0));
    }

    // Locks the sidecar-naming fix: `foo.cf32` → `foo.json`, matching the
    // recording blocks (not `foo.cf32.json`, the old ferrite-ctl guess).
    #[test]
    fn sidecar_path_matches_block_convention() {
        assert_eq!(
            sidecar_path_for(Path::new("/tmp/iq-1-100mhz.cf32")),
            PathBuf::from("/tmp/iq-1-100mhz.json")
        );
        assert_eq!(
            sidecar_path_for(Path::new("/tmp/fft.bin")),
            PathBuf::from("/tmp/fft.json")
        );
        assert_eq!(
            sidecar_path_for(Path::new("/tmp/noext")),
            PathBuf::from("/tmp/noext.json")
        );
    }
}
