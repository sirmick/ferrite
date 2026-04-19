//! Live `SoapySDR` IQ source — reads `Complex<f32>` samples off a real
//! device (RTL-SDR, RSP1A, …) and feeds them into the pipeline.
//!
//! ## Architecture
//!
//! The pipeline ticks at `fft_rate_hz` and consumes exactly `fft_size`
//! samples per tick. Hardware produces samples at the much higher
//! `sample_rate_hz`. The naive "read N then return" model overruns the
//! driver's internal buffer almost immediately (we'd consume 4096
//! samples every 33 ms while the device produces them every 2 ms at
//! 2 MS/s).
//!
//! The stopgap used here: a dedicated **OS thread** calls `read()` in a
//! tight loop, always overwriting a single `Mutex<Option<Vec<…>>>` slot
//! with the most recent `fft_size` samples. The pipeline's `process()`
//! swaps that slot on each tick. Dropped samples are fine for the
//! waterfall display — we only need the **latest** spectrum, not every
//! sample. Phase D will replace this with a proper ring-buffer + VFO
//! channelizer that keeps every sample.
//!
//! ## Lifetime
//!
//! `Drop` flips a stop flag and joins the reader thread, then drops the
//! `RxStream` (which deactivates and closes itself). `Device` is held
//! by the stream so the handle stays valid for the read loop.

#![cfg(feature = "soapysdr")]

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use ferrite_blocks::{BlockIo, OutputPort};
use num_complex::Complex;
use soapysdr::{Device, Direction, ErrorCode, RxStream};

#[derive(Debug, Clone)]
pub struct SoapyIqSourceParams {
    pub args: String,
    pub sample_rate_hz: f64,
    pub center_freq_hz: f64,
    pub fft_size: usize,
    pub channel: usize,
    pub bandwidth_hz: Option<f64>,
    pub antenna: Option<String>,
    pub gain_db: Option<f64>,
    pub agc: Option<bool>,
}

type LatestSlot = Arc<Mutex<Option<Vec<Complex<f32>>>>>;

pub struct SoapyIqSource {
    /// Held so we can issue retunes / gain changes from any thread while
    /// the reader thread holds the `RxStream`. Soapy's `Device` is an
    /// `Arc<DeviceInner>` internally, so cloning it is cheap and the
    /// underlying handle is reference-counted.
    device: Device,
    channel: usize,
    sample_rate_hz: f64,
    center_freq_hz: f64,
    latest: LatestSlot,
    /// Bumped by the reader after every successful write to `latest`.
    /// The pipeline compares it against `last_seen_version` to tell a
    /// fresh block from a re-served stale one.
    version: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    /// Gap instrumentation. `gaps` = pipeline ticks where the reader
    /// hadn't produced any block yet (zero-fill, only at startup).
    /// `stale` = ticks where we re-served the previous block because
    /// the reader was mid-stall; those show as duplicate waterfall
    /// rows rather than black rows. Both are surfaced as rate-limited
    /// warns so the UI log panel shows drop-outs.
    ticks: u64,
    gaps: u64,
    stale: u64,
    last_seen_version: u64,
    last_warn_log: Option<Instant>,
}

impl SoapyIqSource {
    /// Open the device, configure it, activate the Rx stream, and spawn
    /// the reader thread. All Soapy calls are blocking; the caller should
    /// invoke this from `tokio::task::spawn_blocking`.
    pub fn new(params: &SoapyIqSourceParams) -> Result<Self> {
        let device = Device::new(params.args.as_str())
            .with_context(|| format!("open SoapySDR device with args {:?}", params.args))?;
        let dir = Direction::Rx;
        let ch = params.channel;

        device
            .set_sample_rate(dir, ch, params.sample_rate_hz)
            .with_context(|| format!("set sample_rate={}", params.sample_rate_hz))?;
        device
            .set_frequency(dir, ch, params.center_freq_hz, ())
            .with_context(|| format!("set center_freq={}", params.center_freq_hz))?;
        if let Some(bw) = params.bandwidth_hz {
            device
                .set_bandwidth(dir, ch, bw)
                .with_context(|| format!("set bandwidth={bw}"))?;
        }
        if let Some(ant) = &params.antenna {
            device
                .set_antenna(dir, ch, ant.as_bytes())
                .with_context(|| format!("set antenna={ant}"))?;
        }
        if let Some(agc) = params.agc {
            // `set_gain_mode` may not exist on every driver; ignore the
            // error and continue with manual gain.
            let _ = device.set_gain_mode(dir, ch, agc);
        }
        if let Some(g) = params.gain_db {
            device
                .set_gain(dir, ch, g)
                .with_context(|| format!("set gain={g}"))?;
        }

        let actual_rate = device.sample_rate(dir, ch).unwrap_or(params.sample_rate_hz);
        let actual_freq = device.frequency(dir, ch).unwrap_or(params.center_freq_hz);

        let mut stream: RxStream<Complex<f32>> = device
            .rx_stream::<Complex<f32>>(&[ch])
            .context("create Rx stream")?;
        stream.activate(None).context("activate Rx stream")?;

        if params.fft_size == 0 {
            return Err(anyhow!("fft_size must be > 0"));
        }

        let latest: LatestSlot = Arc::new(Mutex::new(None));
        let version = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let reader_latest = latest.clone();
        let reader_version = version.clone();
        let reader_stop = stop.clone();
        let block_size = params.fft_size;
        let reader = thread::Builder::new()
            .name("soapy-rx".into())
            .spawn(move || {
                run_reader(
                    stream,
                    block_size,
                    &reader_latest,
                    &reader_version,
                    &reader_stop,
                );
            })
            .context("spawn soapy reader thread")?;

        Ok(Self {
            device,
            channel: ch,
            sample_rate_hz: actual_rate,
            center_freq_hz: actual_freq,
            latest,
            version,
            stop,
            reader: Some(reader),
            ticks: 0,
            gaps: 0,
            stale: 0,
            last_seen_version: 0,
            last_warn_log: None,
        })
    }

    #[must_use]
    pub fn rate_hz(&self) -> f64 {
        self.sample_rate_hz
    }

    #[must_use]
    pub fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    /// Retune the device to a new RF center frequency. Re-reads the
    /// hardware's actual setting (some drivers snap to a step).
    pub fn set_center_freq(&mut self, hz: f64) -> Result<()> {
        self.device
            .set_frequency(Direction::Rx, self.channel, hz, ())
            .with_context(|| format!("set center_freq={hz}"))?;
        if let Ok(actual) = self.device.frequency(Direction::Rx, self.channel) {
            self.center_freq_hz = actual;
        }
        Ok(())
    }

    pub fn set_gain(&mut self, db: f64) -> Result<()> {
        self.device
            .set_gain(Direction::Rx, self.channel, db)
            .with_context(|| format!("set gain={db}"))?;
        Ok(())
    }

    pub fn set_agc(&mut self, on: bool) -> Result<()> {
        self.device
            .set_gain_mode(Direction::Rx, self.channel, on)
            .with_context(|| format!("set agc={on}"))?;
        Ok(())
    }

    /// Copy the most recent block of samples into the `out` port. If the
    /// reader has not produced its first block yet, fills with zeros so
    /// the rest of the pipeline still runs (the next FFT just shows a
    /// quiet noise floor for one tick).
    pub fn process(&mut self, io: &mut BlockIo<'_>) -> Result<()> {
        let Some(out) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_iq_f32_mut)
        else {
            return Ok(());
        };
        // Read the version first so that a concurrent reader write
        // between the load and the lock counts as "fresh" (we'll copy
        // the new data and the higher version on the next tick).
        let version = self.version.load(Ordering::Acquire);
        let guard = self
            .latest
            .lock()
            .map_err(|_| anyhow!("soapy latest mutex poisoned"))?;
        self.ticks = self.ticks.saturating_add(1);
        let (mut gap, mut stale) = (false, false);
        if let Some(samples) = guard.as_ref() {
            let n = out.len().min(samples.len());
            out[..n].copy_from_slice(&samples[..n]);
            if n < out.len() {
                out[n..].fill(Complex::new(0.0, 0.0));
            }
            if version == self.last_seen_version {
                self.stale = self.stale.saturating_add(1);
                stale = true;
            } else {
                self.last_seen_version = version;
            }
        } else {
            out.fill(Complex::new(0.0, 0.0));
            self.gaps = self.gaps.saturating_add(1);
            gap = true;
        }
        drop(guard);
        if gap || stale {
            let now = Instant::now();
            let due = self
                .last_warn_log
                .is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1));
            if due {
                tracing::warn!(
                    gaps = self.gaps,
                    stale = self.stale,
                    ticks = self.ticks,
                    "soapy reader stall — pipeline served stale or zero-fill block"
                );
                self.last_warn_log = Some(now);
            }
        }
        Ok(())
    }
}

impl Drop for SoapyIqSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.reader.take() {
            // Best-effort join — we hold no async runtime guarantees here.
            let _ = handle.join();
        }
    }
}

fn run_reader(
    mut stream: RxStream<Complex<f32>>,
    block_size: usize,
    latest: &LatestSlot,
    version: &Arc<AtomicU64>,
    stop: &Arc<AtomicBool>,
) {
    let mut buf = vec![Complex::new(0.0_f32, 0.0); block_size];
    let read_timeout_us: i64 = 100_000;
    while !stop.load(Ordering::Relaxed) {
        let mut filled = 0;
        while filled < block_size && !stop.load(Ordering::Relaxed) {
            let result = {
                let dst: &mut [Complex<f32>] = &mut buf[filled..];
                let mut buffers: [&mut [Complex<f32>]; 1] = [dst];
                stream.read(&mut buffers, read_timeout_us)
            };
            match result {
                Ok(0) => {
                    thread::sleep(Duration::from_millis(2));
                }
                Ok(n) => filled += n,
                Err(err) if err.code == ErrorCode::Timeout => {
                    // Normal during stream startup before samples flow,
                    // and under bursty drivers. Retry with brief backoff.
                    thread::sleep(Duration::from_millis(2));
                }
                Err(err) if err.code == ErrorCode::Overflow => {
                    // Driver dropped samples; we only care about the
                    // latest block anyway, so keep reading.
                    tracing::debug!("soapy overflow");
                }
                Err(err) => {
                    tracing::warn!(?err, "soapy read error; ending reader");
                    let _ = stream.deactivate(None);
                    return;
                }
            }
        }
        if filled == block_size {
            if let Ok(mut slot) = latest.lock() {
                // Ping-pong the scratch buffer with whatever the slot
                // held so we avoid a 32 KB allocation every ~2 ms.
                let prev = slot.replace(std::mem::take(&mut buf));
                buf = prev.unwrap_or_else(|| vec![Complex::new(0.0, 0.0); block_size]);
                if buf.len() != block_size {
                    buf.resize(block_size, Complex::new(0.0, 0.0));
                }
            }
            version.fetch_add(1, Ordering::Release);
        }
    }
    let _ = stream.deactivate(None);
}
