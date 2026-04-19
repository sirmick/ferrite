//! `LogMagU8` — complex FFT bins → dBFS quantised to u8.
//!
//! Each input bin's power is normalised by the FFT size to compute dBFS,
//! exponentially smoothed across frames, and linearly mapped from
//! `[floor_dbfs, ceil_dbfs]` to `[0, 255]` for direct consumption by a
//! waterfall colormap texture.
//!
//! The contrast knobs (`floor_dbfs`, `ceil_dbfs`, `alpha`) all live here
//! so the [`crate::fft::FftBlock`] upstream stays pure cf32 → cf32.

use anyhow::Result;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, Work,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct LogMagU8Params {
    pub size: usize,
    pub floor_dbfs: f32,
    pub ceil_dbfs: f32,
    /// Exponential smoothing factor in `(0, 1]`. 1.0 means "no smoothing"
    /// (each frame overwrites the previous); lower values hold noise
    /// floors steadier.
    pub alpha: f32,
}

impl Default for LogMagU8Params {
    fn default() -> Self {
        Self {
            size: 4096,
            floor_dbfs: -100.0,
            ceil_dbfs: 0.0,
            alpha: 0.3,
        }
    }
}

pub struct LogMagU8 {
    params: LogMagU8Params,
    /// Per-bin smoothed dBFS value. Initialised to `floor_dbfs` so the
    /// first frame emits the colormap's black level instead of a spike.
    smoothed: Vec<f32>,
}

impl LogMagU8 {
    #[must_use]
    pub fn new(params: LogMagU8Params) -> Self {
        let smoothed = vec![params.floor_dbfs; params.size];
        Self { params, smoothed }
    }

    pub fn set_floor_dbfs(&mut self, v: f32) {
        self.params.floor_dbfs = v;
    }

    pub fn set_ceil_dbfs(&mut self, v: f32) {
        self.params.ceil_dbfs = v;
    }

    pub fn set_alpha(&mut self, v: f32) {
        self.params.alpha = v.clamp(0.0, 1.0);
    }

    #[must_use]
    pub fn smoothed(&self) -> &[f32] {
        &self.smoothed
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for LogMagU8 {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "LogMagU8",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::FftU8,
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
                    mutable_while_streaming: false,
                },
                ParamSpec {
                    key: "floor_dbfs",
                    label: "Noise floor",
                    kind: ParamKind::Range {
                        min: -160.0,
                        max: 0.0,
                        step: 1.0,
                        default: -100.0,
                        unit: "dBFS",
                    },
                    mutable_while_streaming: true,
                },
                ParamSpec {
                    key: "ceil_dbfs",
                    label: "Ceiling",
                    kind: ParamKind::Range {
                        min: -60.0,
                        max: 60.0,
                        step: 1.0,
                        default: 0.0,
                        unit: "dBFS",
                    },
                    mutable_while_streaming: true,
                },
                ParamSpec {
                    key: "alpha",
                    label: "Smoothing",
                    kind: ParamKind::Range {
                        min: 0.01,
                        max: 1.0,
                        step: 0.01,
                        default: 0.3,
                        unit: "",
                    },
                    mutable_while_streaming: true,
                },
            ],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
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
        if src.len() < n {
            return Ok(Work::new());
        }
        let dst = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_fft_u8_mut);
        let Some(dst) = dst else {
            return Ok(Work::new());
        };
        if dst.len() < n {
            return Ok(Work::new());
        }

        let floor = self.params.floor_dbfs;
        let ceil = self.params.ceil_dbfs;
        let alpha = self.params.alpha;
        let scale = 255.0 / (ceil - floor).max(1e-6);
        #[allow(clippy::cast_precision_loss)]
        let norm = 1.0 / (n as f32 * n as f32);

        for (i, s) in src[..n].iter().enumerate() {
            let power = (s.re * s.re + s.im * s.im) * norm;
            let db = 10.0 * power.max(1e-20).log10();
            let smoothed = alpha * db + (1.0 - alpha) * self.smoothed[i];
            self.smoothed[i] = smoothed;
            let clipped = (smoothed - floor).clamp(0.0, ceil - floor);
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let byte = (clipped * scale).round().clamp(0.0, 255.0) as u8;
            dst[i] = byte;
        }

        let mut work = Work::new();
        work.consumed[0] = n;
        work.produced[0] = n;
        Ok(work)
    }
}

impl BlockFactory for LogMagU8 {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: LogMagU8Params = crate::block::deserialize_params(params)?;
        Ok(Box::new(LogMagU8::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{LogMagU8, LogMagU8Params};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use num_complex::Complex;

    fn run(block: &mut LogMagU8, input: &[Complex<f32>]) -> Vec<u8> {
        let n = input.len();
        let mut out = vec![0_u8; n];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(input),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::FftU8(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let _ = block.process(&mut io).unwrap();
        out
    }

    #[test]
    fn zero_input_goes_to_floor() {
        // alpha=1.0 skips smoothing: zero magnitude → dBFS below floor → 0.
        let n = 64;
        let params = LogMagU8Params {
            size: n,
            alpha: 1.0,
            ..LogMagU8Params::default()
        };
        let mut block = LogMagU8::new(params);
        let input = vec![Complex::new(0.0_f32, 0.0); n];
        let out = run(&mut block, &input);
        for b in out {
            assert_eq!(b, 0);
        }
    }

    #[test]
    fn peak_bin_hits_ceiling() {
        // Magnitude n in one bin → power = n² / n² = 1.0 → dBFS = 0 →
        // with ceil=0, floor=-100, maps to 255.
        let n = 64;
        let params = LogMagU8Params {
            size: n,
            alpha: 1.0,
            floor_dbfs: -100.0,
            ceil_dbfs: 0.0,
        };
        let mut block = LogMagU8::new(params);
        let mut input = vec![Complex::new(0.0_f32, 0.0); n];
        #[allow(clippy::cast_precision_loss)]
        {
            input[n / 2] = Complex::new(n as f32, 0.0);
        }
        let out = run(&mut block, &input);
        assert_eq!(out[n / 2], 255);
        assert_eq!(out[0], 0);
    }

    #[test]
    fn midpoint_maps_to_half() {
        // Power = 1e-5 → dBFS = -50. floor=-100 ceil=0 span=100. -50 is
        // halfway → maps to ~128.
        let n = 64;
        let params = LogMagU8Params {
            size: n,
            alpha: 1.0,
            floor_dbfs: -100.0,
            ceil_dbfs: 0.0,
        };
        let mut block = LogMagU8::new(params);
        // power = |s|² / n² = 1e-5 → |s|² = n² · 1e-5 → |s| = n · sqrt(1e-5).
        #[allow(clippy::cast_precision_loss)]
        let mag = (n as f32) * (1e-5_f32).sqrt();
        let input = vec![Complex::new(mag, 0.0); n];
        let out = run(&mut block, &input);
        for b in out {
            assert!(b.abs_diff(128) <= 2, "expected ~128, got {b}");
        }
    }

    #[test]
    fn smoothing_converges() {
        // With alpha = 0.5, feeding ceil-level input repeatedly should
        // climb towards 255 over a handful of frames.
        let n = 16;
        let params = LogMagU8Params {
            size: n,
            alpha: 0.5,
            floor_dbfs: -100.0,
            ceil_dbfs: 0.0,
        };
        let mut block = LogMagU8::new(params);
        #[allow(clippy::cast_precision_loss)]
        let peak = Complex::new(n as f32, 0.0);
        let input = vec![peak; n];
        let mut last = 0_u8;
        for _ in 0..20 {
            let out = run(&mut block, &input);
            last = out[0];
        }
        assert!(last >= 250, "expected convergence to ceil, got {last}");
    }
}
