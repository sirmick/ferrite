//! RSSI probe — measures the smoothed IQ power and emits periodic
//! events for a UI meter, pass-through on the IQ path.
//!
//! One IQ-in, one IQ-out (identity), one Events-out. Sits anywhere on
//! an IQ path without changing the signal: the scheduler copies the
//! input to `out` and drops an `{"rssi_dbfs": …}` line into `events`
//! every `emit_interval_ms`.
//!
//! ### Algorithm
//!
//! ```text
//! power[n] = (1 − α)·power[n−1] + α·|iq[n]|²
//! rssi_dbfs = 10·log10(max(power, 1e−30))
//! ```
//!
//! with `α = 1 − exp(−dt / τ)`. Emits the current `rssi_dbfs` at a
//! steady cadence (`emit_interval_ms`, default 50 ms ≈ 20 Hz) so the
//! UI meter has enough refresh rate to feel live without flooding the
//! events channel.
//!
//! ### Units
//!
//! `rssi_dbfs` is `10·log10(mean|iq|²)`. Assuming full-scale IQ has
//! magnitude 1.0, 0 dBFS is full-scale and values are ≤ 0 dBFS in
//! practice. Downstream consumers render this directly — no per-mode
//! calibration needed because the value is relative to IQ full-scale.

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct RssiProbeParams {
    /// Input IQ sample rate (Hz).
    pub sample_rate_hz: f32,
    /// Power-smoother time constant (ms).
    pub smooth_tau_ms: f32,
    /// Event emission period (ms). 50 ms = 20 Hz — enough for a live
    /// meter without saturating the events channel.
    pub emit_interval_ms: f32,
}

impl Default for RssiProbeParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000.0,
            smooth_tau_ms: 50.0,
            emit_interval_ms: 50.0,
        }
    }
}

pub struct RssiProbe {
    alpha: f32,
    power: f32,
    /// Samples between emissions.
    emit_stride: u64,
    /// Samples until the next emission.
    samples_until_emit: u64,
    /// Buffered JSON bytes waiting to drain to the events output.
    pending: Vec<u8>,
}

impl RssiProbe {
    /// Build a probe with the supplied params. Fails on non-positive
    /// rates or time constants.
    pub fn new(params: RssiProbeParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "rssi_probe sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        if !(params.smooth_tau_ms.is_finite() && params.smooth_tau_ms > 0.0) {
            bail!(
                "rssi_probe smooth_tau_ms must be > 0 (got {})",
                params.smooth_tau_ms
            );
        }
        if !(params.emit_interval_ms.is_finite() && params.emit_interval_ms > 0.0) {
            bail!(
                "rssi_probe emit_interval_ms must be > 0 (got {})",
                params.emit_interval_ms
            );
        }

        let dt = 1.0 / params.sample_rate_hz;
        let alpha = 1.0 - (-dt / (params.smooth_tau_ms * 1e-3)).exp();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let emit_stride = (params.sample_rate_hz * params.emit_interval_ms * 1e-3).max(1.0) as u64;

        Ok(Self {
            alpha,
            power: 0.0,
            emit_stride,
            samples_until_emit: emit_stride,
            pending: Vec::new(),
        })
    }

    fn emit(&mut self) {
        // f32::log10(0) is -inf; clamp to something reasonably finite.
        let rssi = 10.0_f32 * self.power.max(1e-30).log10();
        // Small manually-formatted JSON — avoids pulling serde_json's
        // ser overhead into the hot path. Two decimal places of dBFS
        // is plenty for a meter.
        let line = format!("{{\"rssi_dbfs\":{rssi:.2}}}\n");
        self.pending.extend_from_slice(line.as_bytes());
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for RssiProbe {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "RssiProbe",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[
                PortSpec {
                    name: "out",
                    port_type: PortType::IqF32,
                },
                PortSpec {
                    name: "events",
                    port_type: PortType::Events,
                },
            ],
            params: &[
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Input sample rate",
                    kind: ParamKind::Range {
                        min: 1_000.0,
                        max: 10_000_000.0,
                        step: 1.0,
                        default: 48_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Input rate of the IQ stream being measured.",
                },
                ParamSpec {
                    key: "smooth_tau_ms",
                    label: "Smoother τ",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 1_000.0,
                        step: 1.0,
                        default: 50.0,
                        unit: "ms",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "Exponential time constant on the power envelope. Shorter = peakier readings, longer = steadier; ~50 ms is the typical S-meter feel.",
                },
                ParamSpec {
                    key: "emit_interval_ms",
                    label: "Emit interval",
                    kind: ParamKind::Range {
                        min: 10.0,
                        max: 1_000.0,
                        step: 1.0,
                        default: 50.0,
                        unit: "ms",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "How often to publish an RSSI event. 50 ms ≈ 20 Hz refresh — fast enough to feel live without flooding the WS.",
                },
            ],
            ai_notes: "Wideband RSSI meter on an IQ stream. Emits structured events with smoothed power readings; drives the UI's S-meter and lets decoders see signal strength alongside their own state.",
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_iq_f32)
        else {
            return Ok(Work::new());
        };

        // Smooth power + schedule emissions in a single pass so the
        // emitted RSSI lines up with the samples they summarise.
        let n = src.len();
        for &z in src {
            let inst = z.re * z.re + z.im * z.im;
            self.power += self.alpha * (inst - self.power);
            self.samples_until_emit = self.samples_until_emit.saturating_sub(1);
            if self.samples_until_emit == 0 {
                self.emit();
                self.samples_until_emit = self.emit_stride;
            }
        }

        // Copy input IQ → pass-through IQ output. The two output ports
        // live in separate slots; we find each by name because the
        // scheduler's `outputs` order isn't guaranteed.
        let mut produced_iq = 0;
        let mut produced_events = 0;

        for port in io.outputs.iter_mut() {
            match port.name {
                "out" => {
                    if let Some(dst) = OutputPort::as_iq_f32_mut(port) {
                        let k = dst.len().min(src.len());
                        dst[..k].copy_from_slice(&src[..k]);
                        produced_iq = k;
                    }
                }
                "events" => {
                    if let crate::block::OutBuf::Events(dst) = &mut port.buf {
                        let take = self.pending.len().min(dst.len());
                        if take > 0 {
                            dst[..take].copy_from_slice(&self.pending[..take]);
                            self.pending.drain(..take);
                            produced_events = take;
                        }
                    }
                }
                _ => {}
            }
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = produced_iq;
        w.produced[1] = produced_events;
        Ok(w)
    }
}

impl BlockFactory for RssiProbe {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: RssiProbeParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(RssiProbe::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{RssiProbe, RssiProbeParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use num_complex::Complex;

    fn run(
        probe: &mut RssiProbe,
        input: &[Complex<f32>],
        event_cap: usize,
    ) -> (Vec<Complex<f32>>, Vec<u8>) {
        let mut out_iq = vec![Complex::new(0.0, 0.0); input.len()];
        let mut out_events = vec![0u8; event_cap];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(input),
        }];
        let mut outputs = [
            OutputPort {
                name: "out",
                meta: PortMeta::default(),
                buf: OutBuf::IqF32(&mut out_iq),
            },
            OutputPort {
                name: "events",
                meta: PortMeta::default(),
                buf: OutBuf::Events(&mut out_events),
            },
        ];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = probe.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], input.len());
        let ev_len = w.produced[1];
        let events = out_events[..ev_len].to_vec();
        (out_iq, events)
    }

    #[test]
    fn constructor_rejects_bad_params() {
        assert!(RssiProbe::new(RssiProbeParams {
            sample_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(RssiProbe::new(RssiProbeParams {
            smooth_tau_ms: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(RssiProbe::new(RssiProbeParams {
            emit_interval_ms: 0.0,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn passthrough_preserves_iq() {
        let params = RssiProbeParams::default();
        let mut probe = RssiProbe::new(params).unwrap();
        let input: Vec<Complex<f32>> = (0..1024)
            .map(|i| Complex::new((i as f32 * 0.01).sin(), (i as f32 * 0.01).cos()))
            .collect();
        let (out, _) = run(&mut probe, &input, 4096);
        for (i, (a, b)) in input.iter().zip(out.iter()).enumerate() {
            assert!(
                (a.re - b.re).abs() < f32::EPSILON && (a.im - b.im).abs() < f32::EPSILON,
                "passthrough mismatch at {i}"
            );
        }
    }

    #[test]
    fn full_scale_input_reports_near_zero_dbfs() {
        // |z| = 1 → power = 1 → rssi = 0 dBFS. A few τ of settling at
        // 10 ms τ in 48 kHz = 480 samples; 4096 is well past that.
        let params = RssiProbeParams {
            sample_rate_hz: 48_000.0,
            smooth_tau_ms: 10.0,
            emit_interval_ms: 50.0,
        };
        let mut probe = RssiProbe::new(params).unwrap();
        let input = vec![Complex::new(1.0_f32, 0.0); 4096];
        let (_, events) = run(&mut probe, &input, 4096);
        // Parse the last event — its rssi_dbfs should be ≈ 0.
        let text = std::str::from_utf8(&events).unwrap();
        let last = text.lines().last().expect("at least one event emitted");
        // last looks like: {"rssi_dbfs":0.00}
        let v_str = last
            .trim_start_matches("{\"rssi_dbfs\":")
            .trim_end_matches('}');
        let v: f32 = v_str.parse().unwrap();
        assert!(v.abs() < 0.2, "expected ≈ 0 dBFS, got {v}");
    }

    #[test]
    fn zero_input_reports_very_low_dbfs() {
        // All zeros → clamped rssi = 10·log10(1e-30) = -300 dBFS.
        let params = RssiProbeParams {
            sample_rate_hz: 48_000.0,
            smooth_tau_ms: 10.0,
            emit_interval_ms: 10.0, // more events for this short run
        };
        let mut probe = RssiProbe::new(params).unwrap();
        let input = vec![Complex::new(0.0_f32, 0.0); 1024];
        let (_, events) = run(&mut probe, &input, 4096);
        let text = std::str::from_utf8(&events).unwrap();
        let last = text.lines().last().expect("at least one event");
        let v_str = last
            .trim_start_matches("{\"rssi_dbfs\":")
            .trim_end_matches('}');
        let v: f32 = v_str.parse().unwrap();
        assert!(v < -200.0, "expected very low dBFS, got {v}");
    }

    #[test]
    fn emits_at_configured_rate() {
        // 48 kHz / 10 ms interval = 480 samples per event. 4800 samples
        // of input ≈ 10 events emitted.
        let params = RssiProbeParams {
            sample_rate_hz: 48_000.0,
            smooth_tau_ms: 10.0,
            emit_interval_ms: 10.0,
        };
        let mut probe = RssiProbe::new(params).unwrap();
        let input = vec![Complex::new(0.5_f32, 0.0); 4800];
        let (_, events) = run(&mut probe, &input, 8192);
        let count = std::str::from_utf8(&events).unwrap().lines().count();
        assert!(
            (9..=11).contains(&count),
            "expected ≈ 10 events, got {count}"
        );
    }
}
