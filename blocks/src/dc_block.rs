//! Complex IQ DC-blocker — `IqF32` in, `IqF32` out, same rate.
//!
//! Removes the zero-frequency (DC) term in the complex baseband — the
//! permanent spike a zero-IF / direct-conversion SDR (HackRF, RTL-SDR,
//! …) parks at the tuned centre from LO self-mixing and I/Q DC offset.
//! It's the *artifact* we want gone, so we kill it once, at the IQ
//! stream, **before** the FFT tap splits — then the spike is absent from
//! the wideband waterfall, the narrow channel FFT, captures, and the
//! `SignalList` watchlist alike. Compare that to a per-consumer
//! frequency notch (the old `SignalList::dc_notch_hz`), which had to be
//! re-applied everywhere and blinded a wide band around centre.
//!
//! Implementation is the same one-pole/one-zero high-pass as
//! [`AudioShaper`](crate::audio_shaper)'s DC blocker (`y = x − x₁ +
//! R·y₁`), run independently on I and Q. With a sub-kHz corner against a
//! multi-MHz source rate, `R` sits a hair below 1: an exact null at DC,
//! flat passband within a few hundred Hz. A real carrier even 1 kHz off
//! centre is untouched — and on zero-IF you tune the signal off centre
//! anyway (the DC dodge), so the null never lands on programme.
//!
//! `enabled` and `corner_hz` are [`ReconfigureScope::SelfBlock`]: they
//! apply live on the next `process` with no graph rebuild, so an
//! operator can toggle the spike removal on/off and watch the centre bin
//! appear/vanish in real time. Hardware DC correction (Soapy
//! `dc_offset_mode`, where the device supports it) still runs at the
//! source; this is the software fallback for the radios that don't.

use anyhow::{bail, Result};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Default high-pass corner (Hz). Sub-kHz: kills DC, leaves any real
/// carrier even a hair off centre alone.
const DEFAULT_CORNER_HZ: f32 = 200.0;
/// Fallback rate when the runtime hasn't populated the input rate yet.
const DEFAULT_RATE_HZ: f32 = 2_400_000.0;

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct DcBlockParams {
    /// Input/output sample rate (Hz). Construction-time hint; init's
    /// `ctx.input_rate("in")` wins once the runtime populates it.
    pub sample_rate_hz: f32,
    /// High-pass corner (Hz). `0` (or negative) bypasses, same as
    /// `enabled = false`. A few hundred Hz is plenty to null DC.
    pub corner_hz: f32,
    /// Master enable. `false` = pure pass-through (the spike is left in).
    /// Live-togglable so you can A/B the spike removal.
    pub enabled: bool,
}

impl Default for DcBlockParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: DEFAULT_RATE_HZ,
            corner_hz: DEFAULT_CORNER_HZ,
            enabled: true,
        }
    }
}

pub struct DcBlock {
    params: DcBlockParams,
    input_rate_hz: f64,
    // One-pole/one-zero high-pass, one state pair per quadrature.
    dc_r: f32,
    x1_i: f32,
    y1_i: f32,
    x1_q: f32,
    y1_q: f32,
}

impl DcBlock {
    pub fn new(params: DcBlockParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "dc_block sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        if !params.corner_hz.is_finite() || params.corner_hz < 0.0 {
            bail!("dc_block corner_hz must be >= 0 (got {})", params.corner_hz);
        }
        let mut s = Self {
            params,
            input_rate_hz: 0.0,
            dc_r: 0.0,
            x1_i: 0.0,
            y1_i: 0.0,
            x1_q: 0.0,
            y1_q: 0.0,
        };
        s.recompute(f64::from(params.sample_rate_hz));
        Ok(s)
    }

    /// Recompute the pole from the corner + rate; reset the filter state
    /// so a re-enable / corner change starts clean (the settle is
    /// ~1/corner seconds — sub-10 ms at these corners, inaudible).
    fn recompute(&mut self, fs: f64) {
        if fs > 0.0 {
            self.input_rate_hz = fs;
        }
        let fs = if self.input_rate_hz > 0.0 {
            self.input_rate_hz
        } else {
            f64::from(DEFAULT_RATE_HZ)
        };
        // R = 1 - 2π·fc/fs (one-pole/one-zero high-pass), clamped just
        // below 1 so the null at DC stays exact without going unstable.
        #[allow(clippy::cast_possible_truncation)]
        {
            self.dc_r = (1.0 - core::f64::consts::TAU * f64::from(self.params.corner_hz) / fs)
                .clamp(0.0, 0.9999) as f32;
        }
        self.x1_i = 0.0;
        self.y1_i = 0.0;
        self.x1_q = 0.0;
        self.y1_q = 0.0;
    }

    #[inline]
    fn active(&self) -> bool {
        self.params.enabled && self.params.corner_hz > 0.0
    }

    #[inline]
    fn dc(&mut self, x: Complex<f32>) -> Complex<f32> {
        let yi = x.re - self.x1_i + self.dc_r * self.y1_i;
        self.x1_i = x.re;
        self.y1_i = yi;
        let yq = x.im - self.x1_q + self.dc_r * self.y1_q;
        self.x1_q = x.im;
        self.y1_q = yq;
        Complex::new(yi, yq)
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for DcBlock {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "DcBlock",
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
                    key: "enabled",
                    label: "DC blocker",
                    kind: ParamKind::Toggle { default: true },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Master enable for the IQ DC blocker. On = remove the zero-IF LO/DC spike at the tuned centre before the FFT split (spike gone from waterfall + narrow FFT + strongest-signal list). Off = pure pass-through, spike left in. Live toggle — flip it to A/B whether a centre bin is a real carrier or the DC artifact.",
                },
                ParamSpec {
                    key: "corner_hz",
                    label: "DC corner",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 20_000.0,
                        step: 10.0,
                        default: 200.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "High-pass corner. ~200 Hz nulls DC while leaving a carrier even 1 kHz off centre untouched. Raise it if a stubborn DC pedestal remains; 0 = bypass (same as disabling). Applies live.",
                },
                ParamSpec {
                    key: "sample_rate_hz",
                    label: "Sample rate",
                    kind: ParamKind::Range {
                        min: 1_000.0,
                        max: 100_000_000.0,
                        step: 1.0,
                        default: 2_400_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "IQ rate the blocker runs at — normally inherited from the source via port-meta, so you rarely set this by hand.",
                },
            ],
            ai_notes: "Complex IQ DC blocker (IqF32 in → IqF32 out). Inject between the source and the FFT tap so the zero-IF centre spike is removed once, consistently, for every downstream consumer. Replaces per-detector frequency notches. `enabled`/`corner_hz` are live (SelfBlock). Harmless to leave always-injected; disable for sources with good hardware DC correction if you prefer.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.recompute(rate);
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.recompute(rate);
            }
        }
        Ok(())
    }

    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let Some(obj) = delta.as_object() else {
            return Ok(false);
        };
        const LIVE_KEYS: &[&str] = &["enabled", "corner_hz"];
        if !obj.keys().all(|k| LIVE_KEYS.contains(&k.as_str())) {
            return Ok(false);
        }
        if let Some(v) = obj.get("enabled").and_then(serde_json::Value::as_bool) {
            self.params.enabled = v;
        }
        if let Some(v) = obj.get("corner_hz").and_then(serde_json::Value::as_f64) {
            if !v.is_finite() || v < 0.0 {
                bail!("dc_block corner_hz must be >= 0 (got {v})");
            }
            #[allow(clippy::cast_possible_truncation)]
            {
                self.params.corner_hz = v as f32;
            }
        }
        // Recompute the pole (and reset state) for the new corner/enable.
        self.recompute(self.input_rate_hz);
        Ok(true)
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
        let Some(dst) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_iq_f32_mut)
        else {
            return Ok(Work::new());
        };

        let n = src.len().min(dst.len());
        if self.active() {
            for i in 0..n {
                dst[i] = self.dc(src[i]);
            }
        } else {
            dst[..n].copy_from_slice(&src[..n]);
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for DcBlock {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: DcBlockParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(DcBlock::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{DcBlock, DcBlockParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use num_complex::Complex;

    fn run(s: &mut DcBlock, input: &[Complex<f32>]) -> Vec<Complex<f32>> {
        let mut out = vec![Complex::new(0.0, 0.0); input.len()];
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
        let w = s.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], input.len());
        assert_eq!(w.produced[0], input.len());
        out
    }

    /// Complex tone at `freq` Hz, with an added complex DC offset.
    fn tone_with_dc(freq: f32, fs: f32, n: usize, dc: Complex<f32>) -> Vec<Complex<f32>> {
        (0..n)
            .map(|i| {
                let ph = core::f32::consts::TAU * freq * i as f32 / fs;
                Complex::new(ph.cos(), ph.sin()) + dc
            })
            .collect()
    }

    fn mean(xs: &[Complex<f32>]) -> Complex<f32> {
        let s: Complex<f32> = xs.iter().copied().sum();
        s / xs.len() as f32
    }

    fn rms(xs: &[Complex<f32>]) -> f32 {
        (xs.iter().map(Complex::norm_sqr).sum::<f32>() / xs.len() as f32).sqrt()
    }

    #[test]
    fn rejects_bad_params() {
        assert!(DcBlock::new(DcBlockParams {
            sample_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(DcBlock::new(DcBlockParams {
            corner_hz: -1.0,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn removes_complex_dc_keeps_offset_carrier() {
        // 2.4 MHz IQ, a strong carrier 50 kHz off centre, sitting on a
        // fat complex DC offset (the zero-IF spike). The DC must die; the
        // off-centre carrier must survive.
        let fs = 2_400_000.0_f32;
        let n = 200_000;
        let dc = Complex::new(0.8, -0.5);
        let sig = tone_with_dc(50_000.0, fs, n, dc);
        let mut s = DcBlock::new(DcBlockParams {
            sample_rate_hz: fs,
            corner_hz: 200.0,
            enabled: true,
        })
        .unwrap();
        let out = run(&mut s, &sig);
        let tail = &out[20_000..];
        let m = mean(tail);
        assert!(
            m.norm() < 0.02,
            "complex DC not removed: residual mean {m} (|{}|)",
            m.norm()
        );
        // Unit carrier preserved (~1.0 RMS for a complex exponential).
        assert!(
            (rms(tail) - 1.0).abs() < 0.05,
            "off-centre carrier altered: rms {}",
            rms(tail)
        );
    }

    #[test]
    fn disabled_is_passthrough() {
        let fs = 2_400_000.0_f32;
        let sig = tone_with_dc(10_000.0, fs, 8_192, Complex::new(0.7, 0.3));
        let mut s = DcBlock::new(DcBlockParams {
            sample_rate_hz: fs,
            corner_hz: 200.0,
            enabled: false,
        })
        .unwrap();
        let out = run(&mut s, &sig);
        for (i, (a, b)) in sig.iter().zip(out.iter()).enumerate() {
            assert!((a - b).norm() < 1e-6, "sample {i}: {a} != {b}");
        }
    }

    #[test]
    fn corner_zero_is_passthrough() {
        let fs = 2_400_000.0_f32;
        let sig = tone_with_dc(10_000.0, fs, 4_096, Complex::new(0.7, 0.3));
        let mut s = DcBlock::new(DcBlockParams {
            sample_rate_hz: fs,
            corner_hz: 0.0,
            enabled: true,
        })
        .unwrap();
        let out = run(&mut s, &sig);
        for (a, b) in sig.iter().zip(out.iter()) {
            assert!((a - b).norm() < 1e-6);
        }
    }

    #[test]
    fn apply_live_params_toggles_without_rebuild() {
        let fs = 2_400_000.0_f32;
        let mut s = DcBlock::new(DcBlockParams {
            sample_rate_hz: fs,
            corner_hz: 200.0,
            enabled: true,
        })
        .unwrap();
        // Live disable accepted.
        assert!(s
            .apply_live_params(&serde_json::json!({ "enabled": false }))
            .unwrap());
        let sig = tone_with_dc(10_000.0, fs, 4_096, Complex::new(0.6, 0.2));
        let out = run(&mut s, &sig);
        for (a, b) in sig.iter().zip(out.iter()) {
            assert!((a - b).norm() < 1e-6, "disabled live should pass through");
        }
        // Live corner change accepted; a non-live key is rejected.
        assert!(s
            .apply_live_params(&serde_json::json!({ "corner_hz": 500.0 }))
            .unwrap());
        assert!(!s
            .apply_live_params(&serde_json::json!({ "sample_rate_hz": 1_000_000.0 }))
            .unwrap());
    }
}
