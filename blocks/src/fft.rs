//! `FFT` block — windowed complex FFT.
//!
//! Reads `size` samples of `iq_f32` per invocation, applies a window,
//! performs an in-place forward FFT, fft-shifts so DC lands at index
//! `size/2`, and writes `size` complex bins to the output port.
//!
//! Deliberately pure cf32 → cf32 — log-magnitude, dBFS mapping, and u8
//! quantization live in the downstream [`crate::log_mag_u8::LogMagU8`]
//! block so the "contrast" sliders users twist at runtime don't have to
//! tear through this hot-path code.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use num_complex::Complex;
use rustfft::{Fft as RustFft, FftPlanner};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work, MAX_PORTS,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FftWindow {
    None,
    Hann,
    Hamming,
    Blackman,
}

impl FftWindow {
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "none" => Some(Self::None),
            "hann" => Some(Self::Hann),
            "hamming" => Some(Self::Hamming),
            "blackman" => Some(Self::Blackman),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct FftBlockParams {
    pub size: usize,
    pub window: FftWindow,
}

impl Default for FftBlockParams {
    fn default() -> Self {
        Self {
            size: 4096,
            window: FftWindow::Hann,
        }
    }
}

pub struct FftBlock {
    params: FftBlockParams,
    plan: Arc<dyn RustFft<f32>>,
    window: Vec<f32>,
    scratch: Vec<Complex<f32>>,
    // Accumulates input across `process` calls until a full `size`-sample
    // frame is ready. The runtime's back-pressure is coarse: unconsumed
    // input is not re-presented next tick, so an FFT that needs more
    // samples than one upstream tick produces must buffer internally.
    accum: Vec<Complex<f32>>,
}

impl FftBlock {
    pub fn new(params: FftBlockParams) -> Result<Self> {
        if !params.size.is_power_of_two() || params.size < 64 {
            return Err(anyhow!(
                "fft size must be power-of-two and ≥ 64, got {}",
                params.size
            ));
        }
        let mut planner = FftPlanner::<f32>::new();
        let plan = planner.plan_fft_forward(params.size);
        let window = build_window(params.window, params.size);
        let scratch = vec![Complex::new(0.0, 0.0); params.size];
        let accum = Vec::with_capacity(params.size);
        Ok(Self {
            params,
            plan,
            window,
            scratch,
            accum,
        })
    }

    #[must_use]
    pub const fn size(&self) -> usize {
        self.params.size
    }
}

#[allow(clippy::cast_precision_loss)]
fn build_window(kind: FftWindow, size: usize) -> Vec<f32> {
    if matches!(kind, FftWindow::None) {
        return vec![1.0; size];
    }
    let denom = (size - 1) as f32;
    (0..size)
        .map(|i| {
            let t = core::f32::consts::TAU * (i as f32) / denom;
            match kind {
                FftWindow::None => 1.0,
                FftWindow::Hann => 0.5 - 0.5 * t.cos(),
                FftWindow::Hamming => 0.54 - 0.46 * t.cos(),
                FftWindow::Blackman => 0.42 - 0.5 * t.cos() + 0.08 * (2.0 * t).cos(),
            }
        })
        .collect()
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for FftBlock {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "FFT",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[
                ParamSpec {
                    key: "size",
                    label: "FFT size",
                    kind: ParamKind::EnumNumeric {
                        values: &[1024.0, 2048.0, 4096.0, 8192.0, 16384.0],
                        default: 4096.0,
                        unit: "bins",
                    },
                    // Changing FFT size changes the output port's bin count,
                    // so every downstream consumer must re-init.
                    reconfig_scope: ReconfigureScope::Downstream,
                },
                ParamSpec {
                    key: "window",
                    label: "Window",
                    kind: ParamKind::EnumString {
                        values: &["none", "hann", "hamming", "blackman"],
                        default: "hann",
                    },
                    // Window is just a coefficient table — swap in place
                    // on the next `process` call.
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
            ],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn output_capacity_hints(&self) -> [usize; MAX_PORTS] {
        let mut h = [0; MAX_PORTS];
        h[0] = self.params.size;
        h
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let n = self.params.size;
        let src = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_iq_f32);
        let Some(src) = src else {
            return Ok(Work::new());
        };

        // Pull every available input sample into the internal accumulator.
        // Cap at `n` — anything beyond a full frame is discarded rather
        // than queued, since the runtime already gave us a fresh view
        // next tick and overlapping frames aren't useful for the
        // waterfall.
        let take = (n - self.accum.len()).min(src.len());
        self.accum.extend_from_slice(&src[..take]);
        let mut work = Work::new();
        work.consumed[0] = src.len();

        if self.accum.len() < n {
            return Ok(work);
        }

        let dst = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_iq_f32_mut);
        let Some(dst) = dst else {
            return Ok(work);
        };
        if dst.len() < n {
            return Ok(work);
        }

        for (i, (&s, &w)) in self.accum[..n].iter().zip(self.window.iter()).enumerate() {
            self.scratch[i] = s * w;
        }
        self.plan.process(&mut self.scratch);

        // FFT-shift: DC bin moves from index 0 to the centre so the
        // waterfall displays negative freqs on the left and positive on
        // the right without a second pass.
        let half = n / 2;
        for (k, out) in dst[..n].iter_mut().enumerate() {
            *out = self.scratch[(k + half) % n];
        }

        self.accum.clear();
        work.produced[0] = n;
        Ok(work)
    }
}

impl BlockFactory for FftBlock {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: FftBlockParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(FftBlock::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{FftBlock, FftBlockParams, FftWindow};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use num_complex::Complex;

    fn run_fft(block: &mut FftBlock, input: &[Complex<f32>]) -> Vec<Complex<f32>> {
        let mut out = vec![Complex::new(0.0, 0.0); block.size()];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(input),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = block.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], block.size());
        assert_eq!(w.produced[0], block.size());
        out
    }

    #[test]
    fn zero_input_zero_output() {
        let mut block = FftBlock::new(FftBlockParams {
            size: 64,
            window: FftWindow::None,
        })
        .unwrap();
        let input = vec![Complex::new(0.0_f32, 0.0); 64];
        let out = run_fft(&mut block, &input);
        for s in out {
            assert!(s.norm() < 1e-6);
        }
    }

    #[test]
    fn dc_input_peaks_at_center() {
        // Rectangular window, constant (1+0j) input → energy concentrates
        // at DC. After fft-shift DC is at index n/2 and magnitude = n.
        let n = 64;
        let mut block = FftBlock::new(FftBlockParams {
            size: n,
            window: FftWindow::None,
        })
        .unwrap();
        let input = vec![Complex::new(1.0_f32, 0.0); n];
        let out = run_fft(&mut block, &input);
        #[allow(clippy::cast_precision_loss)]
        let n_f = n as f32;
        for (i, s) in out.iter().enumerate() {
            if i == n / 2 {
                assert!((s.norm() - n_f).abs() < 1e-3, "dc bin: {}", s.norm());
            } else {
                assert!(s.norm() < 1e-3, "non-dc bin {i}: {}", s.norm());
            }
        }
    }

    #[test]
    fn pure_tone_peaks_at_expected_bin() {
        // Complex exponential at normalised frequency k/n → peak at
        // output bin n/2 + k (after fft-shift). Pick k = 5 with n = 64
        // so it's distinguishable by index.
        let n = 64usize;
        let k = 5usize;
        let mut block = FftBlock::new(FftBlockParams {
            size: n,
            window: FftWindow::None,
        })
        .unwrap();
        #[allow(clippy::cast_precision_loss)]
        let (n_f, k_f) = (n as f32, k as f32);
        let mut input = Vec::with_capacity(n);
        for i in 0..n {
            #[allow(clippy::cast_precision_loss)]
            let i_f = i as f32;
            let theta = core::f32::consts::TAU * k_f * i_f / n_f;
            input.push(Complex::new(theta.cos(), theta.sin()));
        }
        let out = run_fft(&mut block, &input);
        let expected_bin = n / 2 + k;
        let peak = (0..n)
            .max_by(|a, b| out[*a].norm().partial_cmp(&out[*b].norm()).unwrap())
            .unwrap();
        assert_eq!(peak, expected_bin);
        assert!((out[expected_bin].norm() - n_f).abs() < 1e-2);
    }

    #[test]
    fn rejects_non_power_of_two() {
        assert!(FftBlock::new(FftBlockParams {
            size: 96,
            window: FftWindow::Hann
        })
        .is_err());
    }
}
