//! `LogMagU8` — complex FFT bins → dBFS quantised to u8.
//!
//! Each input bin's power is normalised by the FFT size to compute dBFS,
//! exponentially smoothed across frames, and linearly mapped from a
//! **fixed** `[SERVER_FLOOR_DBFS, SERVER_CEIL_DBFS]` window to `[0, 255]`.
//! That's a deliberate architectural choice: the server's quantisation
//! window is wide and constant (−160..0 dBFS, 0.625 dB/byte), and every
//! display-range decision is made client-side on top of the byte stream.
//! No more "weak signal invisible because the preset shipped floor=−100"
//! foot-gun; no more server round-trip to zoom the spectrum.
//!
//! Only `alpha` (per-bin EMA smoothing) remains a live-tunable block
//! param — it's a real signal-processing choice, not a display one.

use anyhow::Result;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work, MAX_PORTS,
};

/// Low end of the server quantisation window. Byte 0 maps to this.
pub const SERVER_FLOOR_DBFS: f32 = -160.0;
/// High end of the server quantisation window. Byte 255 maps to this.
pub const SERVER_CEIL_DBFS: f32 = 0.0;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct LogMagU8Params {
    pub size: usize,
    /// Exponential smoothing factor in `(0, 1]`. 1.0 means "no smoothing"
    /// (each frame overwrites the previous); lower values hold noise
    /// floors steadier.
    pub alpha: f32,
}

impl Default for LogMagU8Params {
    fn default() -> Self {
        Self {
            size: 4096,
            alpha: 0.3,
        }
    }
}

pub struct LogMagU8 {
    params: LogMagU8Params,
    /// Per-bin smoothed dBFS value. Initialised to the server floor so
    /// the first frame emits the colormap's black level instead of a
    /// spike.
    smoothed: Vec<f32>,
}

impl LogMagU8 {
    #[must_use]
    pub fn new(params: LogMagU8Params) -> Self {
        let smoothed = vec![SERVER_FLOOR_DBFS; params.size];
        Self { params, smoothed }
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
                    // Output bin count — downstream consumers re-init.
                    reconfig_scope: ReconfigureScope::Downstream,
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
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
            ],
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    /// Live apply `alpha` (EMA smoothing factor). `size` reallocates
    /// the smoothing buffer so it still falls back to block-rebuild.
    /// Display-range decisions (formerly floor/ceil) moved client-side,
    /// so nothing else belongs here.
    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let Some(obj) = delta.as_object() else {
            return Ok(false);
        };
        const LIVE_KEYS: &[&str] = &["alpha"];
        if !obj.keys().all(|k| LIVE_KEYS.contains(&k.as_str())) {
            return Ok(false);
        }
        if let Some(v) = obj.get("alpha").and_then(|v| v.as_f64()) {
            self.set_alpha(v as f32);
        }
        Ok(true)
    }

    fn output_capacity_hints(&self) -> [usize; MAX_PORTS] {
        let mut h = [0; MAX_PORTS];
        h[0] = self.params.size;
        h
    }

    fn forecast(&self, _noutput_items: usize) -> Option<[usize; MAX_PORTS]> {
        // LogMagU8 operates on whole FFT bin blocks: consume `size`
        // cf32 bins and emit `size` u8 magnitudes, atomically. Tell the
        // scheduler not to call us before a full window is available —
        // upstream is `FftBlock` which emits exactly `size` samples at
        // a time, so the wire fills in one shot.
        let mut f = [0; MAX_PORTS];
        f[0] = self.params.size;
        Some(f)
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

        // Fixed wide quantisation window — client handles any visual
        // zoom via a display-range remap on the byte stream.
        let floor = SERVER_FLOOR_DBFS;
        let ceil = SERVER_CEIL_DBFS;
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
        // SERVER_CEIL_DBFS = 0, maps to 255.
        let n = 64;
        let params = LogMagU8Params {
            size: n,
            alpha: 1.0,
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
    fn midpoint_of_server_window_maps_to_half() {
        // Power = 1e-8 → dBFS = -80. SERVER window is [-160, 0] so -80
        // is the exact halfway point → maps to ~128.
        let n = 64;
        let params = LogMagU8Params {
            size: n,
            alpha: 1.0,
        };
        let mut block = LogMagU8::new(params);
        // power = |s|² / n² = 1e-8 → |s|² = n² · 1e-8 → |s| = n · sqrt(1e-8).
        #[allow(clippy::cast_precision_loss)]
        let mag = (n as f32) * (1e-8_f32).sqrt();
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
        };
        let mut block = LogMagU8::new(params);
        #[allow(clippy::cast_precision_loss)]
        let peak = Complex::new(n as f32, 0.0);
        let input = vec![peak; n];
        let mut last = 0_u8;
        for _ in 0..100 {
            let out = run(&mut block, &input);
            last = out[0];
        }
        assert!(last >= 250, "expected convergence to ceil, got {last}");
    }
}
