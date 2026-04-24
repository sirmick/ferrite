//! RDS (Radio Data System) decoder — Phase 1 scaffold.
//!
//! RDS is the ~1.2 kbps data stream broadcast FM stations carry on a
//! 57 kHz suppressed-carrier subcarrier. Every commercial FM signal
//! has it. This block takes the FM demod's MPX output, recovers the
//! 57 kHz subcarrier, coherently mixes it to baseband, and emits the
//! biphase-encoded data envelope for a downstream bit-sync / block-
//! sync / group-parser to turn into PS / RT / PI events.
//!
//! ### Why 57 kHz is trivially coherent to recover
//!
//! Broadcasters constrain the 57 kHz subcarrier to be phase-locked to
//! the 19 kHz stereo pilot at its third harmonic. A PLL that locks to
//! the pilot directly hands us the subcarrier reference — no separate
//! acquisition, no squaring loop. We run an `Nco` PLL at 19 kHz and
//! multiply its phase by 3 to synthesise the 57 kHz local oscillator.
//!
//! ### Signal chain (this commit)
//!
//! ```text
//! MPX @ Fs ──┬── BPF 18.5..19.5 kHz ── Nco PLL lock ── phase19
//!            │                                             ├── ×3 → phase57
//! MPX @ Fs ──┼── BPF 54..60 kHz ── × cos(phase57) ─┬── LPF 2.4 kHz ── I (data)
//!            │                   ── × sin(phase57) ┴── LPF 2.4 kHz ── Q (lock)
//!            └── delay match ──────────────────────────────────────────┘
//! ```
//!
//! I and Q are decimated by `DECIMATE_TO_BAUD_RATIO × symbol_rate` so
//! downstream bit sync sees a manageable sample count. The decimated
//! stream is what the block emits today; bit/block/group decoding and
//! JSON event emission arrive in subsequent commits.
//!
//! ### Rate support
//!
//! Requires `sample_rate_hz ≥ 128 kHz` so the 57 kHz subcarrier lands
//! below Nyquist with margin. 240 kHz (the standard wbfm MPX rate) is
//! the primary target.

use anyhow::{bail, Result};
use ferrite_liquid_dsp::Nco;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// RDS subcarrier (Hz) — fixed by the spec at 3 × 19 kHz.
pub const RDS_SUBCARRIER_HZ: f32 = 57_000.0;
/// RDS pilot (Hz) — the 19 kHz stereo pilot the subcarrier is locked to.
pub const RDS_PILOT_HZ: f32 = 19_000.0;
/// RDS baud (bps) — biphase symbol rate = subcarrier / 48.
pub const RDS_BAUD: f32 = 1_187.5;

/// Ratio of the output sample rate to the RDS baud. 8× baud (=9500 Hz)
/// is enough oversampling for the downstream bit-sync to find the
/// transition midpoints without being expensive.
pub const DECIMATE_TO_BAUD_RATIO: usize = 8;

const PILOT_BPF_LO: f32 = 18_500.0;
const PILOT_BPF_HI: f32 = 19_500.0;
// 57 ± 2.4 kHz captures the biphase sidebands without pulling in the
// 67 kHz auxiliary sub (SCA) or the 76 kHz RBDS extension.
const RDS_BPF_LO: f32 = 54_600.0;
const RDS_BPF_HI: f32 = 59_400.0;
const NUM_TAPS_PILOT: usize = 161;
const NUM_TAPS_RDS: usize = 161;
const NUM_TAPS_LPF: usize = 201;
/// Loop bandwidth as a fraction of Fs. Same regime as the stereo
/// decoder's pilot PLL — narrow enough to reject leakage, fast enough
/// to acquire in tens of ms.
const PLL_BANDWIDTH_NORM: f32 = 5e-4;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct RdsDemodParams {
    /// Input MPX sample rate (Hz). Typically 240 kHz.
    pub sample_rate_hz: f32,
}

impl Default for RdsDemodParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 240_000.0,
        }
    }
}

/// Windowed-sinc FIR used for both the narrow pilot / RDS band-passes
/// and the baseband low-pass. Same design as the stereo decoder's
/// private `BandPass` — Hamming-windowed, odd length for linear phase.
struct Fir {
    taps: Vec<f32>,
    ring: Vec<f32>,
    mask: usize,
    write: usize,
}

impl Fir {
    fn bandpass(fs: f32, f_lo: f32, f_hi: f32, num_taps: usize) -> Self {
        assert!(
            num_taps >= 3 && num_taps % 2 == 1,
            "num_taps must be odd ≥ 3"
        );
        #[allow(clippy::cast_precision_loss)]
        let m = (num_taps - 1) as f32 / 2.0;
        let wl = core::f32::consts::TAU * f_lo / fs;
        let wh = core::f32::consts::TAU * f_hi / fs;
        let mut taps = Vec::with_capacity(num_taps);
        #[allow(clippy::cast_precision_loss)]
        for n in 0..num_taps {
            let k = n as f32 - m;
            let ideal = if k == 0.0 {
                (wh - wl) / core::f32::consts::PI
            } else {
                ((wh * k).sin() - (wl * k).sin()) / (core::f32::consts::PI * k)
            };
            let w = 0.54 - 0.46 * (core::f32::consts::TAU * n as f32 / (num_taps - 1) as f32).cos();
            taps.push(ideal * w);
        }
        let buf_len = num_taps.next_power_of_two();
        Self {
            taps,
            ring: vec![0.0; buf_len],
            mask: buf_len - 1,
            write: 0,
        }
    }

    fn lowpass(fs: f32, f_c: f32, num_taps: usize) -> Self {
        Self::bandpass(fs, 0.0, f_c, num_taps)
    }

    fn step(&mut self, x: f32) -> f32 {
        self.ring[self.write] = x;
        self.write = (self.write + 1) & self.mask;
        let n = self.taps.len();
        let mut acc = 0.0_f32;
        for (i, &t) in self.taps.iter().enumerate() {
            let idx = (self.write + self.mask + 1 - n + i) & self.mask;
            acc += t * self.ring[idx];
        }
        acc
    }

    fn group_delay(&self) -> usize {
        (self.taps.len() - 1) / 2
    }
}

pub struct RdsDemod {
    params: RdsDemodParams,
    sample_rate_hz: f32,
    /// Integer decimation factor computed from `sample_rate_hz` and
    /// [`DECIMATE_TO_BAUD_RATIO`]. Output rate = `Fs / decim`.
    decim: usize,
    /// Phase counter for symbol-rate decimation — when it hits `decim`
    /// the accumulator emits one output sample.
    decim_phase: usize,

    pilot_bpf: Fir,
    rds_bpf: Fir,
    lpf_i: Fir,
    lpf_q: Fir,
    /// Pilot PLL — 19 kHz phase accumulator; ×3 gives 57 kHz.
    pilot_nco: Nco,

    /// Delay line to align the RDS-band signal with the PLL-derived
    /// reference. Pilot BPF introduces group delay the PLL sees but
    /// the RDS mixer needs the *sample-time* version of the RDS
    /// subcarrier too.
    delay_line: Vec<f32>,
    delay_write: usize,
    delay_len: usize,

    /// Lock detection — smoothed Q-channel power. RDS data shows up
    /// on I when the PLL is locked; Q dips to near-zero noise. We
    /// expose this for later event emission / UI health indicators.
    q_power: f32,
    i_power: f32,
    alpha_lock: f32,

    /// Mean-accumulator-based decimator output. Collecting `decim`
    /// samples and averaging gives a built-in anti-alias LPF for the
    /// decimation stage (in addition to the explicit 2.4 kHz LPF).
    accum_i: f32,
    accum_q: f32,
}

impl RdsDemod {
    pub fn new(params: RdsDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz >= 128_000.0) {
            bail!(
                "rds_demod sample_rate_hz must be ≥ 128000 (got {})",
                params.sample_rate_hz
            );
        }
        Self::build_from_rate(params, params.sample_rate_hz)
    }

    fn build_from_rate(params: RdsDemodParams, fs: f32) -> Result<Self> {
        let pilot_bpf = Fir::bandpass(fs, PILOT_BPF_LO, PILOT_BPF_HI, NUM_TAPS_PILOT);
        let rds_bpf = Fir::bandpass(fs, RDS_BPF_LO, RDS_BPF_HI, NUM_TAPS_RDS);
        // Baseband LPF cut above the Manchester signal's highest
        // significant energy — RDS uses biphase-Manchester at 1187.5
        // bps, so the main lobe sits around ±2.4 kHz from carrier.
        let lpf_i = Fir::lowpass(fs, 2_400.0, NUM_TAPS_LPF);
        let lpf_q = Fir::lowpass(fs, 2_400.0, NUM_TAPS_LPF);

        let mut pilot_nco = Nco::new().map_err(|e| anyhow::anyhow!("nco_crcf: {e}"))?;
        pilot_nco.set_frequency(core::f32::consts::TAU * RDS_PILOT_HZ / fs);
        pilot_nco.pll_set_bandwidth(PLL_BANDWIDTH_NORM);

        // Align RDS-band samples with pilot-derived reference; both
        // filters have the same tap count so their group delays match.
        let delay_len = pilot_bpf.group_delay();

        // Output rate ≈ DECIMATE_TO_BAUD_RATIO × baud. Round to the
        // nearest integer decimation factor to stay timing-consistent;
        // at 240 kHz this picks 25 (target 25.26) — close enough for
        // the bit-sync stage to phase-correct later.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let decim = ((fs / (RDS_BAUD * DECIMATE_TO_BAUD_RATIO as f32)).round() as usize).max(1);

        let alpha_lock = 1.0 - (-core::f32::consts::TAU * 20.0 / fs).exp();

        Ok(Self {
            params,
            sample_rate_hz: fs,
            decim,
            decim_phase: 0,
            pilot_bpf,
            rds_bpf,
            lpf_i,
            lpf_q,
            pilot_nco,
            delay_line: vec![0.0; delay_len],
            delay_write: 0,
            delay_len,
            q_power: 0.0,
            i_power: 0.0,
            alpha_lock,
            accum_i: 0.0,
            accum_q: 0.0,
        })
    }

    /// Current smoothed I-channel signal power. Data sits on I once the
    /// PLL is locked and the subcarrier polarity is correct; useful as
    /// a health indicator.
    #[must_use]
    pub const fn i_power(&self) -> f32 {
        self.i_power
    }

    /// Current smoothed Q-channel noise power. Low Q relative to I
    /// means the 57 kHz reference is in quadrature with the received
    /// subcarrier — the canonical "locked and coherent" condition.
    #[must_use]
    pub const fn q_power(&self) -> f32 {
        self.q_power
    }

    /// Integer decimation factor from MPX rate to the output data rate.
    #[must_use]
    pub const fn decimation(&self) -> usize {
        self.decim
    }

    /// Output sample rate emitted on the `data` port.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn output_rate_hz(&self) -> f32 {
        self.sample_rate_hz / self.decim as f32
    }

    /// Core per-sample pump: pulls one MPX sample, updates the pilot
    /// PLL, generates the 57 kHz reference, mixes, filters, and
    /// returns `Some((i, q))` when the decimation accumulator has a
    /// fresh output.
    fn pump(&mut self, mpx: f32) -> Option<(f32, f32)> {
        // Pilot path: BPF → PLL. The NCO's phase drifts with the live
        // pilot; `pll_step` takes a phase-error measurement.
        let pilot = self.pilot_bpf.step(mpx);
        let (ref_re, ref_im) = self.pilot_nco.cexpf();
        // Phase detector: imag(pilot × conj(ref)) = −pilot · ref_im,
        // up to scaling. Works because the filtered pilot is (roughly)
        // `A·cos(ω·n+θ)`; multiplying by `-sin(ω·n)` gives `A·sin(θ)/2
        // + AC`, and the PLL treats that as the error signal.
        let phase_err = pilot * (-ref_im);
        self.pilot_nco.pll_step(phase_err);
        self.pilot_nco.step();

        let phase19 = self.pilot_nco.phase();
        // 57 kHz LO = 3× pilot phase. `cos(3θ)` via the trig identity
        // `cos(3θ) = 4cos³θ − 3cosθ` avoids another sin/cos evaluation;
        // same for `sin(3θ) = 3sinθ − 4sin³θ`. Since we already have
        // `ref_re = cosθ` and `ref_im = sinθ` we just cube.
        let c = ref_re;
        let s = ref_im;
        let cos3 = 4.0 * c * c * c - 3.0 * c;
        let sin3 = 3.0 * s - 4.0 * s * s * s;
        let _ = phase19; // kept for future diagnostics

        // RDS path: delay-align with the pilot path, then BPF.
        let delayed = self.delay_line[self.delay_write];
        self.delay_line[self.delay_write] = mpx;
        self.delay_write = (self.delay_write + 1) % self.delay_len.max(1);
        let rds_band = self.rds_bpf.step(delayed);

        // Coherent mix: multiply by the 57 kHz I and Q references.
        // Post-LPF, the I branch carries the biphase-modulated data,
        // the Q branch carries noise + any phase-quadrature leakage.
        let mixed_i = rds_band * cos3;
        let mixed_q = rds_band * sin3;
        let i = self.lpf_i.step(mixed_i);
        let q = self.lpf_q.step(mixed_q);

        // Smooth signal/noise power trackers (single-pole IIR, ~20 Hz
        // cut) for lock indication.
        self.i_power += self.alpha_lock * (i * i - self.i_power);
        self.q_power += self.alpha_lock * (q * q - self.q_power);

        // Decimation: running sum over `decim` samples, emit average.
        self.accum_i += i;
        self.accum_q += q;
        self.decim_phase += 1;
        if self.decim_phase >= self.decim {
            #[allow(clippy::cast_precision_loss)]
            let norm = 1.0 / self.decim as f32;
            let out_i = self.accum_i * norm;
            let out_q = self.accum_q * norm;
            self.accum_i = 0.0;
            self.accum_q = 0.0;
            self.decim_phase = 0;
            Some((out_i, out_q))
        } else {
            None
        }
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for RdsDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "RdsDemod",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[PortSpec {
                name: "data",
                port_type: PortType::RealF32,
            }],
            params: &[ParamSpec {
                key: "sample_rate_hz",
                label: "MPX sample rate",
                kind: ParamKind::Range {
                    min: 128_000.0,
                    max: 2_400_000.0,
                    step: 1.0,
                    default: 240_000.0,
                    unit: "Hz",
                },
                reconfig_scope: ReconfigureScope::SourceRestart,
            }],
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            #[allow(clippy::cast_possible_truncation)]
            let fs = rate as f32;
            if fs > 0.0 && (fs - self.sample_rate_hz).abs() > 1.0 {
                // Rebuild internals against the actual scheduler-negotiated
                // rate. This is a new-object swap rather than an in-place
                // update so every filter is freshly designed at the right
                // cutoffs.
                *self = Self::build_from_rate(self.params, fs)?;
            }
        }
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            #[allow(clippy::cast_possible_truncation)]
            let fs = rate as f32;
            if fs > 0.0 && (fs - self.sample_rate_hz).abs() > 1.0 {
                *self = Self::build_from_rate(self.params, fs)?;
            }
        }
        Ok(())
    }

    fn relative_rate(&self, _in_port: usize, _out_port: usize) -> (u32, u32) {
        // Output rate = Fs / decim. Express as a rational so the
        // scheduler can size the downstream buffer correctly.
        #[allow(clippy::cast_possible_truncation)]
        let d = self.decim as u32;
        (1, d.max(1))
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
        else {
            return Ok(Work::new());
        };
        let Some(dst) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "data")
            .and_then(OutputPort::as_real_f32_mut)
        else {
            return Ok(Work::new());
        };

        let mut consumed = 0;
        let mut produced = 0;
        for &x in src.iter() {
            consumed += 1;
            if let Some((i, _q)) = self.pump(x) {
                if produced >= dst.len() {
                    // Output port full — stop consuming so the
                    // scheduler re-presents the remainder next tick.
                    consumed -= 1;
                    self.decim_phase = self.decim; // re-emit on next pump
                    self.accum_i += i; // keep the sample we just consumed
                    break;
                }
                dst[produced] = i;
                produced += 1;
            }
        }
        let mut w = Work::new();
        w.consumed[0] = consumed;
        w.produced[0] = produced;
        Ok(w)
    }
}

impl BlockFactory for RdsDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: RdsDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(RdsDemod::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{RdsDemod, RdsDemodParams, RDS_PILOT_HZ, RDS_SUBCARRIER_HZ};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;

    fn rms(xs: &[f32]) -> f32 {
        (xs.iter().map(|v| v * v).sum::<f32>() / xs.len() as f32).sqrt()
    }

    fn run(block: &mut RdsDemod, mpx: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0_f32; mpx.len() / block.decim + 64];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(mpx),
        }];
        let mut outputs = [OutputPort {
            name: "data",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = block.process(&mut io).unwrap();
        out.truncate(w.produced[0]);
        out
    }

    #[test]
    fn rejects_bad_rate() {
        assert!(RdsDemod::new(RdsDemodParams {
            sample_rate_hz: 48_000.0,
        })
        .is_err());
    }

    #[test]
    fn locks_pilot_and_demods_square_wave_on_carrier() {
        // Build a synthetic MPX: 19 kHz pilot + a 1 kHz square wave
        // amplitude-modulating a 57 kHz carrier (DSB-SC). The square
        // wave stands in for Manchester-shaped data in this scaffold
        // test; Phase 2 will bring real biphase at 1187.5 bps. The I
        // output should track the square wave at DC (post-decim), and
        // the I power should far exceed the Q power after lock.
        let fs = 240_000.0_f32;
        let n = (fs as usize) * 1; // 1 s
        let carrier_hz = RDS_SUBCARRIER_HZ;
        let data_hz = 1_000.0_f32;

        let mpx: Vec<f32> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                let pilot = 0.1 * (TAU * RDS_PILOT_HZ * t).sin();
                // Square-wave data: ±1 alternating at `data_hz`.
                let sq = if (TAU * data_hz * t).sin() >= 0.0 {
                    1.0
                } else {
                    -1.0
                };
                let sub = sq * (TAU * carrier_hz * t).sin();
                pilot + 0.3 * sub
            })
            .collect();

        let mut block = RdsDemod::new(RdsDemodParams { sample_rate_hz: fs }).unwrap();
        let out = run(&mut block, &mpx);

        // Skip the pilot-PLL acquisition transient and the filter
        // warmup (a few hundred ms).
        let warm = out.len() / 4;
        let tail = &out[warm..];
        let tail_rms = rms(tail);

        // I-channel output at baseband should carry meaningful energy
        // — the square wave's fundamental sits well below the 2.4 kHz
        // LPF cut-off, so the envelope survives.
        assert!(
            tail_rms > 0.05,
            "I-channel RMS too low: {tail_rms:.4} — PLL probably didn't lock"
        );

        // Coherence check: once the PLL is locked, I · I should
        // dominate Q · Q. Using the smoothed power trackers that
        // update every input sample, so by the end of the 1 s
        // fixture they've converged.
        let i_p = block.i_power();
        let q_p = block.q_power();
        let ratio_db = 10.0 * (i_p / q_p.max(1e-12)).log10();
        assert!(
            ratio_db > 10.0,
            "PLL coherence poor: I/Q = {ratio_db:.1} dB \
             (I={i_p:.5}, Q={q_p:.5})"
        );
    }

    #[test]
    fn output_rate_is_decimated() {
        let fs = 240_000.0_f32;
        let block = RdsDemod::new(RdsDemodParams { sample_rate_hz: fs }).unwrap();
        // At 240 kHz with 8× baud oversample target, decim ≈ 25 and
        // output rate lands close to 8 × 1187.5 = 9500 Hz.
        let out_rate = block.output_rate_hz();
        assert!(
            (out_rate - 9_500.0).abs() < 1_000.0,
            "output rate off: {out_rate}"
        );
    }
}
