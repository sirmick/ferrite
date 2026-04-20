//! Live `SoapySDR` IQ source — reads `Complex<f32>` samples off a real
//! device (RTL-SDR, RSP1A, Airspy, …) and emits them on an `IqF32` port.
//!
//! ## Architecture
//!
//! Hardware produces samples at whatever the driver delivers; the
//! pipeline consumes `frames_hint` samples per [`process`] call. The
//! two cadences rarely line up. To keep this block allocation-free on
//! the hot path while absorbing driver bursts, a dedicated OS thread
//! reads into a single `Mutex<Option<Vec<…>>>` slot, overwriting it
//! with the most recent `frames_hint`-sized block. `process()` copies
//! that slot into the output buffer on each tick — dropping older
//! blocks the reader already replaced.
//!
//! This is the lossy-latest policy from the server's original
//! `soapy_source.rs`: fine for a waterfall or a narrowband receiver,
//! wrong for a decoder that needs every sample. A ring-buffer variant
//! lands when a decoder preset actually needs it.
//!
//! ## Lifetime
//!
//! - [`SoapySource::new`] opens the device, configures it, and
//!   activates the Rx stream. Failure here surfaces at flowgraph
//!   instantiation rather than first tick.
//! - [`Block::init`] spawns the reader thread, sized to the
//!   scheduler's `frames_hint`.
//! - [`Block::process`] copies the latest block, zero-filling when the
//!   reader hasn't produced one yet.
//! - `Drop` flips a stop flag and joins the reader.
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

type LatestSlot = Arc<Mutex<Option<Vec<Complex<f32>>>>>;

pub struct SoapySource {
    /// Cloneable handle to the open device. Held so retune / gain calls
    /// can issue from the pipeline thread while the reader holds the
    /// `RxStream`. Reconfigure hooks land in M3.
    #[allow(dead_code)]
    device: Device,
    #[allow(dead_code)]
    channel: usize,
    sample_rate_hz: f64,
    center_freq_hz: f64,
    /// Some before [`Block::init`]; taken and moved into the reader
    /// thread when init spawns it.
    stream: Option<RxStream<Complex<f32>>>,
    latest: LatestSlot,
    /// Bumped by the reader after every successful write to `latest`.
    /// Pipeline compares it against `last_seen_version` to distinguish
    /// fresh blocks from a re-served stale one.
    version: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    last_seen_version: u64,
    ticks: u64,
    gaps: u64,
    stale: u64,
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
            latest: Arc::new(Mutex::new(None)),
            version: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            reader: None,
            last_seen_version: 0,
            ticks: 0,
            gaps: 0,
            stale: 0,
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
                    // Swapping devices = reopen the whole source.
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
                    // Hardware clock parameter — must re-open the stream.
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
                    // Tuning is live on Soapy devices — just call set_freq.
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

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if self.reader.is_some() {
            return Err(anyhow!("SoapySource::init called more than once"));
        }
        let stream = self
            .stream
            .take()
            .ok_or_else(|| anyhow!("SoapySource stream missing at init — was new() called?"))?;
        let block_size = ctx.frames_hint.max(1);

        let reader_latest = self.latest.clone();
        let reader_version = self.version.clone();
        let reader_stop = self.stop.clone();
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

        // Version snapshot before the lock so a racing reader write
        // counts as "fresh" on the next tick rather than being missed.
        let version = self.version.load(Ordering::Acquire);
        let guard = self
            .latest
            .lock()
            .map_err(|_| anyhow!("soapy latest mutex poisoned"))?;
        self.ticks = self.ticks.saturating_add(1);

        let mut w = Work::new();
        if let Some(samples) = guard.as_ref() {
            let n = out.len().min(samples.len());
            out[..n].copy_from_slice(&samples[..n]);
            if n < out.len() {
                out[n..].fill(Complex::new(0.0, 0.0));
            }
            if version == self.last_seen_version {
                self.stale = self.stale.saturating_add(1);
            } else {
                self.last_seen_version = version;
            }
            w.produced[0] = out.len();
        } else {
            out.fill(Complex::new(0.0, 0.0));
            self.gaps = self.gaps.saturating_add(1);
            w.produced[0] = out.len();
        }
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
                    thread::sleep(Duration::from_millis(2));
                }
                Err(err) if err.code == ErrorCode::Overflow => {
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
                // Ping-pong scratch ↔ slot so steady-state does no allocation.
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
