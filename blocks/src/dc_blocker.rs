//! Single-pole high-pass at DC. Removes the LO-leakage spike that
//! zero-IF SDRs (HackRF everywhere, SDRplay above ~30 MHz) park on the
//! tuned centre frequency.
//!
//! The DC term shows up as a bright vertical line at the source
//! centre, dominates auto-contrast on the waterfall, and obliterates
//! whatever real signal happens to sit on-target when the user tunes
//! directly to it. The standard SDR workarounds are either:
//!
//! 1. Tune the source slightly off-target and pull the target back to
//!    baseband via the channelizer's `freq_shift_hz` — moves the spike
//!    to the corner of the wideband view rather than the centre.
//! 2. Apply a DC-blocking filter in the IQ stream — kills the spike
//!    entirely at the cost of suppressing a few Hz of real-signal
//!    energy at DC (negligible for everything except CW directly at
//!    carrier).
//!
//! Ferrite's [`DcBlocker`] is the second approach. Insert between the
//! source and the channelizer in any preset that wants on-tune
//! behaviour; gate with `"when": { "dc_block": true }` so the runtime
//! `Profile` can flip the whole chain on/off at the operator's request.
//!
//! ### Filter shape
//!
//! Single-pole IIR high-pass:
//!
//! ```text
//!     y[n] = x[n] - x[n-1] + R * y[n-1]
//! ```
//!
//! `R` lives in (0, 1). The pole sits at `R` on the real axis — the
//! closer to 1, the narrower the notch at DC. Typical values:
//!
//! - `R = 0.995` — wide-ish notch (~6 Hz at 250 kHz Fs), removes
//!   slow drift too. Default.
//! - `R = 0.999` — narrow, surgical (~1 Hz wide). Good when the
//!   target signal sits within tens of Hz of DC.
//! - `R = 0.9` — wider notch, kills more nearby DC-ish energy.
//!
//! For complex IQ the filter runs as two independent real filters on
//! I and Q — no cross-channel coupling. State is per-block-instance so
//! a re-init starts clean rather than carrying drift across a re-tune.

use anyhow::{bail, Result};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct DcBlockerParams {
    /// Pole position on the real axis in (0, 1). Higher = narrower
    /// notch at DC. 0.995 is the default — kills the LO spike plus
    /// slow drift without measurably touching anything past a few Hz.
    pub pole: f32,
}

impl Default for DcBlockerParams {
    fn default() -> Self {
        Self { pole: 0.995 }
    }
}

pub struct DcBlocker {
    pole: f32,
    prev_in: Complex<f32>,
    prev_out: Complex<f32>,
}

impl DcBlocker {
    pub fn new(params: DcBlockerParams) -> Result<Self> {
        if !(params.pole > 0.0 && params.pole < 1.0) {
            bail!(
                "dc_blocker: pole must be in (0, 1), got {} — values near 1 narrow the DC notch",
                params.pole
            );
        }
        Ok(Self {
            pole: params.pole,
            prev_in: Complex::new(0.0, 0.0),
            prev_out: Complex::new(0.0, 0.0),
        })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for DcBlocker {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "DcBlocker",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[ParamSpec {
                key: "pole",
                label: "Pole (R)",
                kind: ParamKind::Range {
                    min: 0.9,
                    max: 0.9999,
                    step: 0.0001,
                    default: 0.995,
                    unit: "",
                },
                reconfig_scope: ReconfigureScope::SelfBlock,
                ai_notes: "Pole position on the real axis. Higher = narrower notch at DC. 0.995 default for the LO-spike use case; raise toward 0.9999 if a signal sits within tens of Hz of carrier and the wider default attenuates it.",
            }],
            ai_notes: "DC blocker for zero-IF sources (HackRF everywhere, SDRplay above 30 MHz). Insert between the source and channelizer when tuning on-target; obviates the off-tune-and-VFO-shift workaround. Single-pole high-pass at DC — kills the LO spike entirely at the cost of suppressing a few Hz of real-signal energy at carrier. Gated by `profile.dc_block` via `when: { \"dc_block\": true }`.",
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        // Single-rate block — no port-meta dependencies; the filter
        // state is initialised at construct time.
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
        let dst = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_iq_f32_mut)
            .ok_or_else(|| anyhow::anyhow!("dc_blocker: missing iq_f32 output port"))?;

        let n = src.len().min(dst.len());
        for (x, y) in src[..n].iter().zip(dst[..n].iter_mut()) {
            // y[n] = x[n] - x[n-1] + R * y[n-1] — independently on I/Q.
            let out = *x - self.prev_in + self.prev_out * self.pole;
            self.prev_in = *x;
            self.prev_out = out;
            *y = out;
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for DcBlocker {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: DcBlockerParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(DcBlocker::new(p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{InBuf, InputPort, OutBuf as OutBufEnum, OutputPort, PortMeta};

    #[test]
    fn rejects_pole_out_of_range() {
        assert!(DcBlocker::new(DcBlockerParams { pole: 0.0 }).is_err());
        assert!(DcBlocker::new(DcBlockerParams { pole: 1.0 }).is_err());
        assert!(DcBlocker::new(DcBlockerParams { pole: 1.5 }).is_err());
    }

    fn run(block: &mut DcBlocker, input: &[Complex<f32>]) -> Vec<Complex<f32>> {
        let mut out = vec![Complex::new(0.0, 0.0); input.len()];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(input),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBufEnum::IqF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let work = block.process(&mut io).unwrap();
        assert_eq!(work.produced[0], input.len());
        out
    }

    #[test]
    fn pure_dc_input_decays_toward_zero() {
        // Constant input (= pure DC) should decay to ~0 at the output
        // after enough samples — that's exactly what we want a DC
        // blocker to do.
        let mut b = DcBlocker::new(DcBlockerParams { pole: 0.99 }).unwrap();
        let dc = vec![Complex::new(0.5_f32, -0.3); 4096];
        let out = run(&mut b, &dc);
        // First sample = x[0] - 0 + 0 = x[0] (the DC itself, before the
        // filter has memory). Last sample should be near zero after
        // several time constants.
        let tail = out[out.len() - 1];
        assert!(
            tail.re.abs() < 0.01 && tail.im.abs() < 0.01,
            "DC should decay near zero; got {tail}"
        );
    }

    #[test]
    fn ac_input_passes_through_largely_unchanged() {
        // A high-frequency oscillation should pass through with most
        // of its amplitude intact — the DC blocker is a high-pass,
        // not a brick-wall.
        let mut b = DcBlocker::new(DcBlockerParams { pole: 0.995 }).unwrap();
        // 0.25 cycles/sample → near Nyquist; well above the DC notch.
        let n = 4096;
        let ac: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let phase = i as f32 * std::f32::consts::TAU * 0.25;
                Complex::new(phase.cos(), phase.sin())
            })
            .collect();
        let out = run(&mut b, &ac);
        // Skip the first 100 samples (filter warm-up) and check the
        // remainder has near-unit magnitude.
        let mag_sum: f32 = out[100..].iter().map(|c| c.norm()).sum();
        let mean_mag = mag_sum / (out.len() - 100) as f32;
        assert!(
            (mean_mag - 1.0).abs() < 0.1,
            "AC signal should pass with ~unit gain; mean |y| = {mean_mag}"
        );
    }
}
