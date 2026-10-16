//! `SweepSource` — the spectrum analyzer on a real radio: a
//! [`RadioHandle`] (the same open / gain / readback / safe-release
//! lifecycle as `SoapySource`) driven by the stepped-sweep loop in
//! [`crate::sweep`]. Emits panorama rows on `events`.
//!
//! The rate is fixed for the block's lifetime (a rate change on an open
//! stream crashed SoapySDRServer — see `docs/plans/specan.md`); a new
//! rate is a new block. AGC is forced off (levels must compare across
//! steps) and the IF filter is set to the widest one the rate allows.

#![cfg(feature = "soapysdr")]

use std::sync::OnceLock;

use anyhow::{Context, Result};
use num_complex::Complex;
use serde::Deserialize;
use soapysdr::{Device, Direction};

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, ParamSpec, Placement, Work, MAX_PORTS,
};
use crate::soapy_device::{
    RadioHandle, RxReadError, RxStreamLike, SoapyReadback, SoapyRx, SoapySourceParams,
    SourceHealth, HUNG_STALL, RADIO_LIVE_KEYS,
};
use crate::sweep::{
    events_hints, leak_params, sweep_params_with, SweepParams, SweepPlan, SweepRadio, SweepRead,
    SweepRunner, PANORAMA_OUTPUTS,
};

/// Radio params + sweep params, flat in the preset JSON.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SweepSourceParams {
    #[serde(flatten)]
    pub radio: SoapySourceParams,
    #[serde(flatten)]
    pub sweep: SweepParams,
}

/// The real radio behind the sweep loop.
struct SoapySweepRadio {
    stream: SoapyRx,
    device: Device,
    channel: usize,
    rate: f64,
}

impl SweepRadio for SoapySweepRadio {
    fn rate_hz(&self) -> f64 {
        self.rate
    }

    fn tune(&mut self, hz: f64) -> Result<()> {
        self.device
            .set_frequency(Direction::Rx, self.channel, hz, ())
            .with_context(|| format!("sweep set_frequency={hz}"))
    }

    fn read(&mut self, buf: &mut [Complex<f32>]) -> SweepRead {
        match self.stream.read(buf, 100_000) {
            Ok(n) => SweepRead::Samples(n),
            Err(RxReadError::Timeout) => SweepRead::Timeout,
            Err(RxReadError::Overflow) => SweepRead::Overflow,
            Err(RxReadError::Fatal(m)) => SweepRead::Fatal(m),
        }
    }
}

impl Drop for SoapySweepRadio {
    fn drop(&mut self) {
        self.stream.deactivate();
    }
}

pub struct SweepSource {
    radio: RadioHandle,
    /// Taken by `init` and moved into the hop thread.
    pending: Option<SoapySweepRadio>,
    runner: SweepRunner,
}

impl SweepSource {
    pub fn new(params: &SweepSourceParams) -> Result<Self> {
        let rate = params.radio.sample_rate_hz;
        SweepPlan::new(&params.sweep, rate)?;
        let mut radio_params = params.radio.clone();
        // Levels must compare across steps and passes.
        radio_params.agc = Some(false);
        // The live source's filter belongs to the receiver's rate; the
        // sweep picks its own below.
        radio_params.bandwidth_hz = None;
        let (radio, stream) = RadioHandle::open(&radio_params)?;
        let rate = radio.sample_rate_hz;
        let dir = Direction::Rx;
        let ch = radio.channel;
        // Widest IF filter the rate allows (the usable fraction assumes it).
        if let Ok(ranges) = radio.device().bandwidth_range(dir, ch) {
            let best = ranges
                .iter()
                .flat_map(|r| [r.minimum, r.maximum])
                .filter(|bw| *bw <= rate + 1.0)
                .fold(0.0f64, f64::max);
            if best > 0.0 {
                if let Err(err) = radio.device().set_bandwidth(dir, ch, best) {
                    tracing::warn!(?err, bw = best, "sweep: set_bandwidth failed (ignored)");
                }
            }
        }
        tracing::info!(
            rate_hz = rate,
            bandwidth_hz = ?radio.device().bandwidth(dir, ch).ok(),
            start = params.sweep.sweep_start_hz,
            stop = params.sweep.sweep_stop_hz,
            "SweepSource opened"
        );
        let device = radio.device().clone();
        Ok(Self {
            pending: Some(SoapySweepRadio {
                stream: SoapyRx(stream),
                device,
                channel: ch,
                rate,
            }),
            runner: SweepRunner::new(params.sweep.clone()),
            radio,
        })
    }

    /// Radio liveness, same shape as `SoapySource::health`, so the
    /// daemon's supervisor treats a sweep like any hardware source.
    #[must_use]
    pub fn health(&self) -> SourceHealth {
        use std::sync::atomic::Ordering;
        let s = &self.runner.shared;
        let last = s.last_sample_at_ns.load(Ordering::Relaxed);
        let stalled_ms = (last != 0).then(|| {
            (self.runner.anchor.elapsed().as_nanos() as u64).saturating_sub(last) / 1_000_000
        });
        SourceHealth {
            remote: self.radio.is_remote,
            hung: s.hung.load(Ordering::Relaxed),
            reader_ended: s.reader_ended.load(Ordering::Relaxed),
            reader_error: s.reader_error.lock().ok().and_then(|g| g.clone()),
            stalled_ms,
            age_ms: u64::try_from(self.runner.anchor.elapsed().as_millis()).unwrap_or(u64::MAX),
            samples_pushed: s.samples_pushed.load(Ordering::Relaxed),
            overflow_drops: 0,
            ring_drops: 0,
            ring_gaps: 0,
        }
    }

    pub fn readback(&self) -> SoapyReadback {
        self.radio.readback()
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for SweepSource {
    fn spec() -> BlockSpec {
        static PARAMS: OnceLock<&'static [ParamSpec]> = OnceLock::new();
        BlockSpec {
            type_name: "SweepSource",
            placement: Placement::NativeOnly,
            inputs: &[],
            outputs: PANORAMA_OUTPUTS,
            params: PARAMS.get_or_init(|| {
                // The radio knobs are SoapySource's (same handle, same
                // meaning) minus the ones a sweep owns itself.
                let radio: Vec<ParamSpec> = crate::SoapySource::spec()
                    .params
                    .iter()
                    .filter(|p| {
                        matches!(
                            p.key,
                            "args" | "sample_rate_hz" | "gain_db" | "gain_mode" | "antenna"
                                | "dc_offset_correction" | "channel"
                        )
                    })
                    .copied()
                    .collect();
                leak_params(sweep_params_with(&radio))
            }),
            ai_notes: "Spectrum-analyzer source: hops the radio's LO across [sweep_start_hz, sweep_stop_hz] (~7.6 MHz/step at 10 MS/s), FFT-averages each step, keeps the flat middle of the passband and emits one row per step on `events` → ui:panorama (store kind `panorama`, keyed by step; power = base64 i16 centi-dBFS). AGC is forced off and the rate is fixed for the block's lifetime. Live: the sweep_* / rbw_hz / averages knobs restart the pass; gain/antenna apply directly.",
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        let radio = self
            .pending
            .take()
            .ok_or_else(|| anyhow::anyhow!("SweepSource::init called twice"))?;
        self.runner.start(radio, HUNG_STALL)
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        Ok(self.runner.drain(io))
    }

    fn output_capacity_hints(&self) -> [usize; MAX_PORTS] {
        events_hints()
    }

    fn stop(&mut self) -> Result<()> {
        self.runner.stop();
        Ok(())
    }

    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let Some(obj) = delta.as_object() else {
            return Ok(false);
        };
        // All keys must be live (sweep or radio) or the whole delta
        // rebuilds — never apply half.
        let (sweep, radio): (serde_json::Map<_, _>, serde_json::Map<_, _>) = obj
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .partition(|(k, _)| crate::sweep::SWEEP_LIVE_KEYS.contains(&k.as_str()));
        // AGC stays off on a sweep; the LO is the sweep's.
        if radio
            .keys()
            .any(|k| !RADIO_LIVE_KEYS.contains(&k.as_str()) || k == "center_freq_hz" || k == "agc")
        {
            return Ok(false);
        }
        if !radio.is_empty() {
            self.radio.apply_live(&radio)?;
        }
        if !sweep.is_empty() {
            self.runner.apply_live(&sweep)?;
        }
        Ok(true)
    }
}

impl Drop for SweepSource {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.runner.stop();
        // Close the stream (if the hop thread never started) before the
        // device is released.
        self.pending = None;
        let hung = self.runner.shared.hung.load(Ordering::Relaxed);
        self.radio.release(hung);
    }
}

impl BlockFactory for SweepSource {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: SweepSourceParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(Self::new(&p)?))
    }
}
