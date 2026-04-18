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
        Arc,
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
use tokio::sync::{broadcast, oneshot, RwLock};

use crate::ws_frame::{encode_into, FrameHeader, PayloadType, FFT_STREAM};

/// Server-wide source selection. Set once at startup via CLI; every session
/// opened afterwards uses this kind. (A future commit will let the UI pick
/// between registered sources per session.)
#[derive(Debug, Clone, Default)]
pub enum SourceKind {
    #[default]
    Sine,
    File {
        path: PathBuf,
        loop_playback: bool,
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

struct Session {
    id: String,
    frames: FrameTx,
    /// Taken-and-fired by `close()`; the pipeline task watches this.
    shutdown: Option<oneshot::Sender<()>>,
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
        }
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
    pub async fn open(&self, spec: OpenSpec) -> Result<OpenedSession> {
        let fft = FftBlock::new(FftBlockParams {
            size: spec.fft_size,
            window: FftWindow::Hann,
        })?;
        let source = build_source(&self.cli.source, &spec)?;
        // For file sources, honour the file's own sample rate rather than
        // whatever the request carries — the FFT bin spacing must match the
        // samples we're actually consuming.
        let effective_spec = match &source {
            PipelineSource::Sine(_) => spec,
            PipelineSource::File(f) => OpenSpec {
                sample_rate_hz: f.rate_hz(),
                center_freq_hz: f.center_freq_hz(),
                ..spec
            },
        };
        let mut inner = self.inner.write().await;
        if let Some(mut prev) = inner.session.take() {
            if let Some(tx) = prev.shutdown.take() {
                let _ = tx.send(());
            }
        }
        let id = format!("{:016x}", inner.next_id.fetch_add(1, Ordering::Relaxed));
        let (frames, _) = broadcast::channel(32);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let frames_for_task = frames.clone();
        tokio::spawn(run_pipeline(
            effective_spec,
            source,
            fft,
            frames_for_task,
            shutdown_rx,
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
        });
        Ok(opened)
    }

    /// Close the session if `id` matches the active one.
    pub async fn close(&self, id: &str) -> bool {
        let mut inner = self.inner.write().await;
        if inner.session.as_ref().is_some_and(|s| s.id == id) {
            let mut s = inner.session.take().expect("checked above");
            if let Some(tx) = s.shutdown.take() {
                let _ = tx.send(());
            }
            true
        } else {
            false
        }
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
/// (file open, WAV parse); the pipeline driver only sees [`process`].
pub enum PipelineSource {
    Sine(SineSource),
    File(FileIqSource),
}

impl PipelineSource {
    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<()> {
        match self {
            Self::Sine(s) => s.process(io).map(|_| ()).map_err(|e| anyhow!("sine: {e}")),
            Self::File(f) => f.process(io).map(|_| ()).map_err(|e| anyhow!("file: {e}")),
        }
    }

    fn rate_hz(&self) -> f64 {
        match self {
            Self::Sine(_) => f64::NAN,
            Self::File(f) => f.rate_hz(),
        }
    }

    fn center_freq_hz(&self) -> f64 {
        match self {
            Self::Sine(_) => f64::NAN,
            Self::File(f) => f.center_freq_hz(),
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
    }
}

async fn run_pipeline(
    spec: OpenSpec,
    mut source: PipelineSource,
    mut fft: FftBlock,
    tx: FrameTx,
    shutdown: oneshot::Receiver<()>,
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
        let opened = state.open(spec).await.unwrap();
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
        let a = state.open(OpenSpec::default()).await.unwrap();
        let b = state.open(OpenSpec::default()).await.unwrap();
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
