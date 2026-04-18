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
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use ferrite_blocks::{
    Block, BlockIo, FftBlock, FftBlockParams, FftWindow, InBuf, InputPort, LogMagU8,
    LogMagU8Params, OutBuf, OutputPort, PortMeta, SineSource, SineSourceParams,
};
use num_complex::Complex;
use tokio::sync::{broadcast, oneshot, RwLock};

use crate::ws_frame::{encode_into, FrameHeader, PayloadType, FFT_STREAM};

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
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                next_id: AtomicU64::new(0),
                session: None,
            })),
        }
    }

    /// Start a new pipeline task. If a session is already active it is
    /// closed atomically (last-connect-wins, per `docs/02-protocol.md`).
    pub async fn open(&self, spec: OpenSpec) -> Result<OpenedSession> {
        let fft = FftBlock::new(FftBlockParams {
            size: spec.fft_size,
            window: FftWindow::Hann,
        })?;
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
        tokio::spawn(run_pipeline(spec, fft, frames_for_task, shutdown_rx));
        let opened = OpenedSession {
            id: id.clone(),
            fft_size: spec.fft_size,
            fft_rate_hz: spec.fft_rate_hz,
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

async fn run_pipeline(
    spec: OpenSpec,
    mut fft: FftBlock,
    tx: FrameTx,
    shutdown: oneshot::Receiver<()>,
) {
    if spec.fft_rate_hz <= 0.0 {
        tracing::error!(rate = spec.fft_rate_hz, "non-positive fft rate");
        return;
    }
    let mut sine = SineSource::new(SineSourceParams {
        rate_hz: spec.sample_rate_hz,
        center_freq_hz: spec.center_freq_hz,
        tone_freq_abs_hz: spec.tone_freq_abs_hz,
        amplitude: spec.amplitude,
    });
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
                if let Err(err) = tick(&mut sine, &mut fft, &mut log, &mut iq_a, &mut iq_b, &mut bins) {
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
    sine: &mut SineSource,
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
        sine.process(&mut io).map_err(|e| anyhow!("sine: {e}"))?;
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
    use super::{AppState, OpenSpec};
    use crate::ws_frame::{decode, PayloadType, FFT_STREAM};
    use std::time::Duration;

    #[tokio::test]
    async fn open_then_subscribe_receives_frame() {
        let state = AppState::new();
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
        let state = AppState::new();
        let a = state.open(OpenSpec::default()).await.unwrap();
        let b = state.open(OpenSpec::default()).await.unwrap();
        assert_ne!(a.id, b.id);
        assert!(state.subscribe(&a.id).await.is_none());
        assert!(state.subscribe(&b.id).await.is_some());
        state.close(&b.id).await;
    }

    #[tokio::test]
    async fn close_returns_false_for_unknown() {
        let state = AppState::new();
        assert!(!state.close("nope").await);
    }
}
