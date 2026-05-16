//! AutoTune — coarse AFC estimator. Pass-through on the IQ path;
//! measures where the signal's energy sits and emits a suggested
//! `freq_shift_hz` correction on an events port.
//!
//! One IQ-in, one IQ-out (identity), one Events-out — the
//! [`RssiProbe`](crate::rssi_probe) shape. It does **not** wire back
//! into the channelizer itself: the Ferrite flowgraph is a forward
//! DAG, so AFC closes through the *control plane*, not a feedback
//! edge. AutoTune emits `{"afc_center_hz":…,"afc_shift_hz":…}`; the
//! same control consumer that handles a manual VFO retune applies it
//! to `Channelizer.freq_shift_hz`. That loop is slow and
//! latency-tolerant (carrier drift is sub-Hz/s) — exactly how real
//! AFC works, and why crossing the browser bridge as a control
//! message is fine.
//!
//! ### Estimator
//!
//! Power-weighted spectral centroid over a frequency gate:
//!
//! ```text
//! X = FFT(window);  P[k] = |X[k]|²
//! centre = Σ f[k]·P[k] / Σ P[k]   for f[k] ∈ [gate_lo, gate_hi]
//! ```
//!
//! Decoder-agnostic: the centroid of a 2-FSK pair is its midpoint, of
//! PSK its carrier, of an MFSK/MT63 block its centre — all the right
//! thing. `afc_shift_hz = centre − target_hz`: add it to the
//! channelizer's shift and the signal's energy lands at `target_hz`
//! (the per-decoder sweet spot, supplied — not searched).

// Centroid math is intentionally f32 (bin index → Hz); doc comments
// name DSP terms (FFT/AFC/DAG/PSK) that aren't code spans.
#![allow(clippy::cast_precision_loss, clippy::doc_markdown)]

use anyhow::{bail, Result};
use num_complex::Complex;
use rustfft::FftPlanner;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutBuf, OutputPort, ParamKind,
    ParamSpec, Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Power-weighted spectral centroid (Hz) of `iq` (sampled at `fs`)
/// over the bins whose frequency lies in `[gate_lo_hz, gate_hi_hz]`.
/// `None` if there is no energy in the gate. `iq.len()` is the FFT
/// size — pass a windowed power-of-two slice for speed.
#[must_use]
pub fn estimate_center_hz(
    iq: &[Complex<f32>],
    fs: f32,
    gate_lo_hz: f32,
    gate_hi_hz: f32,
) -> Option<f32> {
    let n = iq.len();
    if n == 0 || !(fs > 0.0) {
        return None;
    }
    let mut buf: Vec<Complex<f32>> = iq.to_vec();
    FftPlanner::<f32>::new()
        .plan_fft_forward(n)
        .process(&mut buf);

    let nf = n as f32;
    let (mut num, mut den) = (0.0f64, 0.0f64);
    for (k, x) in buf.iter().enumerate() {
        // Unshifted FFT bin → signed frequency.
        let f = if k <= n / 2 {
            k as f32 * fs / nf
        } else {
            (k as f32 - nf) * fs / nf
        };
        if f < gate_lo_hz || f > gate_hi_hz {
            continue;
        }
        let p = f64::from(x.re * x.re + x.im * x.im);
        num += f64::from(f) * p;
        den += p;
    }
    if den <= 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some((num / den) as f32)
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct AutoTuneParams {
    /// Input IQ rate. Construction hint; `ctx.input_rate` wins.
    pub sample_rate_hz: f32,
    /// Desired signal-energy centre in this IQ stream. `afc_shift_hz =
    /// measured − target`; a control consumer adds it to the
    /// channelizer shift so the signal lands here. The per-decoder
    /// sweet spot (supplied, not searched).
    pub target_hz: f32,
    /// Only consider energy within `±gate_hz` of `target_hz` — keeps
    /// the centroid off out-of-band noise / adjacent signals.
    pub gate_hz: f32,
    /// FFT window (samples, power of two). Bigger = finer Hz
    /// resolution, slower cadence.
    pub window: u32,
}

impl Default for AutoTuneParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 8_000.0,
            target_hz: 0.0,
            gate_hz: 3_500.0,
            window: 8_192,
        }
    }
}

pub struct AutoTune {
    params: AutoTuneParams,
    fs: f32,
    win: usize,
    buf: Vec<Complex<f32>>,
    pending: Vec<u8>,
}

impl AutoTune {
    pub fn new(params: AutoTuneParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!("auto_tune sample_rate_hz must be > 0");
        }
        if !(params.gate_hz.is_finite() && params.gate_hz > 0.0) {
            bail!("auto_tune gate_hz must be > 0");
        }
        let win = (params.window as usize).max(64);
        Ok(Self {
            params,
            fs: params.sample_rate_hz,
            win,
            buf: Vec::with_capacity(win),
            pending: Vec::new(),
        })
    }

    fn flush_estimate(&mut self) {
        let lo = self.params.target_hz - self.params.gate_hz;
        let hi = self.params.target_hz + self.params.gate_hz;
        if let Some(center) = estimate_center_hz(&self.buf, self.fs, lo, hi) {
            let shift = center - self.params.target_hz;
            self.pending.extend_from_slice(
                format!("{{\"afc_center_hz\":{center:.1},\"afc_shift_hz\":{shift:.1}}}\n")
                    .as_bytes(),
            );
        }
        self.buf.clear();
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for AutoTune {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "AutoTune",
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
                    key: "target_hz",
                    label: "Target centre",
                    kind: ParamKind::Range {
                        min: -50_000.0,
                        max: 50_000.0,
                        step: 1.0,
                        default: 0.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Where the signal's energy should sit. afc_shift_hz = measured − this; a control consumer adds it to the channelizer shift. Set to the decoder's audio sweet spot.",
                },
                ParamSpec {
                    key: "gate_hz",
                    label: "Search gate",
                    kind: ParamKind::Range {
                        min: 50.0,
                        max: 1_000_000.0,
                        step: 1.0,
                        default: 3_500.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Only energy within ±this of target is considered — excludes adjacent signals / out-of-band noise from the centroid.",
                },
                ParamSpec {
                    key: "window",
                    label: "FFT window",
                    kind: ParamKind::EnumNumeric {
                        values: &[2_048.0, 4_096.0, 8_192.0, 16_384.0, 32_768.0],
                        default: 8_192.0,
                        unit: "samples",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "FFT size. Bigger = finer Hz resolution but slower estimate cadence.",
                },
            ],
            ai_notes: "Coarse AFC: estimates the signal-energy centroid and emits a suggested freq_shift correction (events). Pass-through on IQ. Closes the AFC loop through the control plane (like a manual VFO retune), not a feedback edge — slow, latency-tolerant. Decoder-agnostic.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(r) = ctx.input_rate("in") {
            if r > 0.0 {
                #[allow(clippy::cast_possible_truncation)]
                {
                    self.fs = r as f32;
                }
            }
        }
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
        let n = src.len();

        for &z in src {
            self.buf.push(z);
            if self.buf.len() >= self.win {
                self.flush_estimate();
            }
        }

        for port in io.outputs.iter_mut() {
            match port.name {
                "out" => {
                    if let Some(dst) = OutputPort::as_iq_f32_mut(port) {
                        let k = dst.len().min(n);
                        dst[..k].copy_from_slice(&src[..k]);
                    }
                }
                "events" => {
                    if let OutBuf::Events(dst) = &mut port.buf {
                        let take = self.pending.len().min(dst.len());
                        if take > 0 {
                            dst[..take].copy_from_slice(&self.pending[..take]);
                            self.pending.drain(..take);
                        }
                    }
                }
                _ => {}
            }
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for AutoTune {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: AutoTuneParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(AutoTune::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{estimate_center_hz, AutoTune, AutoTuneParams};
    use crate::block::Block;
    use num_complex::Complex;

    fn tone(freq: f32, fs: f32, n: usize) -> Vec<Complex<f32>> {
        (0..n)
            .map(|i| {
                let th = core::f32::consts::TAU * freq * i as f32 / fs;
                Complex::new(th.cos(), th.sin())
            })
            .collect()
    }

    #[test]
    fn centroid_finds_a_complex_tone() {
        let fs = 48_000.0;
        let x = tone(7_000.0, fs, 8_192);
        let c = estimate_center_hz(&x, fs, -24_000.0, 24_000.0).unwrap();
        assert!((c - 7_000.0).abs() < 50.0, "got {c}");
    }

    #[test]
    fn gate_rejects_out_of_band_energy() {
        // Strong tone at +9 kHz, weak at +1 kHz; gate ±3 kHz around 0
        // must report ~1 kHz, ignoring the bigger out-of-gate tone.
        let fs = 48_000.0;
        let mut x = tone(1_000.0, fs, 8_192);
        for (i, s) in x.iter_mut().enumerate() {
            let th = core::f32::consts::TAU * 9_000.0 * i as f32 / fs;
            *s += Complex::new(th.cos(), th.sin()) * 4.0;
        }
        let c = estimate_center_hz(&x, fs, -3_000.0, 3_000.0).unwrap();
        assert!((c - 1_000.0).abs() < 80.0, "gate leaked, got {c}");
    }

    #[test]
    fn negative_offset() {
        let fs = 8_000.0;
        let x = tone(-1_200.0, fs, 8_192);
        let c = estimate_center_hz(&x, fs, -3_500.0, 3_500.0).unwrap();
        assert!((c + 1_200.0).abs() < 40.0, "got {c}");
    }

    #[test]
    fn block_emits_shift_event() {
        use crate::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
        let mut at = AutoTune::new(AutoTuneParams {
            sample_rate_hz: 48_000.0,
            target_hz: 0.0,
            gate_hz: 24_000.0,
            window: 8_192,
        })
        .unwrap();
        let x = tone(5_000.0, 48_000.0, 8_192);
        let mut out = vec![Complex::new(0.0_f32, 0.0); 8_192];
        let mut ev = vec![0u8; 256];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&x),
        }];
        let mut outputs = [
            OutputPort {
                name: "out",
                meta: PortMeta::default(),
                buf: OutBuf::IqF32(&mut out),
            },
            OutputPort {
                name: "events",
                meta: PortMeta::default(),
                buf: OutBuf::Events(&mut ev),
            },
        ];
        at.process(&mut BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        })
        .unwrap();
        let s = String::from_utf8_lossy(&ev);
        assert!(s.contains("afc_center_hz"), "no event: {s:?}");
        assert!(s.contains("afc_shift_hz"));
        // Identity passthrough.
        assert!((out[10] - x[10]).norm() < 1e-6);
    }
}
