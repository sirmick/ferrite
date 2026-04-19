//! Session — a running source pipeline streaming WS frames.
//!
//! For this commit there is exactly one source kind (sine) and one output
//! stream (waterfall FFT, [`PayloadType::FftU8`]). Real device drivers and
//! VFOs land in later commits; the session shape is what they will reuse.
//!
//! Concurrency: the pipeline ticks in its own tokio task at `fft_rate_hz`,
//! pushing framed bytes into a broadcast channel. Each WebSocket connection
//! subscribes and forwards. A single oneshot serves as the kill switch.

use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use ferrite_blocks::{
    Block, BlockIo, FftBlock, FftBlockParams, FftWindow, FileIqSource, FileIqSourceParams, InBuf,
    InputPort, LogMagU8, LogMagU8Params, OutBuf, OutputPort, PortMeta, SineSource,
    SineSourceParams,
};
use num_complex::Complex;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock};

use crate::ws_frame::{encode_into, FrameHeader, PayloadType, CONTROL_STREAM, FFT_STREAM};

/// Server-wide source selection. Set once at startup via CLI; every session
/// opened afterwards uses this kind, unless the open request explicitly
/// names a `SoapySDR` device — in which case the per-session [`Soapy`]
/// kind takes precedence (see [`AppState::open`]).
///
/// [`Soapy`]: SourceKind::Soapy
#[derive(Debug, Clone, Default)]
pub enum SourceKind {
    #[default]
    Sine,
    File {
        path: PathBuf,
        loop_playback: bool,
    },
    /// Live `SoapySDR` device. Constructed per session from the
    /// `device_args` field of the open request.
    #[cfg(feature = "soapysdr")]
    Soapy {
        args: String,
        antenna: Option<String>,
        gain_db: Option<f64>,
        agc: Option<bool>,
        bandwidth_hz: Option<f64>,
    },
}

/// CLI-derived configuration wired into [`AppState`] at startup.
#[derive(Debug, Clone, Default)]
pub struct CliConfig {
    pub source: SourceKind,
    /// Applied to [`OpenSpec`] defaults so the client's slider starts at
    /// the right value. WAV files override this from their own header.
    pub rate_override_hz: Option<f64>,
    /// Applied to [`OpenSpec`] defaults (IQ files do not carry it).
    pub center_override_hz: Option<f64>,
}

pub type FrameBytes = Arc<Vec<u8>>;
pub type FrameTx = broadcast::Sender<FrameBytes>;

/// User-supplied open spec — every field has a default.
#[derive(Debug, Clone, Copy)]
pub struct OpenSpec {
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    pub tone_freq_abs_hz: f64,
    pub amplitude: f32,
    pub fft_size: usize,
    pub fft_rate_hz: f32,
    pub floor_dbfs: f32,
    pub ceil_dbfs: f32,
    pub alpha: f32,
}

impl Default for OpenSpec {
    fn default() -> Self {
        Self {
            sample_rate_hz: 2_000_000.0,
            center_freq_hz: 100_000_000.0,
            tone_freq_abs_hz: 100_001_000.0,
            amplitude: 0.25,
            fft_size: 4096,
            fft_rate_hz: 30.0,
            floor_dbfs: -100.0,
            ceil_dbfs: 0.0,
            alpha: 0.3,
        }
    }
}

/// Descriptor returned by [`AppState::open`] — what the REST handler echoes.
#[derive(Debug, Clone)]
pub struct OpenedSession {
    pub id: String,
    pub fft_size: usize,
    pub fft_rate_hz: f32,
}

/// JSON-shape snapshot of the active session — what `GET /api/device/{id}/state`
/// returns. Optional fields are populated only when the field is meaningful
/// for the current source (e.g. `gain_db` is `None` for a sine source).
#[derive(Debug, Clone, Serialize)]
pub struct SessionState {
    pub id: String,
    pub source_kind: &'static str,
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    pub fft_size: usize,
    pub fft_rate_hz: f32,
    pub floor_dbfs: f32,
    pub ceil_dbfs: f32,
    pub alpha: f32,
    pub tone_freq_abs_hz: Option<f64>,
    pub amplitude: Option<f32>,
    pub gain_db: Option<f64>,
    pub antenna: Option<String>,
    pub agc: Option<bool>,
}

/// Field-update spec for `PATCH /api/device/{id}/settings`. Every field is
/// optional — only set fields are applied. Per `docs/02-protocol.md` the
/// listed knobs are mutable while streaming; the request silently no-ops
/// fields that the active source does not support (e.g. `gain_db` on a
/// sine source) rather than failing.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PatchRequest {
    pub center_freq_hz: Option<f64>,
    pub tone_freq_abs_hz: Option<f64>,
    pub amplitude: Option<f32>,
    pub gain_db: Option<f64>,
    pub agc: Option<bool>,
    pub floor_dbfs: Option<f32>,
    pub ceil_dbfs: Option<f32>,
    pub alpha: Option<f32>,
}

impl PatchRequest {
    fn is_noop(&self) -> bool {
        self.center_freq_hz.is_none()
            && self.tone_freq_abs_hz.is_none()
            && self.amplitude.is_none()
            && self.gain_db.is_none()
            && self.agc.is_none()
            && self.floor_dbfs.is_none()
            && self.ceil_dbfs.is_none()
            && self.alpha.is_none()
    }
}

enum ControlCmd {
    Patch(PatchRequest),
}

struct Session {
    id: String,
    frames: FrameTx,
    /// Taken-and-fired by `close()`; the pipeline task watches this.
    shutdown: Option<oneshot::Sender<()>>,
    control: mpsc::UnboundedSender<ControlCmd>,
    /// Last-known snapshot, refreshed by the pipeline task as it applies
    /// patches and reads back hardware values.
    state: Arc<Mutex<SessionState>>,
}

struct Inner {
    next_id: AtomicU64,
    /// Single-listener: at most one session is active. A second `open()`
    /// closes the first.
    session: Option<Session>,
}

#[derive(Clone)]
pub struct AppState {
    inner: Arc<RwLock<Inner>>,
    cli: Arc<CliConfig>,
    logs: Option<crate::log_stream::LogBroadcast>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(CliConfig::default())
    }
}

impl AppState {
    #[must_use]
    pub fn new(cli: CliConfig) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                next_id: AtomicU64::new(0),
                session: None,
            })),
            cli: Arc::new(cli),
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

    /// Seed for [`OpenSpec`] in request handlers — starts from the plain
    /// default and folds in CLI overrides.
    #[must_use]
    pub fn default_spec(&self) -> OpenSpec {
        let mut spec = OpenSpec::default();
        if let Some(r) = self.cli.rate_override_hz {
            spec.sample_rate_hz = r;
        }
        if let Some(f) = self.cli.center_override_hz {
            spec.center_freq_hz = f;
        }
        spec
    }

    /// Start a new pipeline task. If a session is already active it is
    /// closed atomically (last-connect-wins, per `docs/02-protocol.md`).
    ///
    /// `override_kind` lets the caller (a request handler) override the
    /// CLI-default source — used so `POST /api/device/open` can target a
    /// specific `SoapySDR` device per session.
    pub async fn open(
        &self,
        spec: OpenSpec,
        override_kind: Option<SourceKind>,
    ) -> Result<OpenedSession> {
        let fft = FftBlock::new(FftBlockParams {
            size: spec.fft_size,
            window: FftWindow::Hann,
        })?;
        let kind = override_kind.unwrap_or_else(|| self.cli.source.clone());
        // Soapy device construction is blocking (USB enumeration, control
        // transfers); offload so the runtime worker stays responsive.
        let source = tokio::task::spawn_blocking(move || build_source(&kind, &spec))
            .await
            .map_err(|e| anyhow!("source build task panicked: {e}"))??;
        // For file sources, honour the file's own sample rate rather than
        // whatever the request carries — the FFT bin spacing must match the
        // samples we're actually consuming. Same story for Soapy devices
        // that round to a supported rate.
        let effective_spec = match &source {
            PipelineSource::Sine(_) => spec,
            PipelineSource::File(f) => OpenSpec {
                sample_rate_hz: f.rate_hz(),
                center_freq_hz: f.center_freq_hz(),
                ..spec
            },
            #[cfg(feature = "soapysdr")]
            PipelineSource::Soapy(d) => OpenSpec {
                sample_rate_hz: d.rate_hz(),
                center_freq_hz: d.center_freq_hz(),
                ..spec
            },
        };
        let mut inner = self.inner.write().await;
        if let Some(prev) = inner.session.take() {
            evict(prev, "evicted");
        }
        let id = format!("{:016x}", inner.next_id.fetch_add(1, Ordering::Relaxed));
        let (frames, _) = broadcast::channel(32);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let frames_for_task = frames.clone();
        let initial_state =
            build_session_state(&id, &source, &effective_spec, kind_for_state(&source));
        let state = Arc::new(Mutex::new(initial_state));
        let state_for_task = state.clone();
        tokio::spawn(run_pipeline(
            effective_spec,
            source,
            fft,
            frames_for_task,
            shutdown_rx,
            control_rx,
            state_for_task,
        ));
        let opened = OpenedSession {
            id: id.clone(),
            fft_size: effective_spec.fft_size,
            fft_rate_hz: effective_spec.fft_rate_hz,
        };
        inner.session = Some(Session {
            id,
            frames,
            shutdown: Some(shutdown_tx),
            control: control_tx,
            state,
        });
        Ok(opened)
    }

    /// Close the session if `id` matches the active one. Sends a
    /// `session_closed` JSON event to subscribers before tearing down the
    /// pipeline so the client can distinguish a deliberate close from
    /// network breakage.
    pub async fn close(&self, id: &str) -> bool {
        let mut inner = self.inner.write().await;
        if inner.session.as_ref().is_some_and(|s| s.id == id) {
            let s = inner.session.take().expect("checked above");
            evict(s, "closed");
            true
        } else {
            false
        }
    }

    /// Snapshot the named session's settings. Returns `None` if no session
    /// or the id doesn't match.
    pub async fn state(&self, id: &str) -> Option<SessionState> {
        let inner = self.inner.read().await;
        let s = inner.session.as_ref()?;
        if s.id != id {
            return None;
        }
        s.state.lock().ok().map(|g| g.clone())
    }

    /// Apply a settings patch to the active session. Returns `None` if no
    /// session matches the id, `Some(SessionState)` with the post-apply
    /// snapshot otherwise. The pipeline task picks up the command on its
    /// next tick — there's no synchronous error path for hardware errors;
    /// those are logged and the snapshot reflects what the device actually
    /// accepted.
    pub async fn patch(&self, id: &str, req: PatchRequest) -> Option<SessionState> {
        if req.is_noop() {
            return self.state(id).await;
        }
        let inner = self.inner.read().await;
        let s = inner.session.as_ref()?;
        if s.id != id {
            return None;
        }
        if s.control.send(ControlCmd::Patch(req)).is_err() {
            tracing::warn!(session = %id, "patch dropped — pipeline closed");
        }
        // Brief drop the lock + yield so the pipeline can apply before we
        // snapshot. This is best-effort; `state()` reads whatever the
        // pipeline has written so far.
        drop(inner);
        tokio::task::yield_now().await;
        self.state(id).await
    }

    /// Get a fresh frame subscription for `id`. Returns `None` if there is
    /// no matching active session.
    pub async fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<FrameBytes>> {
        let inner = self.inner.read().await;
        let session = inner.session.as_ref()?;
        if session.id != id {
            return None;
        }
        Some(session.frames.subscribe())
    }
}

/// Tear down a session, sending a final `session_closed` JSON event to
/// subscribers so the client can react before the WebSocket closes.
fn evict(mut s: Session, reason: &str) {
    let body = format!(
        r#"{{"type":"session_closed","session_id":"{id}","reason":"{reason}"}}"#,
        id = s.id,
    );
    let header = FrameHeader {
        version: crate::ws_frame::PROTOCOL_VERSION,
        payload_type: PayloadType::JsonEvent,
        stream_id: CONTROL_STREAM,
        seq: 0,
        timestamp_ns: now_ns(),
    };
    let mut buf = Vec::with_capacity(crate::ws_frame::HEADER_LEN + body.len());
    encode_into(&header, body.as_bytes(), &mut buf);
    let _ = s.frames.send(Arc::new(buf));
    if let Some(tx) = s.shutdown.take() {
        let _ = tx.send(());
    }
}

fn kind_for_state(source: &PipelineSource) -> &'static str {
    match source {
        PipelineSource::Sine(_) => "sine",
        PipelineSource::File(_) => "file",
        #[cfg(feature = "soapysdr")]
        PipelineSource::Soapy(_) => "soapy",
    }
}

fn build_session_state(
    id: &str,
    source: &PipelineSource,
    spec: &OpenSpec,
    kind: &'static str,
) -> SessionState {
    let (tone, amp, gain, ant, agc) = match source {
        PipelineSource::Sine(_) => (
            Some(spec.tone_freq_abs_hz),
            Some(spec.amplitude),
            None,
            None,
            None,
        ),
        PipelineSource::File(_) => (None, None, None, None, None),
        #[cfg(feature = "soapysdr")]
        PipelineSource::Soapy(_) => (None, None, None, None, None),
    };
    SessionState {
        id: id.to_string(),
        source_kind: kind,
        sample_rate_hz: spec.sample_rate_hz,
        center_freq_hz: spec.center_freq_hz,
        fft_size: spec.fft_size,
        fft_rate_hz: spec.fft_rate_hz,
        floor_dbfs: spec.floor_dbfs,
        ceil_dbfs: spec.ceil_dbfs,
        alpha: spec.alpha,
        tone_freq_abs_hz: tone,
        amplitude: amp,
        gain_db: gain,
        antenna: ant,
        agc,
    }
}

fn now_ns() -> u64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    #[allow(clippy::cast_possible_truncation)]
    {
        dur.as_nanos() as u64
    }
}

/// Runtime union of the supported IQ sources. Construction is fallible
/// (file open, WAV parse, Soapy device open); the pipeline driver only
/// sees [`process`].
pub enum PipelineSource {
    Sine(SineSource),
    File(FileIqSource),
    #[cfg(feature = "soapysdr")]
    Soapy(crate::soapy_source::SoapyIqSource),
}

impl PipelineSource {
    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<()> {
        match self {
            Self::Sine(s) => s.process(io).map(|_| ()).map_err(|e| anyhow!("sine: {e}")),
            Self::File(f) => f.process(io).map(|_| ()).map_err(|e| anyhow!("file: {e}")),
            #[cfg(feature = "soapysdr")]
            Self::Soapy(d) => d.process(io).map_err(|e| anyhow!("soapy: {e}")),
        }
    }

    fn rate_hz(&self) -> f64 {
        match self {
            Self::Sine(_) => f64::NAN,
            Self::File(f) => f.rate_hz(),
            #[cfg(feature = "soapysdr")]
            Self::Soapy(d) => d.rate_hz(),
        }
    }

    fn center_freq_hz(&self) -> f64 {
        match self {
            Self::Sine(_) => f64::NAN,
            Self::File(f) => f.center_freq_hz(),
            #[cfg(feature = "soapysdr")]
            Self::Soapy(d) => d.center_freq_hz(),
        }
    }
}

fn build_source(kind: &SourceKind, spec: &OpenSpec) -> Result<PipelineSource> {
    match kind {
        SourceKind::Sine => Ok(PipelineSource::Sine(SineSource::new(SineSourceParams {
            rate_hz: spec.sample_rate_hz,
            center_freq_hz: spec.center_freq_hz,
            tone_freq_abs_hz: spec.tone_freq_abs_hz,
            amplitude: spec.amplitude,
        }))),
        SourceKind::File {
            path,
            loop_playback,
        } => {
            let f = FileIqSource::new(&FileIqSourceParams {
                path: path.clone(),
                rate_hz_hint: spec.sample_rate_hz,
                center_freq_hz: spec.center_freq_hz,
                loop_playback: *loop_playback,
            })?;
            tracing::info!(
                path = %path.display(),
                format = ?f.format(),
                rate_hz = f.rate_hz(),
                center_hz = f.center_freq_hz(),
                "file source opened"
            );
            Ok(PipelineSource::File(f))
        }
        #[cfg(feature = "soapysdr")]
        SourceKind::Soapy {
            args,
            antenna,
            gain_db,
            agc,
            bandwidth_hz,
        } => {
            let d = crate::soapy_source::SoapyIqSource::new(
                &crate::soapy_source::SoapyIqSourceParams {
                    args: args.clone(),
                    sample_rate_hz: spec.sample_rate_hz,
                    center_freq_hz: spec.center_freq_hz,
                    fft_size: spec.fft_size,
                    channel: 0,
                    bandwidth_hz: *bandwidth_hz,
                    antenna: antenna.clone(),
                    gain_db: *gain_db,
                    agc: *agc,
                },
            )?;
            tracing::info!(
                args = %args,
                rate_hz = d.rate_hz(),
                center_hz = d.center_freq_hz(),
                "soapy source opened"
            );
            Ok(PipelineSource::Soapy(d))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_pipeline(
    spec: OpenSpec,
    mut source: PipelineSource,
    mut fft: FftBlock,
    tx: FrameTx,
    shutdown: oneshot::Receiver<()>,
    mut control: mpsc::UnboundedReceiver<ControlCmd>,
    state: Arc<Mutex<SessionState>>,
) {
    if spec.fft_rate_hz <= 0.0 {
        tracing::error!(rate = spec.fft_rate_hz, "non-positive fft rate");
        return;
    }
    let _ = (source.rate_hz(), source.center_freq_hz()); // silence unused warnings on sine
    let mut log = LogMagU8::new(LogMagU8Params {
        size: spec.fft_size,
        floor_dbfs: spec.floor_dbfs,
        ceil_dbfs: spec.ceil_dbfs,
        alpha: spec.alpha,
    });

    let n = spec.fft_size;
    let mut iq_a = vec![Complex::new(0.0_f32, 0.0); n];
    let mut iq_b = vec![Complex::new(0.0_f32, 0.0); n];
    let mut bins = vec![0_u8; n];
    let mut frame_buf = Vec::with_capacity(crate::ws_frame::HEADER_LEN + n);

    let mut seq: u32 = 0;
    let period = Duration::from_secs_f32(1.0 / spec.fft_rate_hz);
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                tracing::debug!("pipeline shutdown");
                return;
            }
            cmd = control.recv() => {
                match cmd {
                    Some(ControlCmd::Patch(req)) => apply_patch(&req, &mut source, &mut log, &state),
                    None => {
                        tracing::debug!("control channel closed");
                        return;
                    }
                }
            }
            _ = interval.tick() => {
                if let Err(err) = tick(&mut source, &mut fft, &mut log, &mut iq_a, &mut iq_b, &mut bins) {
                    tracing::error!(?err, "pipeline tick failed");
                    return;
                }
                let header = FrameHeader {
                    version: crate::ws_frame::PROTOCOL_VERSION,
                    payload_type: PayloadType::FftU8,
                    stream_id: FFT_STREAM,
                    seq,
                    timestamp_ns: now_ns(),
                };
                frame_buf.clear();
                encode_into(&header, &bins, &mut frame_buf);
                // Send fails only when there are no live receivers; that's
                // expected when nothing has connected yet — drop and keep
                // ticking so a later subscriber gets the next frame.
                let _ = tx.send(Arc::new(frame_buf.clone()));
                seq = seq.wrapping_add(1);
            }
        }
    }
}

fn apply_patch(
    req: &PatchRequest,
    source: &mut PipelineSource,
    log: &mut LogMagU8,
    state: &Arc<Mutex<SessionState>>,
) {
    if let Some(v) = req.floor_dbfs {
        log.set_floor_dbfs(v);
    }
    if let Some(v) = req.ceil_dbfs {
        log.set_ceil_dbfs(v);
    }
    if let Some(v) = req.alpha {
        log.set_alpha(v);
    }
    match source {
        PipelineSource::Sine(s) => {
            if let Some(v) = req.center_freq_hz {
                s.set_center_freq(v);
            }
            if let Some(v) = req.tone_freq_abs_hz {
                s.set_tone_freq_abs(v);
            }
            if let Some(v) = req.amplitude {
                s.set_amplitude(v);
            }
        }
        PipelineSource::File(_) => { /* file replay is immutable */ }
        #[cfg(feature = "soapysdr")]
        PipelineSource::Soapy(d) => {
            if let Some(v) = req.center_freq_hz {
                if let Err(err) = d.set_center_freq(v) {
                    tracing::warn!(?err, "soapy set_center_freq failed");
                }
            }
            if let Some(v) = req.gain_db {
                if let Err(err) = d.set_gain(v) {
                    tracing::warn!(?err, "soapy set_gain failed");
                }
            }
            if let Some(v) = req.agc {
                if let Err(err) = d.set_agc(v) {
                    tracing::warn!(?err, "soapy set_agc failed");
                }
            }
        }
    }
    if let Ok(mut s) = state.lock() {
        if let Some(v) = req.center_freq_hz {
            s.center_freq_hz = match source {
                #[cfg(feature = "soapysdr")]
                PipelineSource::Soapy(d) => d.center_freq_hz(),
                _ => v,
            };
        }
        if let Some(v) = req.tone_freq_abs_hz {
            s.tone_freq_abs_hz = Some(v);
        }
        if let Some(v) = req.amplitude {
            s.amplitude = Some(v);
        }
        if let Some(v) = req.gain_db {
            s.gain_db = Some(v);
        }
        if let Some(v) = req.agc {
            s.agc = Some(v);
        }
        if let Some(v) = req.floor_dbfs {
            s.floor_dbfs = v;
        }
        if let Some(v) = req.ceil_dbfs {
            s.ceil_dbfs = v;
        }
        if let Some(v) = req.alpha {
            s.alpha = v;
        }
    }
}

fn tick(
    source: &mut PipelineSource,
    fft: &mut FftBlock,
    log: &mut LogMagU8,
    iq_a: &mut [Complex<f32>],
    iq_b: &mut [Complex<f32>],
    bins: &mut [u8],
) -> Result<()> {
    {
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(iq_a),
        }];
        let mut io = BlockIo {
            inputs: &mut [],
            outputs: &mut outputs,
        };
        source.process(&mut io)?;
    }
    {
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(iq_a),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(iq_b),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        fft.process(&mut io).map_err(|e| anyhow!("fft: {e}"))?;
    }
    {
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(iq_b),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::FftU8(bins),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        log.process(&mut io).map_err(|e| anyhow!("log: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{AppState, CliConfig, OpenSpec, SourceKind};
    use crate::ws_frame::{decode, PayloadType, FFT_STREAM};
    use std::time::Duration;

    #[tokio::test]
    async fn open_then_subscribe_receives_frame() {
        let state = AppState::new(CliConfig::default());
        let spec = OpenSpec {
            fft_size: 256,
            fft_rate_hz: 200.0,
            ..OpenSpec::default()
        };
        let opened = state.open(spec, None).await.unwrap();
        let mut rx = state.subscribe(&opened.id).await.expect("subscribe");
        let bytes = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("frame within 1s")
            .expect("broadcast ok");
        let (header, payload) = decode(&bytes).unwrap();
        assert_eq!(header.payload_type, PayloadType::FftU8);
        assert_eq!(header.stream_id, FFT_STREAM);
        assert_eq!(payload.len(), 256);
        state.close(&opened.id).await;
    }

    #[tokio::test]
    async fn second_open_evicts_first() {
        let state = AppState::new(CliConfig::default());
        let a = state.open(OpenSpec::default(), None).await.unwrap();
        let b = state.open(OpenSpec::default(), None).await.unwrap();
        assert_ne!(a.id, b.id);
        assert!(state.subscribe(&a.id).await.is_none());
        assert!(state.subscribe(&b.id).await.is_some());
        state.close(&b.id).await;
    }

    #[tokio::test]
    async fn close_returns_false_for_unknown() {
        let state = AppState::new(CliConfig::default());
        assert!(!state.close("nope").await);
    }

    #[test]
    fn default_spec_applies_cli_overrides() {
        let cli = CliConfig {
            source: SourceKind::Sine,
            rate_override_hz: Some(3.2e6),
            center_override_hz: Some(433.0e6),
        };
        let state = AppState::new(cli);
        let spec = state.default_spec();
        assert!((spec.sample_rate_hz - 3.2e6).abs() < 1.0);
        assert!((spec.center_freq_hz - 433.0e6).abs() < 1.0);
    }
}
