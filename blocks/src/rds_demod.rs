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

    /// Phase 2: Manchester bit sync + differential decoder fed from
    /// the decimated I stream.
    bit_sync: BitSync,
    /// Phase 3: 26-bit block assembler with CRC-10 / offset-word
    /// sync detection.
    block_sync: BlockSync,
    /// Phase 4: group assembler → PI / PS / PTY / TP state.
    parser: GroupParser,
    /// UTF-8 JSON events queued for drain on the `events` output port.
    /// Newline-delimited so a downstream `EventsSink` or UI shim can
    /// split them cleanly — same convention `StereoDecoder` uses.
    events_out: Vec<u8>,
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

        #[allow(clippy::cast_precision_loss)]
        let out_rate = fs / decim as f32;
        let bit_sync = BitSync::new(out_rate);

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
            bit_sync,
            block_sync: BlockSync::new(),
            parser: GroupParser::new(),
            events_out: Vec::new(),
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

            // Chain: decimated I → bit sync → block sync → group
            // parser. The parser accumulates 4 blocks per group,
            // updates PI / PS / PTY / TP state, and returns a
            // compact change set whenever something new was decoded;
            // we serialize that to newline-delimited JSON into
            // `events_out` for the `events` port drain.
            if let Some(bit) = self.bit_sync.push(out_i) {
                if let Some(block) = self.block_sync.push(bit) {
                    if let Some(update) = self.parser.push_block(block) {
                        update.write_json(&mut self.events_out);
                    }
                }
            }

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
            outputs: &[
                PortSpec {
                    name: "data",
                    port_type: PortType::RealF32,
                },
                PortSpec {
                    name: "events",
                    port_type: PortType::Events,
                },
            ],
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
        // Resolve the input up front so the outputs can be iterated
        // mutably afterwards without overlapping borrows.
        let src: Vec<f32> = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
            .map(<[f32]>::to_vec)
            .unwrap_or_default();

        // Capacity of the data port caps how many decimated soft
        // samples we can emit this tick. Capacity of the events port
        // caps how many JSON bytes we can drain. We size consumption
        // against the data cap so the scheduler re-presents excess.
        let mut data_cap = usize::MAX;
        let mut events_cap = 0_usize;
        for port in io.outputs.iter() {
            match port.name {
                "data" => {
                    if let crate::block::OutBuf::RealF32(buf) = &port.buf {
                        data_cap = buf.len();
                    }
                }
                "events" => {
                    if let crate::block::OutBuf::Events(buf) = &port.buf {
                        events_cap = buf.len();
                    }
                }
                _ => {}
            }
        }

        let mut consumed = 0;
        let mut pending_data = Vec::with_capacity(data_cap.min(src.len()));
        for &x in src.iter() {
            consumed += 1;
            if let Some((i, _q)) = self.pump(x) {
                if pending_data.len() >= data_cap {
                    // Data port full — back out the consumption and
                    // stash the sample so it re-emits on the next
                    // pump. The decimator accumulator takes the
                    // computed sample back in.
                    consumed -= 1;
                    self.decim_phase = self.decim;
                    self.accum_i += i;
                    break;
                }
                pending_data.push(i);
            }
        }

        // Write the pending data + drain events into the typed
        // output buffers.
        let mut produced_data = 0;
        let mut produced_events = 0;
        for port in io.outputs.iter_mut() {
            match port.name {
                "data" => {
                    if let Some(dst) = OutputPort::as_real_f32_mut(port) {
                        let k = dst.len().min(pending_data.len());
                        dst[..k].copy_from_slice(&pending_data[..k]);
                        produced_data = k;
                    }
                }
                "events" => {
                    if let crate::block::OutBuf::Events(dst) = &mut port.buf {
                        let take = self.events_out.len().min(dst.len());
                        if take > 0 {
                            dst[..take].copy_from_slice(&self.events_out[..take]);
                            self.events_out.drain(..take);
                            produced_events = take;
                        }
                    }
                }
                _ => {}
            }
        }

        let _ = events_cap; // retained for symmetry; drain logic
                            // reads port length live above.
        let mut w = Work::new();
        w.consumed[0] = consumed;
        w.produced[0] = produced_data;
        w.produced[1] = produced_events;
        Ok(w)
    }
}

impl BlockFactory for RdsDemod {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: RdsDemodParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(RdsDemod::new(p)?))
    }
}

// ---------------------------------------------------------------------------
// Phase 2: Manchester bit sync + differential decode
// ---------------------------------------------------------------------------
//
// RDS source bits S[n] are transmitted as T[n] = S[n] XOR T[n-1]
// (differential) then Manchester-encoded: T[n]=1 is a low→high
// transition mid-symbol, T[n]=0 is high→low. Receiver reverses:
// recover T_r[n] from the symbol, then S_r[n] = T_r[n] XOR T_r[n-1].
//
// Bit sync uses a fractional-rate two-half integrator. At the input
// rate of ~9.5 kS/s (8 × 1187.5 baud), one symbol spans ~8 samples.
// We accumulate the first half and second half of each symbol window
// separately; the sign of `second - first` is the received T bit
// (Manchester polarity). `samples_per_bit` is a float because the
// MPX-to-output decimation doesn't divide evenly into the symbol
// rate; the fractional carry over `samples_in_bit` keeps the timing
// aligned to the baud clock on average.

struct BitSync {
    first_half: f32,
    second_half: f32,
    samples_in_bit: f32,
    samples_per_bit: f32,
    half_bit: f32,
    last_raw: bool,
}

impl BitSync {
    fn new(input_rate_hz: f32) -> Self {
        let samples_per_bit = input_rate_hz / RDS_BAUD;
        Self {
            first_half: 0.0,
            second_half: 0.0,
            samples_in_bit: 0.0,
            samples_per_bit,
            half_bit: samples_per_bit / 2.0,
            last_raw: false,
        }
    }

    /// Push one soft sample; return `Some(bit)` each time a full
    /// symbol completes. `bit` is the differentially-decoded source
    /// bit (true = 1).
    fn push(&mut self, sample: f32) -> Option<bool> {
        if self.samples_in_bit < self.half_bit {
            self.first_half += sample;
        } else {
            self.second_half += sample;
        }
        self.samples_in_bit += 1.0;

        if self.samples_in_bit >= self.samples_per_bit {
            // Manchester: low→high transition means T=1. Sign of
            // `second − first` picks that out directly.
            let raw = self.second_half > self.first_half;
            let decoded = raw != self.last_raw;
            self.last_raw = raw;

            self.first_half = 0.0;
            self.second_half = 0.0;
            // Fractional carry: don't reset to zero — keep the
            // remainder so we don't drift over many symbols. This is
            // what makes an 8.08-samples-per-bit decimation stable.
            self.samples_in_bit -= self.samples_per_bit;

            Some(decoded)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Phase 3: Block sync + CRC-10
// ---------------------------------------------------------------------------
//
// RDS groups are 4 × 26-bit blocks of 16 data + 10 check bits. The
// check is CRC-10 (g(x) = x^10 + x^8 + x^7 + x^5 + x^4 + x^3 + 1 =
// 0x5B9) with an offset word XORed onto the transmitted check. The
// five offsets (A, B, C, C', D) identify which block-in-group the
// decoder is looking at.
//
// Key algebraic trick: the syndrome of a received 26-bit block is
// exactly the offset word that was added. Valid codeword c satisfies
// `c mod g(x) = 0`; transmitted `t = c XOR offset` (with offset in
// the low 10 bits) gives `t mod g(x) = offset`. So we compute the
// syndrome of every rolling 26-bit window and look up the offset.
//
// Sync hysteresis: a single 10-bit syndrome match could be random
// (1-in-205 per bit at 5 offsets). We require 2 consecutive correct
// block positions to declare sync, and 2 consecutive misses to drop
// it — matches every hobby-grade RDS decoder's behaviour.

/// RDS check-word offset words, per the ITU / IEC 62106 spec.
/// `C_PRIME` is only used in 0B/2B-variant group layouts.
mod offsets {
    pub const A: u16 = 0x0FC;
    pub const B: u16 = 0x198;
    pub const C: u16 = 0x168;
    pub const C_PRIME: u16 = 0x350;
    pub const D: u16 = 0x1B4;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockOffset {
    A,
    B,
    C,
    CPrime,
    D,
}

impl BlockOffset {
    #[cfg(test)]
    fn syndrome(self) -> u16 {
        match self {
            Self::A => offsets::A,
            Self::B => offsets::B,
            Self::C => offsets::C,
            Self::CPrime => offsets::C_PRIME,
            Self::D => offsets::D,
        }
    }

    fn from_syndrome(s: u16) -> Option<Self> {
        match s {
            offsets::A => Some(Self::A),
            offsets::B => Some(Self::B),
            offsets::C => Some(Self::C),
            offsets::C_PRIME => Some(Self::CPrime),
            offsets::D => Some(Self::D),
            _ => None,
        }
    }
}

/// Compute the 10-bit CRC-RDS syndrome of a 26-bit received word.
/// `word` must fit in the low 26 bits. Shift-register reduction over
/// the generator `g(x) = x^10 + x^8 + x^7 + x^5 + x^4 + x^3 + 1`.
fn rds_syndrome(word: u32) -> u16 {
    // Bit grouping mirrors the polynomial coefficients (x^10 + x^8 + x^7
    // + x^5 + x^4 + x^3 + 1), not the standard 4-bit grouping clippy
    // expects.
    #[allow(clippy::unusual_byte_groupings)]
    const G: u32 = 0b1_0110_1110_01; // 11-bit, bit 10 is the reducing term
    let mut reg = word & 0x3FF_FFFF;
    // Reduce from bit 25 down to bit 10.
    for i in (10..26).rev() {
        if (reg >> i) & 1 == 1 {
            reg ^= G << (i - 10);
        }
    }
    (reg & 0x3FF) as u16
}

/// A successfully-CRC-matched 26-bit RDS block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RdsBlock {
    /// Offset word identified during sync — tells us which block-in-
    /// group position this is.
    pub offset: BlockOffset,
    /// 16-bit data payload.
    pub data: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SyncState {
    /// Every bit is a candidate sync point; look for any offset
    /// syndrome match, then transition to Candidate.
    Searching,
    /// Saw one syndrome hit; wait 26 bits and verify a second block
    /// lands. If it does, promote to Locked. If not, drop back to
    /// Searching — the initial hit was a stray 1-in-205 coincidence.
    Candidate { bits_until_check: u32 },
    /// Tracking block boundaries every 26 bits; emits a block on each
    /// boundary whose syndrome parses as a valid offset word.
    Locked {
        bits_until_boundary: u32,
        consecutive_bad: u32,
    },
}

struct BlockSync {
    /// 26-bit rolling window of most recent bits.
    window: u32,
    state: SyncState,
}

impl BlockSync {
    /// Drop back to searching after this many consecutive bad CRCs.
    const UNLOCK_THRESHOLD: u32 = 2;

    fn new() -> Self {
        Self {
            window: 0,
            state: SyncState::Searching,
        }
    }

    fn push(&mut self, bit: bool) -> Option<RdsBlock> {
        self.window = ((self.window << 1) & 0x3FF_FFFF) | u32::from(bit);

        match self.state {
            SyncState::Searching => {
                let syndrome = rds_syndrome(self.window);
                if BlockOffset::from_syndrome(syndrome).is_some() {
                    // First match — don't emit yet; wait a full block
                    // period for a confirming second hit.
                    self.state = SyncState::Candidate {
                        bits_until_check: 26,
                    };
                }
                None
            }
            SyncState::Candidate { bits_until_check } => {
                let remaining = bits_until_check - 1;
                if remaining > 0 {
                    self.state = SyncState::Candidate {
                        bits_until_check: remaining,
                    };
                    return None;
                }
                let syndrome = rds_syndrome(self.window);
                if let Some(off) = BlockOffset::from_syndrome(syndrome) {
                    // Confirmation hit — lock in and emit this block.
                    self.state = SyncState::Locked {
                        bits_until_boundary: 26,
                        consecutive_bad: 0,
                    };
                    return Some(RdsBlock {
                        offset: off,
                        data: ((self.window >> 10) & 0xFFFF) as u16,
                    });
                }
                // False alarm on the first hit — back to searching.
                // (`Searching` re-evaluates from *this* bit; the
                // rolling window is the same object.)
                self.state = SyncState::Searching;
                None
            }
            SyncState::Locked {
                bits_until_boundary,
                consecutive_bad,
            } => {
                let remaining = bits_until_boundary - 1;
                if remaining > 0 {
                    self.state = SyncState::Locked {
                        bits_until_boundary: remaining,
                        consecutive_bad,
                    };
                    return None;
                }
                let syndrome = rds_syndrome(self.window);
                if let Some(off) = BlockOffset::from_syndrome(syndrome) {
                    self.state = SyncState::Locked {
                        bits_until_boundary: 26,
                        consecutive_bad: 0,
                    };
                    return Some(RdsBlock {
                        offset: off,
                        data: ((self.window >> 10) & 0xFFFF) as u16,
                    });
                }
                let bad = consecutive_bad + 1;
                if bad >= Self::UNLOCK_THRESHOLD {
                    self.state = SyncState::Searching;
                } else {
                    self.state = SyncState::Locked {
                        bits_until_boundary: 26,
                        consecutive_bad: bad,
                    };
                }
                None
            }
        }
    }

    #[cfg(test)]
    fn is_synced(&self) -> bool {
        matches!(self.state, SyncState::Locked { .. })
    }
}

// ---------------------------------------------------------------------------
// Phase 4: Group parser — accumulate 4 blocks into groups, extract
// PI / PS / PTY / TP / TA, emit newline-delimited JSON events on each
// state change.
// ---------------------------------------------------------------------------
//
// An RDS group is always 4 consecutive blocks: (A, B, C | C′, D). The
// block sync emits blocks in that order once locked. Block roles:
//
//   A: PI (Programme Identification) — 16-bit station ID, repeats
//      unchanged in every group from a given transmitter.
//   B: Group type (top 4 bits) + variant A/B (bit 11) + TP (bit 10) +
//      PTY (bits 9..5) + 5 type-specific bits.
//   C / C′: type-specific payload. C′ variant B groups repeat PI here.
//   D: type-specific payload.
//
// PS (8-char station name) is spread over 4 successive 0A or 0B
// groups: block B bits 0..1 give the segment index, block D gives two
// ASCII chars. We emit on every update — callers see the name filling
// in character pairs from left to right as groups arrive.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GroupUpdate {
    pi_changed: Option<u16>,
    pty_changed: Option<u8>,
    tp_changed: Option<bool>,
    ta_changed: Option<bool>,
    /// Snapshot of the PS buffer whenever a new character pair lands
    /// or the name completes a full sweep. `None` means no PS change
    /// in this group.
    ps_changed: Option<[u8; 8]>,
}

impl GroupUpdate {
    const fn empty() -> Self {
        Self {
            pi_changed: None,
            pty_changed: None,
            tp_changed: None,
            ta_changed: None,
            ps_changed: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.pi_changed.is_none()
            && self.pty_changed.is_none()
            && self.tp_changed.is_none()
            && self.ta_changed.is_none()
            && self.ps_changed.is_none()
    }

    /// Serialize this update as one newline-terminated JSON object
    /// appended to `out`. Only present fields are emitted, so
    /// downstream consumers can merge into the authoritative state
    /// without re-sending unchanged ones.
    fn write_json(&self, out: &mut Vec<u8>) {
        use std::io::Write;
        if self.is_empty() {
            return;
        }
        out.extend_from_slice(br#"{"t":"rds""#);
        if let Some(pi) = self.pi_changed {
            let _ = write!(out, r#","pi":{}"#, pi);
        }
        if let Some(pty) = self.pty_changed {
            let _ = write!(out, r#","pty":{}"#, pty);
        }
        if let Some(tp) = self.tp_changed {
            let _ = write!(out, r#","tp":{}"#, tp);
        }
        if let Some(ta) = self.ta_changed {
            let _ = write!(out, r#","ta":{}"#, ta);
        }
        if let Some(ps) = self.ps_changed {
            out.extend_from_slice(br#","ps":""#);
            // ASCII-only escape — replace control bytes with spaces.
            // Non-printables land in malformed PS frames occasionally;
            // fall-through to space keeps downstream JSON parsers
            // happy without an extra transform.
            for &b in &ps {
                let ch = if (0x20..=0x7E).contains(&b) { b } else { b' ' };
                match ch {
                    b'"' => out.extend_from_slice(b"\\\""),
                    b'\\' => out.extend_from_slice(b"\\\\"),
                    _ => out.push(ch),
                }
            }
            out.push(b'"');
        }
        out.extend_from_slice(b"}\n");
    }
}

struct GroupParser {
    /// The four most recent blocks assembled into a group slot,
    /// indexed by offset role (A=0, B=1, C/C'=2, D=3). Each slot
    /// stores the 16-bit data payload or `None` if we haven't yet
    /// seen that offset in the current group.
    slots: [Option<u16>; 4],
    /// Authoritative state. Event emission compares against these.
    pi: Option<u16>,
    pty: Option<u8>,
    tp: Option<bool>,
    ta: Option<bool>,
    ps_buf: [u8; 8],
    ps_seen_segments: u8,
    /// Last fully-sent PS snapshot — emits only when content changes.
    last_emitted_ps: Option<[u8; 8]>,
}

impl GroupParser {
    const fn new() -> Self {
        Self {
            slots: [None; 4],
            pi: None,
            pty: None,
            tp: None,
            ta: None,
            ps_buf: [b' '; 8],
            ps_seen_segments: 0,
            last_emitted_ps: None,
        }
    }

    fn push_block(&mut self, block: RdsBlock) -> Option<GroupUpdate> {
        // Map offset → slot index. Use the A slot as the group-start
        // anchor; seeing A resets the pipeline so an out-of-order
        // block can't poison a later group.
        let idx = match block.offset {
            BlockOffset::A => 0,
            BlockOffset::B => 1,
            BlockOffset::C | BlockOffset::CPrime => 2,
            BlockOffset::D => 3,
        };
        if idx == 0 {
            self.slots = [None; 4];
        }
        self.slots[idx] = Some(block.data);

        // Parse on the D block only when we have at least A and B —
        // everything PS/PTY/PI-related is derivable from those.
        if idx != 3 {
            return None;
        }
        let (Some(a), Some(b)) = (self.slots[0], self.slots[1]) else {
            return None;
        };
        let d = self.slots[3]?;
        let c_or_cprime = self.slots[2];

        let mut update = GroupUpdate::empty();

        // Block A: PI.
        if self.pi != Some(a) {
            self.pi = Some(a);
            update.pi_changed = Some(a);
        }

        // Block B parse.
        let group_type_code = (b >> 12) & 0x0F;
        let variant_b = (b >> 11) & 0x01 == 1;
        let tp = (b >> 10) & 0x01 == 1;
        let pty = ((b >> 5) & 0x1F) as u8;
        let b_low5 = (b & 0x1F) as u8;

        if self.tp != Some(tp) {
            self.tp = Some(tp);
            update.tp_changed = Some(tp);
        }
        if self.pty != Some(pty) {
            self.pty = Some(pty);
            update.pty_changed = Some(pty);
        }

        // Group-type-specific handling. V1: 0A + 0B for PS + TA.
        if group_type_code == 0 {
            // Group 0A/0B layout of block B's low 5 bits:
            //   bit 4: TA flag
            //   bit 3: Music/Speech
            //   bit 2: DI (decoder identification) bit
            //   bits 1..0: PS segment index (0..3)
            let ta = (b_low5 >> 4) & 0x01 == 1;
            if self.ta != Some(ta) {
                self.ta = Some(ta);
                update.ta_changed = Some(ta);
            }
            let seg = (b_low5 & 0x03) as usize;
            // Block D holds the two PS chars for this segment,
            // regardless of 0A vs 0B.
            let ch_hi = ((d >> 8) & 0xFF) as u8;
            let ch_lo = (d & 0xFF) as u8;
            self.ps_buf[seg * 2] = ch_hi;
            self.ps_buf[seg * 2 + 1] = ch_lo;
            self.ps_seen_segments |= 1 << seg;
            // Only emit PS snapshots once all 4 segments have been
            // seen at least once — avoids broadcasting half-filled
            // names on first-seek. After that, emit on any change.
            if self.ps_seen_segments == 0b1111 && self.last_emitted_ps != Some(self.ps_buf) {
                self.last_emitted_ps = Some(self.ps_buf);
                update.ps_changed = Some(self.ps_buf);
            }
        }

        let _ = (variant_b, c_or_cprime); // future: RT, AF, CT
        if update.is_empty() {
            None
        } else {
            Some(update)
        }
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
        let n = fs as usize; // 1 s
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

    // ----- Phase 2/3/4 unit tests --------------------------------------

    use super::{rds_syndrome, BitSync, BlockOffset, BlockSync, GroupParser, RdsBlock};

    #[test]
    fn crc_syndrome_of_valid_codeword_is_offset() {
        // Hand-rolled golden: encode `data = 0x1234`, compute
        // `c(x) = data · x^10 mod g(x)`, form codeword = data·x^10 XOR c,
        // then XOR the offset A onto the check bits. Syndrome of the
        // resulting 26-bit word must come back as `offset`.
        let data: u32 = 0x1234;
        let shifted = data << 10;
        // Compute crc10 check bits by reducing `shifted` modulo g.
        let check = rds_syndrome(shifted);
        let codeword = shifted | u32::from(check);
        // Sanity: a valid codeword has syndrome zero.
        assert_eq!(rds_syndrome(codeword), 0);
        // Apply offset A; syndrome should become A.
        let tx = codeword ^ u32::from(BlockOffset::A.syndrome());
        assert_eq!(rds_syndrome(tx), BlockOffset::A.syndrome());
        assert_eq!(
            BlockOffset::from_syndrome(rds_syndrome(tx)),
            Some(BlockOffset::A)
        );
    }

    #[test]
    fn bit_sync_recovers_differential_encoded_bits() {
        // Encode `source = [1, 0, 1, 1, 0, 0, 1]` differentially, then
        // render each T bit as a Manchester symbol (8 samples per bit:
        // 4 high then 4 low for T=0, 4 low then 4 high for T=1).
        // Feed it to BitSync and verify the decoded stream matches the
        // original source. Starts at `last_raw = false`, so the very
        // first emitted bit is `T[0] XOR 0 = T[0]` — that's also the
        // source S[0] given the initial condition `T[-1] = 0`.
        let fs_out = 9_500.0_f32; // matches 240 kHz / decim=25
        let source = [true, false, true, true, false, false, true];

        // Differentially encode: T[n] = S[n] XOR T[n-1], T[-1]=false.
        let mut t = false;
        let transmitted: Vec<bool> = source
            .iter()
            .map(|&s| {
                t ^= s;
                t
            })
            .collect();

        // Render each T as 8 Manchester samples. 1 → [-1;4]+[+1;4],
        // 0 → [+1;4]+[-1;4]. The detector emits `raw = second_half >
        // first_half`, so for T=1 we need second_half > first_half.
        let samples_per_bit = 8_usize;
        let half = samples_per_bit / 2;
        let mut mpx = Vec::new();
        for &t in &transmitted {
            for i in 0..samples_per_bit {
                let first_half = i < half;
                let high = if t { !first_half } else { first_half };
                mpx.push(if high { 1.0_f32 } else { -1.0 });
            }
        }

        let mut bs = BitSync::new(fs_out);
        // At fs_out=9500, samples_per_bit = 8.0 exactly — no fractional
        // drift. Each 8 samples emits one bit.
        let mut decoded = Vec::new();
        for &s in &mpx {
            if let Some(bit) = bs.push(s) {
                decoded.push(bit);
            }
        }
        assert_eq!(decoded, source.to_vec());
    }

    #[test]
    fn block_sync_locks_on_offset_and_emits_blocks() {
        // Build a synthetic bit stream: two valid blocks back-to-back
        // with offsets A and B. After the sync hysteresis (2 consecutive
        // matches), a block with offset A should emerge, followed by
        // block B.
        fn build_block(data: u32, off: BlockOffset) -> u32 {
            let shifted = (data & 0xFFFF) << 10;
            let check = rds_syndrome(shifted);
            let codeword = shifted | u32::from(check);
            codeword ^ u32::from(off.syndrome())
        }

        let a = build_block(0x1234, BlockOffset::A);
        let b = build_block(0x5678, BlockOffset::B);

        let mut stream = Vec::<bool>::new();
        for &word in &[a, b] {
            for i in (0..26).rev() {
                stream.push(((word >> i) & 1) == 1);
            }
        }

        let mut bsync = BlockSync::new();
        let mut blocks = Vec::<(BlockOffset, u16)>::new();
        // Prepend 32 zero bits so the `good_run` counter can't fire
        // inside uninitialised-window territory before the real data.
        for _ in 0..32 {
            bsync.push(false);
        }
        for bit in stream {
            if let Some(blk) = bsync.push(bit) {
                blocks.push((blk.offset, blk.data));
            }
        }
        // First emitted block is block B (locks after A's second-hit
        // confirmation lands inside B), then the next call would need
        // a third block to verify — we only built two, so exactly one
        // block emits. That's enough to prove the lock path works;
        // tighter sequencing is covered by the group-parser tests in
        // the follow-up commit.
        assert!(bsync.is_synced(), "block sync should have locked");
        assert!(!blocks.is_empty(), "expected at least one block after sync");
        let (off, data) = blocks[0];
        assert_eq!(
            (off, data),
            (BlockOffset::B, 0x5678),
            "first emitted block should be B/0x5678"
        );
    }

    #[test]
    fn group_parser_assembles_ps_from_four_0a_groups() {
        // Build four 0A groups that spell "KEXP 90 " across 4 segments
        // of 2 chars each: seg 0 "KE", seg 1 "XP", seg 2 " 9", seg 3
        // "0 ". PI is constant across groups; PTY = 22 (Rock per RBDS);
        // TP = true. Verify the parser eventually emits a PS update
        // carrying the full name after the fourth group.
        let pi: u16 = 0xABCD;
        let pty: u16 = 22;
        let tp: u16 = 1;
        // Group type 0A = code 0, variant-A bit = 0 → top 5 bits = 0.
        // Block B: gggg_vttt_tt__ddddd — we manually pack:
        //   bits 15..11 = 0 (group code 0, variant A)
        //   bit 10 = TP = 1
        //   bits 9..5 = PTY
        //   bits 4..0 = 00000 | seg  (TA=0, M/S=0, DI=0)
        let make_b = |seg: u16| (tp << 10) | (pty << 5) | seg;

        let ps_chars: [&[u8; 2]; 4] = [b"KE", b"XP", b" 9", b"0 "];

        let mut parser = GroupParser::new();
        let mut updates = Vec::new();
        for (seg, pair) in ps_chars.iter().enumerate() {
            let d = u16::from(pair[0]) << 8 | u16::from(pair[1]);
            let blocks = [
                RdsBlock {
                    offset: BlockOffset::A,
                    data: pi,
                },
                RdsBlock {
                    offset: BlockOffset::B,
                    data: make_b(seg as u16),
                },
                RdsBlock {
                    offset: BlockOffset::C,
                    data: 0x0000,
                },
                RdsBlock {
                    offset: BlockOffset::D,
                    data: d,
                },
            ];
            for blk in blocks {
                if let Some(u) = parser.push_block(blk) {
                    updates.push(u);
                }
            }
        }

        // First update (from group 1, seg 0): PI + PTY + TP set, no
        // PS yet (we only emit PS once all 4 segments are seen).
        let first = updates[0];
        assert_eq!(first.pi_changed, Some(pi));
        assert_eq!(first.pty_changed, Some(pty as u8));
        assert_eq!(first.tp_changed, Some(true));
        assert_eq!(first.ps_changed, None);

        // The PS-complete event must land somewhere in the stream and
        // carry the full assembled name.
        let ps_event = updates
            .iter()
            .find_map(|u| u.ps_changed)
            .expect("should emit a PS snapshot after the 4th segment");
        assert_eq!(&ps_event, b"KEXP 90 ");
    }

    #[test]
    fn group_parser_serializes_expected_json() {
        let mut out = Vec::new();
        let update = super::GroupUpdate {
            pi_changed: Some(0x1234),
            pty_changed: Some(22),
            tp_changed: Some(true),
            ta_changed: None,
            ps_changed: Some(*b"KEXP 90 "),
        };
        update.write_json(&mut out);
        let s = std::str::from_utf8(&out).unwrap();
        let expected = "{\"t\":\"rds\",\"pi\":4660,\"pty\":22,\"tp\":true,\"ps\":\"KEXP 90 \"}\n";
        assert_eq!(s, expected, "full JSON: {s}");
    }
}
