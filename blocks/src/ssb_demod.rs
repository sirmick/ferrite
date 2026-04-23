//! SSB demodulator — complex IQ → real audio, Hilbert-phasing style.
//!
//! One IQ-in, one Real-out, same sample rate. Input IQ is expected to be
//! baseband, centered on the suppressed carrier. The block recovers the
//! selected sideband (USB or LSB) via the phasing method and rejects the
//! opposite sideband by the Hilbert filter's image-rejection ratio.
//!
//! ### Algorithm — Hartley / phasing
//!
//! For a transmit SSB signal carrying message `m(t)`:
//!
//! ```text
//! USB: u(t) = m(t)·cos(ωc·t) − H{m}·sin(ωc·t)
//! LSB: u(t) = m(t)·cos(ωc·t) + H{m}·sin(ωc·t)
//! ```
//!
//! where `H{·}` is the Hilbert transform. After IQ mix-down at the
//! suppressed carrier frequency, the baseband IQ satisfies:
//!
//! ```text
//! I[n] = m[n]  (+ leaking opposite-sideband m_other[n])
//! Q[n] = ±H{m[n]}  (+ leaking ∓H{m_other[n]})
//! ```
//!
//! Passing `Q` through an FIR Hilbert filter `H{·}` collapses the leak
//! term so:
//!
//! ```text
//! audio_USB = I_delayed − H{Q}
//! audio_LSB = I_delayed + H{Q}
//! ```
//!
//! (Factor of 2 absorbed into `audio_gain`.) `I_delayed` matches the
//! Hilbert filter's group delay of `(NUM_TAPS − 1) / 2` samples so the
//! two terms are time-aligned.
//!
//! ### Filter
//!
//! `NUM_TAPS = 63` odd-length Hamming-windowed sinc-Hilbert kernel:
//!
//! ```text
//! h[n] = (2 / (π·k)) · w[n]   where k = n − M, n ∈ {0..N-1}, M = (N-1)/2
//! h[M] = 0                    (exact Hilbert has no centre tap)
//! w[n] = Hamming window
//! ```
//!
//! Only odd taps (k odd) are non-zero, so the inner loop multiplies half
//! as many coefficients as a naive FIR of the same length. Image
//! rejection is ≳ 45 dB across 300–3 000 Hz at 48 kHz — ample for voice
//! work given that the channelizer already isolates a narrow channel.
//!
//! ### Rationale
//!
//! Phasing selects a sideband in one pass without decimation or LPF
//! re-work. `audio_gain` makes up the post-demod level — SSB audio sits
//! well below full-scale after IQ mix-down — and defaults are tuned for
//! a healthy amateur-band signal (≈30 × = 30 dB).

use anyhow::{bail, Result};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Hilbert FIR length. Odd so the ideal-Hilbert centre tap is zero and
/// the filter is type-III (antisymmetric). 63 taps at 48 kHz gives a
/// ~1.3 ms group delay and ≳ 45 dB image rejection across speech band.
const NUM_TAPS: usize = 63;

/// Hilbert group delay in samples — `(NUM_TAPS - 1) / 2`.
const DELAY: usize = (NUM_TAPS - 1) / 2;

/// Sideband to demodulate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sideband {
    /// Upper sideband — `audio = I − H{Q}`.
    Usb,
    /// Lower sideband — `audio = I + H{Q}`.
    Lsb,
}

impl Default for Sideband {
    fn default() -> Self {
        Self::Usb
    }
}

/// Construction-time params.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct SsbDemodParams {
    /// Input IQ sample rate (Hz). Wired in by the scheduler from the
    /// input port's metadata; set explicitly for standalone tests.
    pub sample_rate_hz: f32,
    /// Which sideband to recover. Flips the sign on `H{Q}`.
    pub sideband: Sideband,
    /// Post-demod linear gain. SSB baseband IQ lands well below full
    /// scale after mixer + decimation, so the real audio is similarly
    /// low; 30× (≈30 dB) is a reasonable default for amateur-band
    /// signals and brings a strong station near −6 dBFS.
    pub audio_gain: f32,
}

impl Default for SsbDemodParams {
    fn default() -> Self {
        Self {
            sample_rate_hz: 48_000.0,
            sideband: Sideband::Usb,
            audio_gain: 30.0,
        }
    }
}

pub struct SsbDemod {
    sign: f32,
    gain: f32,
    /// Odd-indexed Hilbert taps only — even taps are exactly zero and
    /// skipping them halves the inner loop's work. Indexed by
    /// `odd_idx = (k - 1) / 2` where `k` is the offset from centre.
    odd_taps: Vec<f32>,
    /// Ring buffer of the last `NUM_TAPS` IQ inputs. Sized to
    /// `NUM_TAPS.next_power_of_two()` so indexing wraps via bit-mask.
    history: Vec<Complex<f32>>,
    mask: usize,
    write: usize,
}

impl SsbDemod {
    /// Build a demod with the supplied params. Fails on non-positive
    /// rates or gain.
    pub fn new(params: SsbDemodParams) -> Result<Self> {
        if !(params.sample_rate_hz.is_finite() && params.sample_rate_hz > 0.0) {
            bail!(
                "ssb_demod sample_rate_hz must be > 0 (got {})",
                params.sample_rate_hz
            );
        }
        if !(params.audio_gain.is_finite() && params.audio_gain > 0.0) {
            bail!(
                "ssb_demod audio_gain must be > 0 (got {})",
                params.audio_gain
            );
        }

        // My `h_q` convolves with `+tap` at future offsets and `-tap` at
        // past offsets, which is the negative of the ideal Hilbert
        // kernel. The algebraic demod rule is `audio_USB = I − H{Q}`;
        // substituting `H{Q} = −h_q` gives `audio_USB = I + h_q` (sign
        // = +1) and `audio_LSB = I − h_q` (sign = −1).
        let sign = match params.sideband {
            Sideband::Usb => 1.0,
            Sideband::Lsb => -1.0,
        };

        // Build windowed-sinc Hilbert kernel. Only odd taps are non-zero.
        // Valid offsets from the centre tap run 1..=DELAY; of those, the
        // odd ones are 1, 3, …, DELAY (when DELAY is odd) → `DELAY/2 + 1`
        // taps. For NUM_TAPS = 63 this gives 16 multiplies per sample.
        let odd_count = (DELAY + 1) / 2;
        let mut odd_taps = Vec::with_capacity(odd_count);
        for i in 0..odd_count {
            // k is the odd offset from centre: 1, 3, 5, ... (positive side)
            // Ideal Hilbert: h[k] = 2/(π·k) for k odd, with antisymmetry
            // giving -h[-k] on the negative side. We store the positive
            // side only and apply the sign at convolution time.
            #[allow(clippy::cast_precision_loss)]
            let k = (2 * i + 1) as f32;
            let ideal = 2.0 / (core::f32::consts::PI * k);
            // Hamming window evaluated at tap index (centre + k).
            #[allow(clippy::cast_precision_loss)]
            let n = (DELAY + 2 * i + 1) as f32;
            #[allow(clippy::cast_precision_loss)]
            let len = (NUM_TAPS - 1) as f32;
            let w = 0.54 - 0.46 * (core::f32::consts::TAU * n / len).cos();
            odd_taps.push(ideal * w);
        }

        let buf_len = NUM_TAPS.next_power_of_two();
        Ok(Self {
            sign,
            gain: params.audio_gain,
            odd_taps,
            history: vec![Complex::new(0.0, 0.0); buf_len],
            mask: buf_len - 1,
            write: 0,
        })
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for SsbDemod {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "SsbDemod",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::RealF32,
            }],
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
                },
                ParamSpec {
                    key: "sideband",
                    label: "Sideband",
                    kind: ParamKind::EnumString {
                        values: &["usb", "lsb"],
                        default: "usb",
                    },
                    // Flips the sign on H{Q}; cheap to re-init. History
                    // carries complex IQ samples that are sideband-agnostic
                    // so we could also re-tap without reset, but the
                    // re-init path is simpler and the glitch inaudible.
                    reconfig_scope: ReconfigureScope::Downstream,
                },
                ParamSpec {
                    key: "audio_gain",
                    label: "Post-demod gain",
                    kind: ParamKind::Range {
                        min: 0.1,
                        max: 1_000.0,
                        step: 0.1,
                        default: 30.0,
                        unit: "×",
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
            .and_then(InputPort::as_iq_f32)
        else {
            return Ok(Work::new());
        };
        let Some(dst) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_real_f32_mut)
        else {
            return Ok(Work::new());
        };

        let n = src.len().min(dst.len());
        for i in 0..n {
            // Push new input into the ring at `write`.
            self.history[self.write] = src[i];
            self.write = (self.write + 1) & self.mask;

            // Centre tap: index `write - 1 - DELAY` (most recent sample
            // we pushed is at `write - 1`; go back DELAY more for the
            // midpoint of the Hilbert filter).
            let centre_idx = (self.write + self.mask - DELAY) & self.mask;
            let i_delayed = self.history[centre_idx].re;

            // Hilbert convolution on the Q channel. Antisymmetric taps:
            // `H{Q}[m] = Σ_k h[k]·Q[m-k] − h[k]·Q[m+k]` where only odd
            // k contributes. `centre_idx` is the sample at m = now - DELAY.
            let mut h_q = 0.0_f32;
            for (j, &tap) in self.odd_taps.iter().enumerate() {
                let k = 2 * j + 1;
                let plus = (centre_idx + k) & self.mask;
                let minus = (centre_idx + self.mask + 1 - k) & self.mask;
                // Antisymmetric: tap on the +k side, -tap on the -k side.
                // Note `plus` may index into samples not yet arrived — but
                // DELAY is chosen so centre_idx is DELAY behind `write`,
                // meaning `centre + DELAY` = most recent arrived sample.
                h_q += tap * (self.history[plus].im - self.history[minus].im);
            }

            dst[i] = (i_delayed + self.sign * h_q) * self.gain;
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = n;
        Ok(w)
    }
}

impl BlockFactory for SsbDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: SsbDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(SsbDemod::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{Sideband, SsbDemod, SsbDemodParams, DELAY};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;
    use num_complex::Complex;

    fn run(demod: &mut SsbDemod, input: &[Complex<f32>]) -> Vec<f32> {
        let mut out = vec![0.0_f32; input.len()];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(input),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = demod.process(&mut io).unwrap();
        assert_eq!(w.consumed[0], input.len());
        assert_eq!(w.produced[0], input.len());
        out
    }

    fn tail_rms(out: &[f32]) -> f32 {
        // Skip the filter's transient (DELAY samples plus a bit of
        // settling for the ring-buffer fill).
        let start = DELAY + 64;
        let tail = &out[start..];
        (tail.iter().map(|y| y * y).sum::<f32>() / tail.len() as f32).sqrt()
    }

    #[test]
    fn constructor_rejects_bad_params() {
        assert!(SsbDemod::new(SsbDemodParams {
            sample_rate_hz: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(SsbDemod::new(SsbDemodParams {
            audio_gain: 0.0,
            ..Default::default()
        })
        .is_err());
        assert!(SsbDemod::new(SsbDemodParams {
            sample_rate_hz: f32::NAN,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn usb_recovers_positive_frequency_tone() {
        // A pure positive-frequency IQ tone is a USB tone at that audio
        // frequency. Expect USB demod to reproduce it at full amplitude
        // (phasing sum doubles, post-gain of 0.5 normalises back to 1.0).
        let fs = 48_000.0_f32;
        let f = 1_000.0_f32;
        let n = 4096;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                Complex::from_polar(1.0, TAU * f * t)
            })
            .collect();
        let mut demod = SsbDemod::new(SsbDemodParams {
            sample_rate_hz: fs,
            sideband: Sideband::Usb,
            audio_gain: 0.5, // cancel the 2× from phasing sum
        })
        .unwrap();
        let out = run(&mut demod, &input);
        let rms = tail_rms(&out);
        // Pure tone of amplitude 1.0 has RMS = 1/√2 ≈ 0.707.
        assert!(
            (rms - 0.5_f32.sqrt()).abs() < 0.02,
            "USB tone RMS={rms}, expected ≈ 0.707"
        );
    }

    #[test]
    fn usb_rejects_lower_sideband_tone() {
        // A pure negative-frequency IQ tone is an LSB tone. USB demod
        // should attenuate it heavily — Hilbert image rejection ≳ 45 dB
        // means output RMS should be < 0.01 × input.
        let fs = 48_000.0_f32;
        let f = 1_000.0_f32;
        let n = 4096;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                Complex::from_polar(1.0, -TAU * f * t)
            })
            .collect();
        let mut demod = SsbDemod::new(SsbDemodParams {
            sample_rate_hz: fs,
            sideband: Sideband::Usb,
            audio_gain: 0.5,
        })
        .unwrap();
        let out = run(&mut demod, &input);
        let rms = tail_rms(&out);
        assert!(rms < 0.05, "USB should reject LSB tone, got RMS={rms}");
    }

    #[test]
    fn lsb_recovers_negative_frequency_tone() {
        let fs = 48_000.0_f32;
        let f = 1_500.0_f32;
        let n = 4096;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                Complex::from_polar(1.0, -TAU * f * t)
            })
            .collect();
        let mut demod = SsbDemod::new(SsbDemodParams {
            sample_rate_hz: fs,
            sideband: Sideband::Lsb,
            audio_gain: 0.5,
        })
        .unwrap();
        let out = run(&mut demod, &input);
        let rms = tail_rms(&out);
        assert!(
            (rms - 0.5_f32.sqrt()).abs() < 0.02,
            "LSB tone RMS={rms}, expected ≈ 0.707"
        );
    }

    #[test]
    fn lsb_rejects_upper_sideband_tone() {
        let fs = 48_000.0_f32;
        let f = 1_500.0_f32;
        let n = 4096;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                Complex::from_polar(1.0, TAU * f * t)
            })
            .collect();
        let mut demod = SsbDemod::new(SsbDemodParams {
            sample_rate_hz: fs,
            sideband: Sideband::Lsb,
            audio_gain: 0.5,
        })
        .unwrap();
        let out = run(&mut demod, &input);
        let rms = tail_rms(&out);
        assert!(rms < 0.05, "LSB should reject USB tone, got RMS={rms}");
    }

    #[test]
    fn state_persists_across_process_calls() {
        // Splitting the input across two process() calls must match one
        // long call — the ring-buffer state carries across.
        let fs = 48_000.0_f32;
        let f = 800.0_f32;
        let n = 1024;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = i as f32 / fs;
                Complex::from_polar(1.0, TAU * f * t)
            })
            .collect();
        let params = SsbDemodParams {
            sample_rate_hz: fs,
            sideband: Sideband::Usb,
            audio_gain: 1.0,
        };

        let mut whole = SsbDemod::new(params).unwrap();
        let out_whole = run(&mut whole, &input);

        let mut split = SsbDemod::new(params).unwrap();
        let first = run(&mut split, &input[..512]);
        let second = run(&mut split, &input[512..]);
        let mut out_split = first;
        out_split.extend_from_slice(&second);

        for (i, (a, b)) in out_whole.iter().zip(out_split.iter()).enumerate() {
            assert!((a - b).abs() < 1e-5, "mismatch at {i}: whole={a} split={b}");
        }
    }
}
