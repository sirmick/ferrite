//! FM broadcast stereo decoder — MPX composite → L / R baseband audio.
//!
//! One RealF32 in (the FM discriminator output, i.e. the MPX composite
//! at the demodulator's sample rate — e.g. 240 kHz in wbfm), two
//! RealF32 outputs (`left`, `right`) at the same rate, plus an events
//! output reporting pilot-lock state for a UI indicator.
//!
//! ### MPX structure
//!
//! A broadcast FM stereo signal carries:
//!
//! ```text
//! 0–15 kHz : L+R  (mono compatible)
//! 19 kHz   : pilot tone, ~10% amplitude
//! 23–53 kHz: L−R, DSB-SC around a suppressed 38 kHz carrier
//! 57 kHz   : RDS subcarrier (ignored here)
//! ```
//!
//! ### Algorithm — PLL-locked pilot, 2× phase for 38 kHz
//!
//! ```text
//! pilot       = BPF_19k(MPX)
//! // liquid's nco_crcf PLL locks its complex exponential to the pilot:
//! (I, Q)      = pilot · conj(nco.cexp())           // mix pilot down to DC
//! err         = Q                                  // small-angle ≈ atan2(Q, I)
//! nco.pll_step(err)                                // adjust phase/freq
//! nco.step()                                       // advance by nominal ω
//! ref_38k     = -sin(2 · nco.phase)                // 38 kHz, in phase w/ subcarrier
//! lmr_raw     = delayed_MPX · ref_38k · 2          // mix L−R down to baseband
//! l_minus_r   = LPF_15k(lmr_raw)
//! l_plus_r    = LPF_15k(delayed_MPX)
//! L           = (l_plus_r + l_minus_r) / 2
//! R           = (l_plus_r − l_minus_r) / 2
//! ```
//!
//! A PLL tracks the pilot by phase instead of amplitude, so it degrades
//! gracefully under weak-pilot conditions where a squaring detector
//! would lose the reference's DC offset. It also produces a pure
//! unit-amplitude sinusoid without a peak-tracker normalisation step.
//!
//! ### Pilot lock
//!
//! Emits a `{"pilot_lock": bool, "pilot_rms": f32}` event every
//! `emit_interval_ms` — the UI latches the lock bit to show a stereo
//! indicator and can render the pilot RMS as a signal-quality hint.
//! The lock bit is a simple threshold on the smoothed pilot RMS with
//! hysteresis so transient drops don't chatter.

use anyhow::{bail, Result};
use ferrite_liquid_dsp::Nco;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutBuf, OutputPort, ParamKind,
    ParamSpec, Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct StereoDecoderParams {
    /// Input sample rate (Hz). Must be ≥ 2 × 53 kHz (106 kHz) to avoid
    /// aliasing the L−R upper sideband; 240 kHz (wbfm) is typical.
    pub sample_rate_hz: f32,
    /// Pilot RMS threshold for `pilot_lock = true`. Values above this
    /// set the lock bit; it clears when the smoothed RMS drops below
    /// `lock_threshold · 0.6` (hysteresis built-in).
    pub lock_threshold: f32,
    /// Pilot-RMS smoother time constant (ms).
    pub lock_tau_ms: f32,
    /// Event emission period (ms).
    pub emit_interval_ms: f32,
}

impl Default for StereoDecoderParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 240_000.0,
            lock_threshold: 0.02,
            lock_tau_ms: 100.0,
            emit_interval_ms: 100.0,
        }
    }
}

/// Windowed-sinc band-pass filter. Odd length → linear phase around
/// the centre tap; simple ring buffer keeps the hot path branch-free.
struct BandPass {
    taps: Vec<f32>,
    ring: Vec<f32>,
    mask: usize,
    write: usize,
}

impl BandPass {
    /// Design a Hamming-windowed sinc BPF at `f_lo..f_hi` (Hz) for
    /// `fs`. `num_taps` should be odd to keep the filter type-I
    /// (linear phase, symmetric); the caller is expected to pass one.
    fn new(fs: f32, f_lo: f32, f_hi: f32, num_taps: usize) -> Self {
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
            // Band-pass = LPF(f_hi) − LPF(f_lo); windowed.
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

    /// Convolve one sample. Input pushed to the ring then dot-product
    /// the taps against the `num_taps` most recent samples.
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

/// Low-pass equivalent of [`BandPass`].
struct LowPass {
    inner: BandPass,
}

impl LowPass {
    fn new(fs: f32, f_c: f32, num_taps: usize) -> Self {
        // LPF = BPF(0, f_c).
        Self {
            inner: BandPass::new(fs, 0.0, f_c, num_taps),
        }
    }
    fn step(&mut self, x: f32) -> f32 {
        self.inner.step(x)
    }
}

// Filter lengths. Pilot BPF is 161 taps — narrow enough to isolate the
// 19 kHz pilot from the L+R passband below and the L−R sidebands above.
// Audio LPFs are the same length; their group delay matches the pilot
// BPF's so the three signal paths land co-timed at the matrix step.
const NUM_TAPS_PILOT: usize = 161;
const NUM_TAPS_AUDIO: usize = 161;

/// Target loop bandwidth for the pilot PLL, as a fraction of the input
/// sample rate. Narrow enough to reject the L+R and L−R energy that
/// leaks through the pilot BPF's shoulders; wide enough to acquire in
/// a few tens of ms under typical broadcast conditions.
const PLL_BANDWIDTH_NORM: f32 = 5e-4;

pub struct StereoDecoder {
    pilot_bpf: BandPass,
    /// NCO PLL-locked to the filtered pilot. Its phase is held at 19
    /// kHz; doubling it gives the 38 kHz reference for the L−R mixer.
    pilot_nco: Nco,
    lpf_sum: LowPass,
    lpf_diff: LowPass,

    /// Delay line for MPX so the audio paths align with the PLL
    /// reference — which has zero group delay after lock, but the
    /// pilot BPF the PLL reads still introduces its own delay.
    mpx_delay: Vec<f32>,
    mpx_delay_write: usize,
    mpx_delay_len: usize,

    /// Smoothed pilot RMS² for lock detection.
    pilot_power: f32,
    alpha_lock: f32,
    lock_threshold_power: f32,
    unlock_threshold_power: f32,
    pilot_locked: bool,
    /// Smoothed [0..1] blend factor applied to L−R before the matrix.
    /// 0 → fully mono (L=R=L+R); 1 → fully stereo. Tracks a ramp of
    /// `pilot_power` between the unlock and lock thresholds so weak
    /// pilots taper out the noisy L−R path instead of snapping off.
    /// Same smoother as `pilot_power`; transitions take a few ms.
    blend: f32,

    /// Event scheduler.
    emit_stride: u64,
    samples_until_emit: u64,
    pending: Vec<u8>,
}

impl StereoDecoder {
    pub fn new(params: StereoDecoderParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz >= 106_000.0) {
            bail!(
                "stereo_decoder sample_rate_hz must be ≥ 106000 (got {})",
                params.sample_rate_hz
            );
        }
        if !(params.lock_threshold.is_finite() && params.lock_threshold > 0.0) {
            bail!(
                "stereo_decoder lock_threshold must be > 0 (got {})",
                params.lock_threshold
            );
        }
        if !(params.lock_tau_ms.is_finite() && params.lock_tau_ms > 0.0) {
            bail!(
                "stereo_decoder lock_tau_ms must be > 0 (got {})",
                params.lock_tau_ms
            );
        }
        if !(params.emit_interval_ms.is_finite() && params.emit_interval_ms > 0.0) {
            bail!(
                "stereo_decoder emit_interval_ms must be > 0 (got {})",
                params.emit_interval_ms
            );
        }

        let fs = params.sample_rate_hz;
        let pilot_bpf = BandPass::new(fs, 18_500.0, 19_500.0, NUM_TAPS_PILOT);
        let lpf_sum = LowPass::new(fs, 15_000.0, NUM_TAPS_AUDIO);
        let lpf_diff = LowPass::new(fs, 15_000.0, NUM_TAPS_AUDIO);

        // NCO running nominally at 19 kHz; the PLL pulls it toward the
        // pilot's actual phase each sample. Starting the NCO at the
        // nominal frequency gives the PLL a tight capture range (we
        // just need to absorb a few Hz of residual offset) rather than
        // asking it to find 19 kHz from scratch.
        let mut pilot_nco = Nco::new().map_err(|e| anyhow::anyhow!("nco_crcf: {e}"))?;
        pilot_nco.set_frequency(core::f32::consts::TAU * 19_000.0 / fs);
        pilot_nco.pll_set_bandwidth(PLL_BANDWIDTH_NORM);

        // Align MPX with the pilot path: the PLL output is in-phase
        // with the *filtered* pilot, so MPX needs to be delayed by the
        // pilot BPF's group delay (and only that — the PLL itself adds
        // no deterministic lag once locked).
        let mpx_delay_len = pilot_bpf.group_delay();
        let mpx_delay = vec![0.0; mpx_delay_len.max(1)];

        let alpha_lock = 1.0 - (-(1.0 / fs) / (params.lock_tau_ms * 1e-3)).exp();
        let lock_threshold_power = params.lock_threshold * params.lock_threshold;
        let unlock_threshold_power = lock_threshold_power * 0.36; // 60% amplitude → 36% power.

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let emit_stride = (fs * params.emit_interval_ms * 1e-3).max(1.0) as u64;

        Ok(Self {
            pilot_bpf,
            pilot_nco,
            lpf_sum,
            lpf_diff,
            mpx_delay,
            mpx_delay_write: 0,
            mpx_delay_len,
            pilot_power: 0.0,
            alpha_lock,
            lock_threshold_power,
            unlock_threshold_power,
            pilot_locked: false,
            blend: 0.0,
            emit_stride,
            samples_until_emit: emit_stride,
            pending: Vec::new(),
        })
    }

    /// Current pilot lock state. Exposed for tests and the UI indicator.
    #[must_use]
    pub const fn pilot_locked(&self) -> bool {
        self.pilot_locked
    }

    fn emit_lock_event(&mut self) {
        let rms = self.pilot_power.sqrt();
        let lock = self.pilot_locked;
        let line = format!("{{\"pilot_lock\":{lock},\"pilot_rms\":{rms:.4}}}\n");
        self.pending.extend_from_slice(line.as_bytes());
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for StereoDecoder {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "StereoDecoder",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[
                PortSpec {
                    name: "left",
                    port_type: PortType::RealF32,
                },
                PortSpec {
                    name: "right",
                    port_type: PortType::RealF32,
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
                        min: 106_000.0,
                        max: 2_400_000.0,
                        step: 1.0,
                        default: 240_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Input rate — needs ≥106 kHz to keep the 38 kHz subcarrier above Nyquist (typically 240 kHz from the channelizer).",
                },
                ParamSpec {
                    key: "lock_threshold",
                    label: "Pilot lock threshold",
                    kind: ParamKind::Range {
                        min: 0.001,
                        max: 0.5,
                        step: 0.001,
                        default: 0.02,
                        unit: "rms",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "RMS amplitude required on the 19 kHz pilot to declare stereo lock. Lower = stickier lock on weak stations; higher = drop to mono faster on noisy signals.",
                },
                ParamSpec {
                    key: "lock_tau_ms",
                    label: "Pilot smoother τ",
                    kind: ParamKind::Range {
                        min: 10.0,
                        max: 1_000.0,
                        step: 1.0,
                        default: 100.0,
                        unit: "ms",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "Pilot envelope smoothing time constant. Shorter = faster lock/unlock; longer = stabler over fades.",
                },
                ParamSpec {
                    key: "emit_interval_ms",
                    label: "Emit interval",
                    kind: ParamKind::Range {
                        min: 50.0,
                        max: 2_000.0,
                        step: 1.0,
                        default: 100.0,
                        unit: "ms",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "How often the events port publishes a stereo-status frame (lock state, pilot level). Decouples UI refresh rate from audio rate.",
                },
            ],
            ai_notes: "Stereo FM decoder. Takes the FM-demod composite (mono + 19 kHz pilot + 38 kHz subcarrier) and outputs L/R audio plus a lock-status events stream. Falls back to mono when the pilot is below `lock_threshold`. Use after `FmDemod` for `wbfm_stereo`.",
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
            .and_then(InputPort::as_real_f32)
        else {
            return Ok(Work::new());
        };

        let n = src.len();

        // Take temporary Vec output buffers — we resolve actual output
        // ports at the end. Keeps the math readable at the cost of one
        // Vec allocation per `process()` call. Not hot enough to matter.
        let mut out_l = vec![0.0_f32; n];
        let mut out_r = vec![0.0_f32; n];

        for i in 0..n {
            let mpx = src[i];

            // Pilot path.
            let pilot = self.pilot_bpf.step(mpx);
            self.pilot_power += self.alpha_lock * (pilot * pilot - self.pilot_power);

            // Lock/unlock hysteresis on smoothed power.
            if self.pilot_locked {
                if self.pilot_power < self.unlock_threshold_power {
                    self.pilot_locked = false;
                }
            } else if self.pilot_power > self.lock_threshold_power {
                self.pilot_locked = true;
            }

            // Smooth blend toward the current target: 0 below the
            // unlock threshold, 1 above the lock threshold, linearly
            // ramped in between. Uses the same α as `pilot_power` so
            // fades and the lock bit track the same time constant.
            let blend_target = if self.pilot_power <= self.unlock_threshold_power {
                0.0
            } else if self.pilot_power >= self.lock_threshold_power {
                1.0
            } else {
                (self.pilot_power - self.unlock_threshold_power)
                    / (self.lock_threshold_power - self.unlock_threshold_power)
            };
            self.blend += self.alpha_lock * (blend_target - self.blend);

            // PLL step: mix the filtered pilot against the NCO's
            // current complex exponential. When locked, (I, Q) sits at
            // (pilot_amplitude, 0); Q is the small-angle phase error
            // that pulls the loop toward zero.
            let (c, s) = self.pilot_nco.cexpf();
            // pilot (real) · conj(c + j s) = pilot · (c − j s)
            let mix_q = -pilot * s;
            self.pilot_nco.pll_step(mix_q);
            self.pilot_nco.step();

            // 38 kHz reference = −sin(2·θ) = −2·sin(θ)·cos(θ). The PLL
            // (mix_q = −pilot·s) nulls at cos(θ−ψ)=0, so at lock it holds
            // the NCO in quadrature to the pilot phase ψ (θ = ψ ∓ π/2).
            // On BOTH branches −sin(2θ) = +sin(2ψ) — in phase with the
            // broadcast DSB-SC subcarrier (FCC/ITU: pilot sin(ψ),
            // subcarrier sin(2ψ)). The old cos(2θ) landed 90° off; its
            // coherent product with sin(2ψ) had zero DC, so L−R nulled
            // and real stations decoded as mono with pilot_lock=true.
            // (c, s) are read pre-step (line 440) — the correct instant.
            let ref_38k = -2.0 * c * s;

            // Delay raw MPX to line up with the pilot BPF output (and
            // hence with the PLL reference). Same-length LPFs downstream
            // keep L+R and L−R paths co-timed into the matrix.
            let mpx_delayed = if self.mpx_delay_len == 0 {
                mpx
            } else {
                let out = self.mpx_delay[self.mpx_delay_write];
                self.mpx_delay[self.mpx_delay_write] = mpx;
                self.mpx_delay_write = (self.mpx_delay_write + 1) % self.mpx_delay_len;
                out
            };

            // L+R = LPF(delayed MPX).
            let l_plus_r = self.lpf_sum.step(mpx_delayed);

            // L−R = LPF(delayed MPX · ref_38k) · 2. Factor 2 recovers
            // the original amplitude from the coherent demod. Scaled
            // by the smoothed blend factor so a weak/absent pilot
            // fades L−R to zero and both outputs converge on L+R — a
            // clean mono signal, no doubled hiss from the 23–53 kHz
            // L−R noise window.
            let l_minus_r = self.lpf_diff.step(mpx_delayed * ref_38k) * 2.0 * self.blend;

            out_l[i] = 0.5 * (l_plus_r + l_minus_r);
            out_r[i] = 0.5 * (l_plus_r - l_minus_r);

            self.samples_until_emit = self.samples_until_emit.saturating_sub(1);
            if self.samples_until_emit == 0 {
                self.emit_lock_event();
                self.samples_until_emit = self.emit_stride;
            }
        }

        let mut produced_l = 0;
        let mut produced_r = 0;
        let mut produced_events = 0;

        for port in io.outputs.iter_mut() {
            match port.name {
                "left" => {
                    if let Some(dst) = OutputPort::as_real_f32_mut(port) {
                        let k = dst.len().min(out_l.len());
                        dst[..k].copy_from_slice(&out_l[..k]);
                        produced_l = k;
                    }
                }
                "right" => {
                    if let Some(dst) = OutputPort::as_real_f32_mut(port) {
                        let k = dst.len().min(out_r.len());
                        dst[..k].copy_from_slice(&out_r[..k]);
                        produced_r = k;
                    }
                }
                "events" => {
                    if let OutBuf::Events(dst) = &mut port.buf {
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
        w.produced[0] = produced_l;
        w.produced[1] = produced_r;
        w.produced[2] = produced_events;
        Ok(w)
    }
}

impl BlockFactory for StereoDecoder {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: StereoDecoderParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(StereoDecoder::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{StereoDecoder, StereoDecoderParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;

    struct DecodedChunk {
        left: Vec<f32>,
        right: Vec<f32>,
        events: Vec<u8>,
    }

    fn run(dec: &mut StereoDecoder, input: &[f32]) -> DecodedChunk {
        let mut out_l = vec![0.0_f32; input.len()];
        let mut out_r = vec![0.0_f32; input.len()];
        let mut out_ev = vec![0u8; 4096];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(input),
        }];
        let mut outputs = [
            OutputPort {
                name: "left",
                meta: PortMeta::default(),
                buf: OutBuf::RealF32(&mut out_l),
            },
            OutputPort {
                name: "right",
                meta: PortMeta::default(),
                buf: OutBuf::RealF32(&mut out_r),
            },
            OutputPort {
                name: "events",
                meta: PortMeta::default(),
                buf: OutBuf::Events(&mut out_ev),
            },
        ];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = dec.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], input.len());
        let ev_len = w.produced[2];
        DecodedChunk {
            left: out_l,
            right: out_r,
            events: out_ev[..ev_len].to_vec(),
        }
    }

    /// Synthesise a valid broadcast MPX: `(L+R) + (L−R)·sin(2π·38k·t) +
    /// pilot·sin(2π·19k·t)`. Pilot and subcarrier are `sin` per the
    /// FCC/ITU convention (the 38 kHz subcarrier is the in-phase 2nd
    /// harmonic of the 19 kHz pilot); the decoder's `−sin(2θ)` reference
    /// locks in phase with this. The factor-of-2 the decoder applies
    /// post-LPF compensates for the ½ the coherent mixer introduces.
    fn make_mpx(fs: f32, n: usize, left_hz: f32, right_hz: f32, pilot_amp: f32) -> Vec<f32> {
        make_mpx_pilot(fs, n, left_hz, right_hz, pilot_amp, 1.0)
    }

    /// Like [`make_mpx`] but with an explicit pilot sign. Negating the
    /// pilot flips its phase by π, so the PLL locks at the mirrored ±π/2
    /// branch — tests use it to prove the `−sin(2θ)` reference lands in
    /// phase with the (unchanged) subcarrier on either branch.
    fn make_mpx_pilot(
        fs: f32,
        n: usize,
        left_hz: f32,
        right_hz: f32,
        pilot_amp: f32,
        pilot_sign: f32,
    ) -> Vec<f32> {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / fs;
            let l = (TAU * left_hz * t).cos();
            let r = (TAU * right_hz * t).cos();
            let sum = l + r;
            let diff = l - r;
            let carrier = (TAU * 38_000.0 * t).sin();
            let pilot = pilot_sign * pilot_amp * (TAU * 19_000.0 * t).sin();
            out.push(sum + diff * carrier + pilot);
        }
        out
    }

    fn rms(x: &[f32]) -> f32 {
        let sum: f32 = x.iter().map(|v| v * v).sum();
        (sum / x.len() as f32).sqrt()
    }

    /// Single-bin DFT magnitude at `target_hz` (Goertzel).
    fn goertzel(samples: &[f32], target_hz: f32, fs: f32) -> f32 {
        let len = samples.len() as f32;
        let k = (len * target_hz / fs).round();
        let omega = TAU * k / len;
        let coeff = 2.0 * omega.cos();
        let (mut q1, mut q2) = (0.0_f32, 0.0_f32);
        for &s in samples {
            let q0 = coeff * q1 - q2 + s;
            q2 = q1;
            q1 = q0;
        }
        (q1 * q1 + q2 * q2 - coeff * q1 * q2).max(0.0).sqrt()
    }

    #[test]
    fn subcarrier_convention_pinned_separation_and_polarity() {
        // Convention pin, independent of the fixture's exact phases:
        // decode a real-standard MPX (sin pilot / sin subcarrier) with
        // L = 1 kHz and R = 2 kHz, then measure per-tone energy in each
        // output with Goertzel. Assert (a) > 30 dB channel separation
        // and (b) the channels are NOT swapped (1 kHz dominates L, 2 kHz
        // dominates R). Repeat with the pilot negated so the PLL locks at
        // the mirrored ±π/2 branch — both must still hold, proving the
        // −sin(2θ) reference is in phase with the subcarrier on either
        // branch. Pre-fix (cos(2θ) reference) L−R nulls: both tones leak
        // equally into both channels and separation collapses to ~0 dB.
        let fs = 240_000.0_f32;
        let n = 65_536;
        let skip = 16_384; // past the lock + filter transient
        for pilot_sign in [1.0_f32, -1.0] {
            let mpx = make_mpx_pilot(fs, n, 1_000.0, 2_000.0, 0.1, pilot_sign);
            let mut dec = StereoDecoder::new(StereoDecoderParams {
                sample_rate_hz: fs,
                lock_tau_ms: 10.0,
                ..Default::default()
            })
            .unwrap();
            let out = run(&mut dec, &mpx);
            assert!(
                dec.pilot_locked(),
                "pilot_sign={pilot_sign}: expected pilot lock"
            );
            let l = &out.left[skip..];
            let r = &out.right[skip..];
            let l_1k = goertzel(l, 1_000.0, fs);
            let l_2k = goertzel(l, 2_000.0, fs);
            let r_1k = goertzel(r, 1_000.0, fs);
            let r_2k = goertzel(r, 2_000.0, fs);
            // (b) not swapped.
            assert!(
                l_1k > l_2k,
                "pilot_sign={pilot_sign}: L must carry the 1 kHz tone (1k={l_1k} 2k={l_2k})"
            );
            assert!(
                r_2k > r_1k,
                "pilot_sign={pilot_sign}: R must carry the 2 kHz tone (2k={r_2k} 1k={r_1k})"
            );
            // (a) separation: the foreign tone is ≥ 30 dB down.
            let sep_l_db = 20.0 * (l_1k / l_2k).log10();
            let sep_r_db = 20.0 * (r_2k / r_1k).log10();
            assert!(
                sep_l_db > 30.0,
                "pilot_sign={pilot_sign}: L separation only {sep_l_db:.1} dB"
            );
            assert!(
                sep_r_db > 30.0,
                "pilot_sign={pilot_sign}: R separation only {sep_r_db:.1} dB"
            );
        }
    }

    #[test]
    fn constructor_rejects_bad_params() {
        assert!(StereoDecoder::new(StereoDecoderParams {
            sample_rate_hz: 48_000.0,
            ..Default::default()
        })
        .is_err());
        assert!(StereoDecoder::new(StereoDecoderParams {
            lock_threshold: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(StereoDecoder::new(StereoDecoderParams {
            lock_tau_ms: 0.0,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn mono_mpx_produces_equal_left_and_right() {
        // L = R means the L−R component is zero, so L and R should be
        // identical (== L+R = 2·L, up to filter gain). Use a single
        // tone on both sides.
        let fs = 240_000.0_f32;
        let n = 16384;
        let mpx = make_mpx(fs, n, 1_000.0, 1_000.0, 0.1);
        let mut dec = StereoDecoder::new(StereoDecoderParams {
            sample_rate_hz: fs,
            ..Default::default()
        })
        .unwrap();
        let out = run(&mut dec, &mpx);
        // Skip the filter transient.
        let tail_l = &out.left[4096..];
        let tail_r = &out.right[4096..];
        let rms_l = rms(tail_l);
        let rms_r = rms(tail_r);
        let rms_diff = rms(&tail_l
            .iter()
            .zip(tail_r.iter())
            .map(|(a, b)| a - b)
            .collect::<Vec<_>>());
        assert!(
            rms_l > 0.3 && rms_r > 0.3,
            "mono tones should come through: L={rms_l} R={rms_r}"
        );
        assert!(
            rms_diff < 0.05,
            "mono MPX must give L == R, RMS diff={rms_diff}"
        );
    }

    #[test]
    fn stereo_mpx_separates_left_from_right() {
        // Distinct tones in L (800 Hz) and R (2000 Hz). After decode L
        // should have high energy at 800 Hz and low at 2000 Hz, and
        // vice versa for R. Easiest check: correlate each output with
        // its own source. Use a short `lock_tau_ms` so the stereo-
        // blend ramp (driven by the same smoother as pilot lock) is
        // fully up within the test window rather than still climbing.
        let fs = 240_000.0_f32;
        let n = 32768;
        let mpx = make_mpx(fs, n, 800.0, 2_000.0, 0.1);
        let mut dec = StereoDecoder::new(StereoDecoderParams {
            sample_rate_hz: fs,
            lock_tau_ms: 10.0,
            ..Default::default()
        })
        .unwrap();
        let out = run(&mut dec, &mpx);
        // Reference tones.
        let ref_l: Vec<f32> = (0..n)
            .map(|i| (TAU * 800.0 * i as f32 / fs).cos())
            .collect();
        let ref_r: Vec<f32> = (0..n)
            .map(|i| (TAU * 2_000.0 * i as f32 / fs).cos())
            .collect();
        let skip = 8192;
        let dot = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
        let l_dot_refl = dot(&out.left[skip..], &ref_l[skip..]).abs();
        let l_dot_refr = dot(&out.left[skip..], &ref_r[skip..]).abs();
        let r_dot_refr = dot(&out.right[skip..], &ref_r[skip..]).abs();
        let r_dot_refl = dot(&out.right[skip..], &ref_l[skip..]).abs();
        // L should correlate strongly with its own tone, weakly with
        // the right-channel tone — the ratio captures channel
        // separation. Anything ≥ 5× is audibly "stereo".
        assert!(
            l_dot_refl > 5.0 * l_dot_refr,
            "L channel should carry L tone, got L·L={l_dot_refl} L·R={l_dot_refr}"
        );
        assert!(
            r_dot_refr > 5.0 * r_dot_refl,
            "R channel should carry R tone, got R·R={r_dot_refr} R·L={r_dot_refl}"
        );
    }

    #[test]
    fn weak_pilot_blends_outputs_toward_mono() {
        // Same L/R tones as `stereo_mpx_separates_left_from_right`
        // but with the pilot below the unlock threshold — the L−R
        // path should fade out and L should end up equal to R.
        let fs = 240_000.0_f32;
        let n = 48_000;
        // Pilot amp 0.0005 → power ≈ 1.25e-7, far below the default
        // 4e-4 lock-threshold² = 1.6e-7… still close, use 0.0001 to
        // guarantee unlock.
        let mpx = make_mpx(fs, n, 800.0, 2_000.0, 0.0001);
        let mut dec = StereoDecoder::new(StereoDecoderParams {
            sample_rate_hz: fs,
            lock_tau_ms: 10.0,
            ..Default::default()
        })
        .unwrap();
        let out = run(&mut dec, &mpx);
        // Tail of the output — blend should be fully collapsed to 0.
        let tail_l = &out.left[n / 2..];
        let tail_r = &out.right[n / 2..];
        let diff: f32 = tail_l
            .iter()
            .zip(tail_r.iter())
            .map(|(a, b)| (a - b).abs())
            .sum::<f32>()
            / tail_l.len() as f32;
        assert!(
            diff < 0.01,
            "mono fallback: L and R should match when pilot is absent, got avg |L−R| = {diff}"
        );
    }

    #[test]
    fn pilot_lock_bit_tracks_pilot_amplitude() {
        let fs = 240_000.0_f32;
        let n = 48_000; // 200 ms — plenty for the ~100 ms lock τ to settle.

        // With pilot amplitude well above threshold the lock bit goes true.
        let mpx_strong = make_mpx(fs, n, 1_000.0, 1_000.0, 0.2);
        let mut dec = StereoDecoder::new(StereoDecoderParams {
            sample_rate_hz: fs,
            lock_threshold: 0.02,
            lock_tau_ms: 30.0,
            ..Default::default()
        })
        .unwrap();
        run(&mut dec, &mpx_strong);
        assert!(dec.pilot_locked(), "expected pilot lock on strong pilot");

        // With pilot nearly absent we expect unlock after the smoother
        // drains. Two passes of 200 ms each is > 3τ at τ=30 ms.
        let mpx_weak = make_mpx(fs, n, 1_000.0, 1_000.0, 0.0005);
        run(&mut dec, &mpx_weak);
        run(&mut dec, &mpx_weak);
        assert!(!dec.pilot_locked(), "expected pilot unlock on absent pilot");
    }

    #[test]
    fn emits_pilot_events_periodically() {
        let fs = 240_000.0_f32;
        let n = 48_000; // 200 ms at 240 kHz.
        let mpx = make_mpx(fs, n, 1_000.0, 1_000.0, 0.1);
        let mut dec = StereoDecoder::new(StereoDecoderParams {
            sample_rate_hz: fs,
            emit_interval_ms: 50.0, // 4 events in 200 ms
            ..Default::default()
        })
        .unwrap();
        let out = run(&mut dec, &mpx);
        let text = std::str::from_utf8(&out.events).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert!(
            (3..=5).contains(&lines.len()),
            "expected ≈ 4 events, got {}: {text}",
            lines.len()
        );
        // Last event should mention pilot_lock=true.
        assert!(
            lines.last().unwrap().contains("\"pilot_lock\":true"),
            "expected last event to report lock: {text}"
        );
    }
}
