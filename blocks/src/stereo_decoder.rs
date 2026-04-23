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
//! ### Algorithm — pilot-squaring, no PLL
//!
//! A PLL would track the pilot more robustly under noise, but a simple
//! "square and band-pass" scheme gives a clean 38 kHz reference in
//! exchange for ~200 taps of FIR and is entirely stateless beyond its
//! delay lines.
//!
//! ```text
//! pilot     = BPF_19k(MPX)
//! ref_38k   = pilot · pilot          // DC + 38 kHz (pilot²)
//! ref_38k   = BPF_38k(ref_38k)       // keep only 38 kHz component
//! ref_38k  /= peak_ref_38k           // normalise to ±1
//! lmr_raw   = MPX · ref_38k          // mix L−R down to baseband
//! l_minus_r = LPF_15k(lmr_raw) · 2   // recover L−R (factor 2 from the mix)
//! l_plus_r  = LPF_15k(MPX)           // recover L+R
//! L         = (l_plus_r + l_minus_r) / 2
//! R         = (l_plus_r − l_minus_r) / 2
//! ```
//!
//! ### Pilot lock
//!
//! Emits a `{"pilot_lock": bool, "pilot_rms": f32}` event every
//! `emit_interval_ms` — the UI latches the lock bit to show a stereo
//! indicator and can render the pilot RMS as a signal-quality hint.
//! The lock bit is a simple threshold on the smoothed pilot RMS with
//! hysteresis so transient drops don't chatter.

use anyhow::{bail, Result};
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

// Filter lengths. Chosen so the narrowest transition (19 kHz pilot BPF
// with ≈ 1 kHz transition region) gets adequate stopband rejection, and
// all three filters share the same group delay → no per-path timing fix-
// ups.
const NUM_TAPS_PILOT: usize = 161;
const NUM_TAPS_REF: usize = 161;
const NUM_TAPS_AUDIO: usize = 161;

pub struct StereoDecoder {
    pilot_bpf: BandPass,
    ref_bpf: BandPass,
    lpf_sum: LowPass,
    lpf_diff: LowPass,

    /// Delay line for MPX so the `lpf_sum` tap aligns with the `lpf_diff`
    /// path (which goes through an extra multiply and another FIR).
    mpx_delay: Vec<f32>,
    mpx_delay_write: usize,
    mpx_delay_len: usize,

    /// Running peak tracker on `ref_38k` so the reference is normalised
    /// to ≈ ±1 regardless of the pilot amplitude at the current RF
    /// level. Uses a decay-hold peak detector.
    ref_peak: f32,

    /// Smoothed pilot RMS² for lock detection.
    pilot_power: f32,
    alpha_lock: f32,
    lock_threshold_power: f32,
    unlock_threshold_power: f32,
    pilot_locked: bool,

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
        let ref_bpf = BandPass::new(fs, 37_000.0, 39_000.0, NUM_TAPS_REF);
        let lpf_sum = LowPass::new(fs, 15_000.0, NUM_TAPS_AUDIO);
        let lpf_diff = LowPass::new(fs, 15_000.0, NUM_TAPS_AUDIO);

        // Align raw MPX with the 38 kHz reference: ref_38k at sample
        // index i is a function of MPX[i − pilot_bpf.delay − ref_bpf.delay],
        // so the multiply `mpx_delayed · ref_38k` must use MPX from
        // exactly the same lag. The same `mpx_delayed` also feeds
        // lpf_sum, whose output ends up at the same time as lpf_diff's
        // (since both LPFs are identical length). Result: L+R and L−R
        // outputs are co-timed, and the matrix step sees the same
        // instant on both.
        let mpx_delay_len = pilot_bpf.group_delay() + ref_bpf.group_delay();
        let mpx_delay = vec![0.0; mpx_delay_len.max(1)];

        let alpha_lock = 1.0 - (-(1.0 / fs) / (params.lock_tau_ms * 1e-3)).exp();
        let lock_threshold_power = params.lock_threshold * params.lock_threshold;
        let unlock_threshold_power = lock_threshold_power * 0.36; // 60% amplitude → 36% power.

        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let emit_stride = (fs * params.emit_interval_ms * 1e-3).max(1.0) as u64;

        Ok(Self {
            pilot_bpf,
            ref_bpf,
            lpf_sum,
            lpf_diff,
            mpx_delay,
            mpx_delay_write: 0,
            mpx_delay_len,
            // Start at 0; the first sample that makes it through the two
            // BPFs instantly pulls the peak up to its absolute value.
            // Initialising at 1.0 (the nominal "unit cosine" guess) would
            // leave us dividing by that for tens of thousands of samples
            // while the slow decay settles — pilot² at MPX × 0.1 sits
            // around 0.005, which would take ≈ 100k samples (0.4 s at
            // 240 kHz) to reach from 1.0 at the current decay rate.
            ref_peak: 0.0,
            pilot_power: 0.0,
            alpha_lock,
            lock_threshold_power,
            unlock_threshold_power,
            pilot_locked: false,
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

    /// Current tracked peak of the 38 kHz reference — useful for debugging
    /// stereo lock issues.
    #[must_use]
    pub const fn ref_peak(&self) -> f32 {
        self.ref_peak
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
                },
            ],
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

            // 38 kHz reference via squaring + BPF.
            let sq = pilot * pilot;
            let mut ref_38k = self.ref_bpf.step(sq);

            // Decay-hold peak tracker so ref_38k is normalised to ±1.
            // Fast attack (instant track upward) + slow decay; keeps us
            // stable during AGC swings without AGC-pumping audibly.
            let abs = ref_38k.abs();
            if abs > self.ref_peak {
                self.ref_peak = abs;
            } else {
                self.ref_peak *= 0.99995;
            }
            if self.ref_peak > 1e-6 {
                ref_38k /= self.ref_peak;
            }

            // Delay the raw MPX so its LPF_sum output lines up with the
            // LPF_diff output (which sits behind two extra FIRs).
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

            // L−R = LPF(delayed MPX · ref_38k) · 2. The delayed MPX is
            // the same lag as ref_38k — essential for the mixer to land
            // L−R at baseband cleanly; mixing the present MPX sample
            // with an old reference just smears the sideband.
            let l_minus_r = self.lpf_diff.step(mpx_delayed * ref_38k) * 2.0;

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

    /// Synthesise a valid broadcast MPX: `(L+R) + (L−R)·cos(2π·38k·t) +
    /// pilot·cos(2π·19k·t)`. Matches the standard BS412 composite; the
    /// factor-of-2 the decoder applies post-LPF compensates for the ½
    /// the mixer introduces.
    fn make_mpx(fs: f32, n: usize, left_hz: f32, right_hz: f32, pilot_amp: f32) -> Vec<f32> {
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f32 / fs;
            let l = (TAU * left_hz * t).cos();
            let r = (TAU * right_hz * t).cos();
            let sum = l + r;
            let diff = l - r;
            let carrier = (TAU * 38_000.0 * t).cos();
            let pilot = pilot_amp * (TAU * 19_000.0 * t).cos();
            out.push(sum + diff * carrier + pilot);
        }
        out
    }

    fn rms(x: &[f32]) -> f32 {
        let sum: f32 = x.iter().map(|v| v * v).sum();
        (sum / x.len() as f32).sqrt()
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
        // its own source.
        let fs = 240_000.0_f32;
        let n = 32768;
        let mpx = make_mpx(fs, n, 800.0, 2_000.0, 0.1);
        let mut dec = StereoDecoder::new(StereoDecoderParams {
            sample_rate_hz: fs,
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
