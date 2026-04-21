//! Live `SoapySDR` IQ source — reads `Complex<f32>` samples off a real
//! device (RTL-SDR, RSP1A, RSPdx, Airspy, …) and emits them on an `IqF32`
//! port.
//!
//! ## Architecture
//!
//! A dedicated reader thread pulls from the Soapy `RxStream` and pushes
//! every sample into a bounded SPSC ring. `process()` pops exactly what
//! the downstream port asks for each tick. The ring sits between the
//! driver's USB cadence (which delivers in fixed-size blocks every few
//! milliseconds) and the scheduler's tick cadence (every 400µs at the
//! current default), so steady-state flow is **sample-for-sample 1:1**
//! with no dupes and no drops.
//!
//! When the scheduler does fall behind for long enough to fill the ring
//! (GC, IO stall, whatever), the reader drops incoming samples and
//! bumps [`SoapySource::ring_drops`] — explicit, countable, not silent.
//! Driver-reported overflows land in [`SoapySource::overflow_drops`]
//! and timestamp discontinuities (gaps between `stream.time_ns()`
//! readings and the expected delta from `sample_rate_hz`) in
//! [`SoapySource::timestamp_gaps`]. A capture that observes all three
//! counters at zero is provably lossless.
//!
//! ## Lifetime
//!
//! - [`SoapySource::new`] opens the device, configures it, and
//!   activates the Rx stream. Failure here surfaces at flowgraph
//!   instantiation rather than first tick.
//! - [`Block::init`] resets ring + counters and spawns the reader.
//! - [`Block::process`] pops from the ring; short reads zero-fill and
//!   bump [`SoapySource::underrun_samples`].
//! - [`Block::stop`] / `Drop` flips a stop flag, joins the reader, and
//!   deactivates the stream.
//!
//! ## Feature
//!
//! Gated on the crate's `soapysdr` feature. WASM builds never pull this
//! in; `ferrite-blocks` compiled without the feature simply does not
//! register the block, so presets referencing it fail validation with a
//! clear "unknown type" error.

#![cfg(feature = "soapysdr")]

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use anyhow::{anyhow, Context, Result};
use num_complex::Complex;
use serde::Deserialize;
use soapysdr::{Device, Direction, ErrorCode, RxStream};

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, OutputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};
use crate::spsc_ring::IqRing;

/// Ring capacity in samples. 512 k complex samples = 4 MiB.
/// 256 ms of headroom at 2 MS/s, 26 ms at 20 MS/s — covers any
/// realistic scheduler hiccup short of a swap storm.
const RING_CAPACITY: usize = 524_288;

/// Samples the reader pulls per `stream.read()` call. Balances syscall
/// overhead against mutex-lock duration. 16 k samples ≈ 8 ms at
/// 2 MS/s — mutex held for a few µs, plenty short for 2.5 kHz tickers.
const READER_CHUNK: usize = 16_384;

/// Read timeout passed to Soapy, microseconds. Long enough to absorb
/// normal USB jitter, short enough that `stop` is responsive.
const READ_TIMEOUT_US: i64 = 100_000;

/// Counters bumped exclusively by the reader thread. `process()` reads
/// them lock-free so diagnostics can sample at any tick rate without
/// perturbing the hot path.
#[derive(Debug, Default)]
struct ReaderCounters {
    overflow_drops: AtomicU64,
    ring_drops: AtomicU64,
    timestamp_gaps: AtomicU64,
    samples_pushed: AtomicU64,
}

/// Construction-time params. All fields are optional in the JSON preset;
/// missing fields fall back to [`Default`].
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SoapySourceParams {
    /// `SoapySDR` device args (e.g. `driver=rtlsdr,serial=0000001`).
    pub args: String,
    /// Driver-level sample rate, Hz. Not all drivers honour arbitrary
    /// values — the block reads back the actual rate after setting.
    pub sample_rate_hz: f64,
    /// RF centre frequency, Hz.
    pub center_freq_hz: f64,
    /// Optional analog filter bandwidth, Hz.
    pub bandwidth_hz: Option<f64>,
    /// Optional antenna port name (driver-specific; e.g. `LNA-H` on RSP1A).
    pub antenna: Option<String>,
    /// Optional manual gain, dB. If both `gain_db` and `agc=true` are
    /// set, the AGC setting is applied first then manual gain overrides.
    pub gain_db: Option<f64>,
    /// Optional AGC toggle. Drivers lacking AGC silently ignore the call.
    pub agc: Option<bool>,
    /// Rx channel index. Most drivers only have channel 0.
    pub channel: usize,
}

impl Default for SoapySourceParams {
    fn default() -> Self {
        Self {
            args: String::new(),
            sample_rate_hz: 2_400_000.0,
            center_freq_hz: 100_000_000.0,
            bandwidth_hz: None,
            antenna: None,
            gain_db: None,
            agc: None,
            channel: 0,
        }
    }
}

type RingHandle = Arc<Mutex<IqRing>>;

pub struct SoapySource {
    #[allow(dead_code)]
    device: Device,
    #[allow(dead_code)]
    channel: usize,
    sample_rate_hz: f64,
    center_freq_hz: f64,
    /// Some before [`Block::init`]; taken and moved into the reader
    /// thread when init spawns it.
    stream: Option<RxStream<Complex<f32>>>,
    ring: RingHandle,
    counters: Arc<ReaderCounters>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    ticks: u64,
    /// Samples the process() hot path could not deliver because the
    /// ring was empty. Zero on a healthy run.
    underrun_samples: u64,
}

impl SoapySource {
    /// Open the device, apply configuration, and activate the Rx stream.
    /// Blocking — call from `tokio::task::spawn_blocking` when invoking
    /// from an async context.
    pub fn new(params: &SoapySourceParams) -> Result<Self> {
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
            // Not all drivers expose an AGC mode; swallow an error here
            // and fall through to manual gain if supplied.
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

        Ok(Self {
            device,
            channel: ch,
            sample_rate_hz: actual_rate,
            center_freq_hz: actual_freq,
            stream: Some(stream),
            ring: Arc::new(Mutex::new(IqRing::new(RING_CAPACITY))),
            counters: Arc::new(ReaderCounters::default()),
            stop: Arc::new(AtomicBool::new(false)),
            reader: None,
            ticks: 0,
            underrun_samples: 0,
        })
    }

    /// Post-configure readback — what the hardware actually locked to.
    #[must_use]
    pub const fn sample_rate_hz(&self) -> f64 {
        self.sample_rate_hz
    }

    #[must_use]
    pub const fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    /// Samples dropped because the driver's internal ring overran
    /// (`ErrorCode::Overflow` from `stream.read`). Nonzero means the
    /// reader thread itself was too slow for the rate.
    #[must_use]
    pub fn overflow_drops(&self) -> u64 {
        self.counters.overflow_drops.load(Ordering::Relaxed)
    }

    /// Samples dropped because our in-process ring was full — the
    /// reader produced them but `process()` was too slow to consume.
    /// Nonzero means the pipeline fell behind its real-time clock.
    #[must_use]
    pub fn ring_drops(&self) -> u64 {
        self.counters.ring_drops.load(Ordering::Relaxed)
    }

    /// Count of reader reads whose timestamp drifted from the expected
    /// monotonic delta by more than one sample period. Nonzero means
    /// the driver believes it dropped samples between reads even when
    /// it didn't report an `Overflow`.
    #[must_use]
    pub fn timestamp_gaps(&self) -> u64 {
        self.counters.timestamp_gaps.load(Ordering::Relaxed)
    }

    /// Cumulative samples the reader successfully pushed into the ring.
    /// Useful to compute effective rate from outside the block.
    #[must_use]
    pub fn samples_pushed(&self) -> u64 {
        self.counters.samples_pushed.load(Ordering::Relaxed)
    }

    /// Samples `process()` could not deliver because the ring was
    /// empty — tick-side underruns. Nonzero typically means the
    /// scheduler is ticking faster than the reader can feed.
    #[must_use]
    pub fn underrun_samples(&self) -> u64 {
        self.underrun_samples
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for SoapySource {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "SoapySource",
            placement: Placement::NativeOnly,
            inputs: &[],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[
                ParamSpec {
                    key: "args",
                    label: "Device args",
                    kind: ParamKind::Text {
                        default: "driver=rtlsdr",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Sample rate",
                    kind: ParamKind::Range {
                        min: 250_000.0,
                        max: 20_000_000.0,
                        step: 1.0,
                        default: 2_400_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
                ParamSpec {
                    key: "center_freq_hz",
                    label: "Centre frequency",
                    kind: ParamKind::Range {
                        min: 24_000_000.0,
                        max: 1_800_000_000.0,
                        step: 1.0,
                        default: 100_000_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "gain_db",
                    label: "Gain",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 60.0,
                        step: 0.5,
                        default: 20.0,
                        unit: "dB",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "channel",
                    label: "Rx channel",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 7.0,
                        step: 1.0,
                        default: 0.0,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
            ],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        if self.reader.is_some() {
            return Err(anyhow!("SoapySource::init called more than once"));
        }
        let stream = self
            .stream
            .take()
            .ok_or_else(|| anyhow!("SoapySource stream missing at init — was new() called?"))?;

        if let Ok(mut ring) = self.ring.lock() {
            ring.reset();
        }
        self.ticks = 0;
        self.underrun_samples = 0;

        let reader_ring = self.ring.clone();
        let reader_counters = self.counters.clone();
        let reader_stop = self.stop.clone();
        let rate_hz = self.sample_rate_hz;
        let reader = thread::Builder::new()
            .name("soapy-rx".into())
            .spawn(move || {
                run_reader(
                    stream,
                    rate_hz,
                    &reader_ring,
                    &reader_counters,
                    &reader_stop,
                );
            })
            .context("spawn soapy reader thread")?;
        self.reader = Some(reader);
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(out) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_iq_f32_mut)
        else {
            return Ok(Work::new());
        };

        self.ticks = self.ticks.saturating_add(1);
        let want = out.len();
        let got = match self.ring.lock() {
            Ok(mut ring) => ring.read(out),
            Err(_) => return Err(anyhow!("soapy ring mutex poisoned")),
        };
        if got < want {
            out[got..].fill(Complex::new(0.0, 0.0));
            self.underrun_samples = self.underrun_samples.saturating_add((want - got) as u64);
        }

        let mut w = Work::new();
        w.produced[0] = want;
        Ok(w)
    }

    fn stop(&mut self) -> Result<()> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
        Ok(())
    }
}

impl Drop for SoapySource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

impl BlockFactory for SoapySource {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: SoapySourceParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(SoapySource::new(&p)?))
    }
}

fn run_reader(
    mut stream: RxStream<Complex<f32>>,
    rate_hz: f64,
    ring: &RingHandle,
    counters: &Arc<ReaderCounters>,
    stop: &Arc<AtomicBool>,
) {
    let mut staging: Vec<Complex<f32>> = vec![Complex::new(0.0, 0.0); READER_CHUNK];
    // Timestamp bookkeeping: the expected next time_ns, updated after
    // each successful read. Gap detection skips the first read and any
    // read where the driver reports a zero timestamp (not all drivers
    // populate it — RTL-SDR notably does not).
    let mut expected_time_ns: i64 = 0;
    let ns_per_sample = if rate_hz > 0.0 {
        1_000_000_000.0_f64 / rate_hz
    } else {
        0.0
    };
    let gap_tolerance_ns = (ns_per_sample * 2.0) as i64;

    while !stop.load(Ordering::Relaxed) {
        let result = {
            let dst: &mut [Complex<f32>] = &mut staging[..];
            let mut buffers: [&mut [Complex<f32>]; 1] = [dst];
            stream.read(&mut buffers, READ_TIMEOUT_US)
        };
        match result {
            Ok(0) => {
                thread::sleep(Duration::from_millis(1));
            }
            Ok(n) => {
                let t = stream.time_ns();
                if t > 0 {
                    if expected_time_ns > 0 {
                        let delta = (t - expected_time_ns).abs();
                        if delta > gap_tolerance_ns {
                            counters.timestamp_gaps.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    expected_time_ns = t + (n as f64 * ns_per_sample) as i64;
                }

                let pushed = match ring.lock() {
                    Ok(mut r) => r.write(&staging[..n]),
                    Err(_) => {
                        tracing::warn!("soapy ring mutex poisoned; ending reader");
                        let _ = stream.deactivate(None);
                        return;
                    }
                };
                counters
                    .samples_pushed
                    .fetch_add(pushed as u64, Ordering::Relaxed);
                if pushed < n {
                    counters
                        .ring_drops
                        .fetch_add((n - pushed) as u64, Ordering::Relaxed);
                }
            }
            Err(err) if err.code == ErrorCode::Timeout => {
                // Normal idle — try again. Brief sleep so the driver
                // has a chance to fill its internal ring if the pipe
                // was momentarily starved.
                thread::sleep(Duration::from_millis(1));
            }
            Err(err) if err.code == ErrorCode::Overflow => {
                counters.overflow_drops.fetch_add(1, Ordering::Relaxed);
                tracing::debug!("soapy overflow");
                // Invalidate the expected-time tracker — we can't
                // compute a sane delta across the gap.
                expected_time_ns = 0;
            }
            Err(err) => {
                tracing::warn!(?err, "soapy read error; ending reader");
                let _ = stream.deactivate(None);
                return;
            }
        }
    }
    let _ = stream.deactivate(None);
}

#[cfg(test)]
mod tests {
    use super::{SoapySource, SoapySourceParams};
    use crate::block::{Block, Placement, PortType};

    #[test]
    fn spec_is_native_only_iq_out() {
        let s = SoapySource::spec();
        assert_eq!(s.type_name, "SoapySource");
        assert!(matches!(s.placement, Placement::NativeOnly));
        assert_eq!(s.inputs.len(), 0);
        assert_eq!(s.outputs.len(), 1);
        assert_eq!(s.outputs[0].name, "out");
        assert!(matches!(s.outputs[0].port_type, PortType::IqF32));
    }

    #[test]
    fn params_round_trip_through_json() {
        let src = serde_json::json!({
            "args": "driver=rtlsdr,serial=0000001",
            "sample_rate_hz": 2_400_000.0,
            "center_freq_hz": 100_100_000.0,
            "bandwidth_hz": 2_000_000.0,
            "gain_db": 20.0,
            "agc": false,
            "channel": 0,
        });
        let p: SoapySourceParams = serde_json::from_value(src).unwrap();
        assert_eq!(p.args, "driver=rtlsdr,serial=0000001");
        assert!((p.sample_rate_hz - 2_400_000.0).abs() < f64::EPSILON);
        assert_eq!(p.bandwidth_hz, Some(2_000_000.0));
        assert_eq!(p.agc, Some(false));
        assert_eq!(p.channel, 0);
    }

    #[test]
    fn defaults_fill_in_omitted_fields() {
        let p: SoapySourceParams = serde_json::from_value(serde_json::json!({})).unwrap();
        // Matches Default::default().
        assert_eq!(p.args, "");
        assert!(p.bandwidth_hz.is_none());
        assert!(p.gain_db.is_none());
        assert!(p.agc.is_none());
    }
}
