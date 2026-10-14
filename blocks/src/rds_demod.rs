//! RDS (Radio Data System) decoder.
//!
//! RDS is the ~1.2 kbps data stream broadcast FM stations carry on a
//! 57 kHz suppressed-carrier subcarrier. Every commercial FM signal
//! has it. This block takes the FM demod's MPX output, recovers the
//! 57 kHz subcarrier, coherently mixes it to baseband, then runs the
//! full pipeline — biphase recovery → bit sync → block sync → group
//! assembler — and emits newline-delimited JSON RDS events (PI / PS /
//! RadioText / PTY / TP / TA). PS and RadioText are only emitted whole
//! and confirmed, never as they fill in (see the group handler); AF and
//! clock-time (CT) groups are not decoded.
//!
//! ### Why 57 kHz is trivially coherent to recover
//!
//! Broadcasters constrain the 57 kHz subcarrier to be phase-locked to
//! the 19 kHz stereo pilot at its third harmonic. A PLL that locks to
//! the pilot directly hands us the subcarrier reference — no separate
//! acquisition, no squaring loop. We run an `Nco` PLL at 19 kHz and
//! multiply its phase by 3 to synthesise the 57 kHz local oscillator.
//!
//! ### Signal chain
//!
//! ```text
//! MPX @ Fs ──┬── BPF 18.5..19.5 kHz ── Nco PLL lock ── phase19
//!            │                                             ├── ×3 → phase57
//! MPX @ Fs ──┴── BPF 54..60 kHz ── × cos(phase57) ─┬── LPF 2.4 kHz ── I (data)
//!                                 ── × sin(phase57) ┴── LPF 2.4 kHz ── Q (lock)
//! ```
//!
//! The pilot BPF and the RDS BPF share a tap count, so their group
//! delays match and `rds_bpf(mpx)` is already time-aligned with the
//! pilot the PLL locks to — no explicit delay line (adding one would
//! double-count the group delay; it was only invisible at 240 kHz).
//!
//! I and Q are decimated by `DECIMATE_TO_BAUD_RATIO × symbol_rate`
//! before bit sync so the symbol tracker sees a manageable sample
//! count. Bit sync then derotates (I, Q) with a slow Costas loop and
//! tracks symbol timing with an early–late gate on the Manchester
//! matched filter, so decoding follows the SDR's sample-clock error
//! instead of drifting off the symbols (audit P5-H3). The `data` port still carries the decimated baseband stream
//! for diagnostics; decoded RDS events go out the `events` port as
//! newline-delimited JSON.
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
/// No complete group for this long = RDS gone: forget the station and
/// clear the readout. ~11 groups/s arrive on a decodable signal, so 3 s
/// rides out a fade without blanking a station still tuned.
const RDS_GONE_S: f32 = 3.0;

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

    /// Phase 2: carrier-phase + symbol-timing recovery, Manchester bit
    /// decision and differential decoder fed from the decimated (I, Q).
    bit_sync: BitSync,
    /// Phase 3: 26-bit block assembler with CRC-10 / offset-word
    /// sync detection.
    block_sync: BlockSync,
    /// Phase 4: group assembler → PI / PS / PTY / TP state.
    parser: GroupParser,
    /// Decimated samples since the last complete group, and how many
    /// make "RDS is gone" ([`RDS_GONE_S`]): retuned off the station, or
    /// it faded out. Then the parser forgets the station and a
    /// [`CLEAR_JSON`] blanks the readout — once (`cleared`).
    since_group: usize,
    gone_after: usize,
    cleared: bool,
    /// UTF-8 JSON events queued for drain on the `events` output port.
    /// Newline-delimited so a downstream `EventsSink` or UI shim can
    /// split them cleanly — same convention `StereoDecoder` uses.
    events_out: Vec<u8>,
}

/// Decoder health counters — diagnostics only (measurement harnesses,
/// tests). Not part of the event output.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RdsDiag {
    /// Blocks that passed CRC / offset check while locked.
    pub blocks_ok: u64,
    /// Complete groups (A + B + D seen, parsed on D).
    pub groups: u64,
    /// Locked → Searching transitions (block-sync losses).
    pub sync_losses: u64,
    /// Symbol-timing loop's tracked clock error (ppm; + = symbols
    /// longer than nominal in input samples).
    pub clock_ppm: f64,
    /// Last decoded PI / PTY.
    pub pi: Option<u16>,
    pub pty: Option<u8>,
    /// PS buffer (raw, may be partially filled).
    pub ps: String,
    /// RadioText buffer from 2A/2B groups (raw, may be partial).
    pub rt: String,
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
            q_power: 0.0,
            i_power: 0.0,
            alpha_lock,
            accum_i: 0.0,
            accum_q: 0.0,
            bit_sync,
            block_sync: BlockSync::new(),
            parser: GroupParser::new(),
            since_group: 0,
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            gone_after: (out_rate * RDS_GONE_S) as usize,
            cleared: false,
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

    /// Snapshot of decoder health counters + the raw last-written
    /// PS / RadioText buffers (unconfirmed, unlike the events).
    #[must_use]
    pub fn diag(&self) -> RdsDiag {
        let printable = |b: &[u8]| -> String {
            b.iter()
                .map(|&c| {
                    if (0x20..=0x7E).contains(&c) {
                        c as char
                    } else {
                        ' '
                    }
                })
                .collect()
        };
        RdsDiag {
            blocks_ok: self.block_sync.blocks_ok,
            groups: self.parser.groups,
            sync_losses: self.block_sync.sync_losses,
            clock_ppm: self.bit_sync.clock_error() * 1e6,
            pi: self.parser.pi.value,
            pty: self.parser.pty.value,
            ps: printable(&self.parser.ps_buf),
            rt: printable(&self.parser.rt_buf),
        }
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

        // 57 kHz LO = 3× pilot phase. `cos(3θ)` via the trig identity
        // `cos(3θ) = 4cos³θ − 3cosθ` avoids another sin/cos evaluation;
        // same for `sin(3θ) = 3sinθ − 4sin³θ`. Since we already have
        // `ref_re = cosθ` and `ref_im = sinθ` we just cube.
        let c = ref_re;
        let s = ref_im;
        let cos3 = 4.0 * c * c * c - 3.0 * c;
        let sin3 = 3.0 * s - 4.0 * s * s * s;

        // RDS path: BPF the raw MPX. No extra delay line — the RDS BPF
        // and the pilot BPF have the same tap count (equal group delay),
        // so `rds_bpf(mpx)` and the pilot the NCO locks to
        // (`pilot_bpf(mpx)`) already correspond to the same input sample.
        // An explicit delay here would double-count the group delay; it
        // was invisible only at exactly 240 kHz, where 80 samples is 19.0
        // whole 57 kHz cycles (0 phase error) — at 200/250 kHz it
        // misaligned the coherent reference and broke the decode.
        let rds_band = self.rds_bpf.step(mpx);

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
            let groups = self.parser.groups;
            if let Some(bit) = self.bit_sync.push(out_i, out_q) {
                if let Some(block) = self.block_sync.push(bit) {
                    if let Some(update) = self.parser.push_block(block) {
                        update.write_json(&mut self.events_out);
                    }
                }
            }
            if self.parser.groups != groups {
                self.since_group = 0;
                self.cleared = false;
            } else {
                self.since_group += 1;
                // Also clears a record a previous run left in the store
                // when this one never hears RDS at all.
                if self.since_group >= self.gone_after && !self.cleared {
                    self.parser.forget();
                    self.events_out.extend_from_slice(CLEAR_JSON);
                    self.cleared = true;
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
                ai_notes: "MPX composite rate from the FM demod. Needs ≥128 kHz to keep the 57 kHz RDS subcarrier above Nyquist.",
            }],
            ai_notes: "RDS / RBDS decoder. Reads the 57 kHz subcarrier in the FM-broadcast composite signal — station name / program type / RadioText (\"now playing\"). Auto-loads in `wbfm`; surfaces in the UI's RdsReadout component and the `rds` store record (`ps`, `rt`, `pi`, `pty`), not as a `tail decoder` category.",
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
        for port in io.outputs.iter() {
            if port.name == "data" {
                if let crate::block::OutBuf::RealF32(buf) = &port.buf {
                    data_cap = buf.len();
                }
            }
        }

        let mut consumed = 0;
        let mut pending_data = Vec::with_capacity(data_cap.min(src.len()));
        for &x in src.iter() {
            // Reserve room *before* pumping a sample that would complete
            // a decimation group and emit a data soft-sample — mirror
            // `decimator.rs`. Stopping up front (rather than pumping then
            // backing out) means the pilot PLL, filters, decimator
            // accumulator and bit-sync are never double-pumped or
            // half-restored; the scheduler re-presents the unconsumed
            // tail next tick.
            if self.decim_phase + 1 >= self.decim && pending_data.len() >= data_cap {
                break;
            }
            consumed += 1;
            if let Some((i, _q)) = self.pump(x) {
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
// Phase 2: carrier-phase + symbol-timing recovery, Manchester bit
// decision, differential decode
// ---------------------------------------------------------------------------
//
// RDS source bits S[n] are transmitted as T[n] = S[n] XOR T[n-1]
// (differential) then Manchester-encoded: T[n]=1 is a low→high
// transition mid-symbol, T[n]=0 is high→low. Receiver reverses:
// recover T_r[n] from the symbol, then S_r[n] = T_r[n] XOR T_r[n-1].
//
// Input is the decimated complex baseband (I, Q) at ~8 samples per
// symbol. Three loops run on it:
//
// 1. **Carrier phase (Costas).** The 57 kHz LO is 3× the pilot PLL's
//    phase, but the spec allows the subcarrier in phase *or* in
//    quadrature with the pilot's third harmonic, and the pilot PLL /
//    band-pass carry their own static offsets. A slow BPSK Costas loop
//    rotates (I, Q) so the data lands on the real axis whatever that
//    offset is (180° ambiguity is harmless — differential coding).
//
// 2. **Symbol timing (early–late gate) — audit P5-H3.** The symbol
//    metric is the Manchester matched filter evaluated at an arbitrary
//    (fractional) symbol-end time τ over the zero-order-hold sample
//    stream: `y(τ) = ∫[τ−T/2, τ] − ∫[τ−T, τ−T/2]`. `|y|` peaks when τ
//    sits on the true symbol boundary, so `|y(τ+δ)| − |y(τ−δ)|` is a
//    timing error. A PI loop steers both the strobe phase and the
//    symbol period, so the decoder tracks the sample-clock error
//    between the SDR and the broadcaster (which is what made the old
//    free-running flywheel drift off the symbols every few tens of
//    seconds and drop sync).
//
// 3. **Half-symbol check.** Manchester has a weak second equilibrium
//    half a symbol off (on runs of equal T bits it looks like a square
//    wave at the baud rate). We keep running averages of `|y|` at τ and
//    at τ−T/2; if the off-phase is persistently stronger, jump half a
//    symbol.

/// Ring of recent derotated samples; must cover 1.5 T + δ + slack.
const SYNC_RING: usize = 64;

struct BitSync {
    // --- carrier phase -------------------------------------------------
    /// Current carrier-phase correction (rad).
    phase: f32,
    /// Smoothed |z| for normalising the Costas error.
    amp: f32,
    // --- symbol timing --------------------------------------------------
    /// Derotated real samples, indexed by absolute sample number.
    ring: [f32; SYNC_RING],
    /// Number of samples pushed so far (sample k covers [k, k+1)).
    n: u64,
    /// Nominal samples per symbol.
    sps: f64,
    /// Absolute time (samples) of the next symbol-end strobe.
    tau: f64,
    /// Integrator of the timing loop — fractional period correction
    /// (i.e. the tracked clock error).
    freq: f64,
    /// Running means of |y| on-phase and half a symbol off.
    avg_on: f32,
    avg_off: f32,
    /// Symbols since the last half-symbol jump.
    since_flip: u32,
    last_raw: bool,
}

impl BitSync {
    /// Costas loop gain (per sample, on the normalised error).
    const COSTAS_GAIN: f32 = 0.002;
    /// Timing loop proportional / integral gains (per symbol, on the
    /// normalised early–late error).
    const KP: f64 = 0.01;
    const KI: f64 = 2e-5;
    /// Clamp on the tracked clock error (±0.5 %).
    const FREQ_LIMIT: f64 = 5e-3;
    /// Early/late offset as a fraction of a symbol.
    const EL_FRAC: f64 = 0.125;
    /// Half-symbol check: averaging constant, ratio and holdoff.
    const AVG_ALPHA: f32 = 1.0 / 64.0;
    const FLIP_RATIO: f32 = 1.3;
    const FLIP_HOLDOFF: u32 = 128;

    fn new(input_rate_hz: f32) -> Self {
        let sps = f64::from(input_rate_hz / RDS_BAUD);
        Self {
            phase: 0.0,
            amp: 0.0,
            ring: [0.0; SYNC_RING],
            n: 0,
            sps,
            tau: sps,
            freq: 0.0,
            avg_on: 0.0,
            avg_off: 0.0,
            since_flip: 0,
            last_raw: false,
        }
    }

    /// Tracked clock error (fractional; +1e-6 = symbols 1 ppm longer
    /// than nominal in input samples).
    const fn clock_error(&self) -> f64 {
        self.freq
    }

    /// ∫ x(t) dt over [a, b] (absolute sample time, ZOH samples).
    /// Caller guarantees the span is inside the ring and already pushed.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn integral(&self, a: f64, b: f64) -> f32 {
        let mut acc = 0.0_f64;
        let mut k = a.floor().max(0.0) as u64;
        while (k as f64) < b {
            let lo = a.max(k as f64);
            let hi = b.min((k + 1) as f64);
            if hi > lo {
                acc += f64::from(self.ring[(k as usize) % SYNC_RING]) * (hi - lo);
            }
            k += 1;
        }
        acc as f32
    }

    /// Manchester matched-filter output for a symbol ending at `t`.
    fn metric(&self, t: f64) -> f32 {
        let h = self.sps * 0.5;
        self.integral(t - h, t) - self.integral(t - 2.0 * h, t - h)
    }

    /// Push one complex soft sample; return `Some(bit)` each time a
    /// full symbol completes. `bit` is the differentially-decoded
    /// source bit (true = 1).
    #[allow(clippy::cast_precision_loss)]
    fn push(&mut self, i: f32, q: f32) -> Option<bool> {
        // Carrier derotation + Costas update (BPSK: error = sgn(re)·im).
        let (s, c) = self.phase.sin_cos();
        let re = i * c + q * s;
        let im = q * c - i * s;
        self.amp += 0.001 * ((re * re + im * im).sqrt() - self.amp);
        if self.amp > 1e-12 {
            let err = re.signum() * im / self.amp;
            self.phase += Self::COSTAS_GAIN * err.clamp(-1.0, 1.0);
            if self.phase > core::f32::consts::PI {
                self.phase -= core::f32::consts::TAU;
            } else if self.phase < -core::f32::consts::PI {
                self.phase += core::f32::consts::TAU;
            }
        }

        self.ring[(self.n as usize) % SYNC_RING] = re;
        self.n += 1;

        let delta = self.sps * Self::EL_FRAC;
        // Strobe once the late gate's window has been fully received.
        if (self.n as f64) < self.tau + delta {
            return None;
        }
        let tau = self.tau;
        let y = self.metric(tau);
        let early = self.metric(tau - delta).abs();
        let late = self.metric(tau + delta).abs();
        let off = self.metric(tau - self.sps * 0.5).abs();

        // Early–late timing error, normalised to ~[-1, 1]. Positive =
        // the peak is later than τ → stretch the next interval.
        let denom = early + late;
        let err = if denom > 1e-20 {
            f64::from((late - early) / denom)
        } else {
            0.0
        };
        self.freq = (self.freq + Self::KI * err).clamp(-Self::FREQ_LIMIT, Self::FREQ_LIMIT);
        let mut next = tau + self.sps * (1.0 + self.freq + Self::KP * err);

        // Half-symbol ambiguity check.
        self.avg_on += Self::AVG_ALPHA * (y.abs() - self.avg_on);
        self.avg_off += Self::AVG_ALPHA * (off - self.avg_off);
        self.since_flip = self.since_flip.saturating_add(1);
        if self.since_flip > Self::FLIP_HOLDOFF && self.avg_off > Self::FLIP_RATIO * self.avg_on {
            // The better phase is half a symbol earlier: strobe there
            // next instead of a full symbol on.
            next -= self.sps * 0.5;
            std::mem::swap(&mut self.avg_on, &mut self.avg_off);
            self.since_flip = 0;
        }
        self.tau = next;

        // Manchester: low→high transition (second half > first half)
        // means T=1.
        let raw = y > 0.0;
        let decoded = raw != self.last_raw;
        self.last_raw = raw;
        Some(decoded)
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
    /// Diagnostics: CRC-good blocks emitted / Locked→Searching drops.
    blocks_ok: u64,
    sync_losses: u64,
}

impl BlockSync {
    /// Drop back to searching after this many consecutive bad CRCs.
    const UNLOCK_THRESHOLD: u32 = 2;

    fn new() -> Self {
        Self {
            window: 0,
            state: SyncState::Searching,
            blocks_ok: 0,
            sync_losses: 0,
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
                    self.blocks_ok += 1;
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
                    self.blocks_ok += 1;
                    return Some(RdsBlock {
                        offset: off,
                        data: ((self.window >> 10) & 0xFFFF) as u16,
                    });
                }
                let bad = consecutive_bad + 1;
                if bad >= Self::UNLOCK_THRESHOLD {
                    self.state = SyncState::Searching;
                    self.sync_losses += 1;
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
// ASCII chars. RadioText (2A: up to 64 chars in 16 segments of 4,
// 2B: 32 chars in 16 segments of 2) works the same way, ending early
// at a 0x0D carriage return.
//
// Neither is emitted as it fills in. Many US stations scroll a slogan
// through the 8-char PS ("MIX 106." → "5 Today'" → …), so a buffer
// written segment by segment mixes two frames ("5 Toda6."); a lost
// group or a bit error does the same to RadioText. [`SegText`] only
// reports a text once one whole pass (segment 0, 1, … in order, none
// missing) arrives twice in a row unchanged.

/// Segments of one RDS text field, reported only when whole and
/// confirmed — see the section comment above.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SegText {
    max_segs: usize,
    /// The pass being assembled, segment 0 onward.
    cur: Vec<u8>,
    /// The segment expected next; `None` = waiting for a segment 0.
    next: Option<usize>,
    /// The last complete pass, to confirm the next one against.
    prev: Option<Vec<u8>>,
    /// The last text reported, so a repeat isn't re-sent.
    emitted: Option<Vec<u8>>,
}

impl SegText {
    const fn new(max_segs: usize) -> Self {
        Self {
            max_segs,
            cur: Vec::new(),
            next: None,
            prev: None,
            emitted: None,
        }
    }

    /// Drop any partial or unconfirmed pass (the text is changing).
    fn restart(&mut self) {
        self.cur.clear();
        self.next = None;
        self.prev = None;
    }

    /// Feed one segment. Returns the text when a pass completes, equals
    /// the pass before it, and differs from the last text reported.
    /// `short_ok`: a pass may end at a wrap back to segment 0 without a
    /// 0x0D (RadioText from stations that only send the used segments).
    fn push(&mut self, seg: usize, chars: &[u8], short_ok: bool) -> Option<Vec<u8>> {
        let mut out = None;
        if seg == 0 {
            if short_ok && self.next.is_some_and(|n| n > 0) {
                let pass = std::mem::take(&mut self.cur);
                out = self.finish(pass);
            }
            self.cur.clear();
            self.next = Some(0);
        }
        if self.next != Some(seg) {
            // A segment was lost or arrived out of order: this pass
            // can't be trusted, wait for the next segment 0.
            self.next = None;
            return out;
        }
        self.cur.extend_from_slice(chars);
        self.next = Some(seg + 1);
        let end = self.cur.iter().position(|&c| c == 0x0D);
        if end.is_some() || seg + 1 == self.max_segs {
            if let Some(end) = end {
                self.cur.truncate(end);
            }
            self.next = None;
            let pass = std::mem::take(&mut self.cur);
            out = self.finish(pass);
        }
        out
    }

    fn finish(&mut self, pass: Vec<u8>) -> Option<Vec<u8>> {
        let confirmed = self.prev.as_ref() == Some(&pass);
        self.prev = Some(pass.clone());
        if confirmed && self.emitted.as_ref() != Some(&pass) {
            self.emitted = Some(pass.clone());
            Some(pass)
        } else {
            None
        }
    }
}

/// A per-group field (PI, PTY, TP, TA) taken only once two groups in a
/// row agree: one bit error that slips past the CRC-10 must not flip
/// it (seen live: one group read PTY 17 / TP on a PTY-9 station).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Confirmed<T> {
    value: Option<T>,
    candidate: Option<T>,
}

impl<T: Copy + PartialEq> Confirmed<T> {
    const fn new() -> Self {
        Self {
            value: None,
            candidate: None,
        }
    }

    /// Feed one reading; returns the new value when it changes.
    fn see(&mut self, v: T) -> Option<T> {
        if self.value == Some(v) {
            self.candidate = None;
            None
        } else if self.candidate == Some(v) {
            self.value = Some(v);
            self.candidate = None;
            Some(v)
        } else {
            self.candidate = Some(v);
            None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupUpdate {
    pi_changed: Option<u16>,
    pty_changed: Option<u8>,
    tp_changed: Option<bool>,
    ta_changed: Option<bool>,
    /// A confirmed station name (8 chars), or empty to clear the old
    /// station's on a PI change. `None` = no PS change in this group.
    ps_changed: Option<Vec<u8>>,
    /// A confirmed RadioText (trailing spaces trimmed), or empty to
    /// clear. `None` = no change.
    rt_changed: Option<Vec<u8>>,
}

/// One JSON string of RDS text: printable ASCII, anything else as a
/// space (malformed frames carry control bytes now and then).
fn push_json_text(out: &mut Vec<u8>, text: &[u8]) {
    out.push(b'"');
    for &b in text {
        let ch = if (0x20..=0x7E).contains(&b) { b } else { b' ' };
        match ch {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            _ => out.push(ch),
        }
    }
    out.push(b'"');
}

impl GroupUpdate {
    const fn empty() -> Self {
        Self {
            pi_changed: None,
            pty_changed: None,
            tp_changed: None,
            ta_changed: None,
            ps_changed: None,
            rt_changed: None,
        }
    }

    const fn is_empty(&self) -> bool {
        self.pi_changed.is_none()
            && self.pty_changed.is_none()
            && self.tp_changed.is_none()
            && self.ta_changed.is_none()
            && self.ps_changed.is_none()
            && self.rt_changed.is_none()
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
        if let Some(ps) = &self.ps_changed {
            out.extend_from_slice(br#","ps":"#);
            push_json_text(out, ps);
        }
        if let Some(rt) = &self.rt_changed {
            out.extend_from_slice(br#","rt":"#);
            push_json_text(out, rt);
        }
        out.extend_from_slice(b"}\n");
    }
}

/// Clears every field downstream (the store merges records, so absent
/// fields would keep the last station's values): sent when RDS has been
/// gone long enough that the readout would otherwise show a station
/// that is no longer tuned.
const CLEAR_JSON: &[u8] = br#"{"t":"rds","pi":null,"pty":null,"tp":null,"ta":null,"ps":"","rt":""}
"#;

struct GroupParser {
    /// The four most recent blocks assembled into a group slot,
    /// indexed by offset role (A=0, B=1, C/C'=2, D=3). Each slot
    /// stores the 16-bit data payload or `None` if we haven't yet
    /// seen that offset in the current group.
    slots: [Option<u16>; 4],
    /// Authoritative state, each confirmed by two groups in a row.
    pi: Confirmed<u16>,
    pty: Confirmed<u8>,
    tp: Confirmed<bool>,
    ta: Confirmed<bool>,
    /// Raw PS / RadioText as last written, for [`RdsDiag`] only.
    ps_buf: [u8; 8],
    rt_buf: [u8; 64],
    /// What is emitted: confirmed whole passes.
    ps: SegText,
    rt: SegText,
    /// RadioText A/B flag and 2A-vs-2B variant of the current text; a
    /// change of either means a new text.
    rt_ab: Option<(bool, bool)>,
    /// Diagnostics: complete groups parsed.
    groups: u64,
}

impl GroupParser {
    const fn new() -> Self {
        Self {
            slots: [None; 4],
            pi: Confirmed::new(),
            pty: Confirmed::new(),
            tp: Confirmed::new(),
            ta: Confirmed::new(),
            ps_buf: [b' '; 8],
            rt_buf: [b' '; 64],
            ps: SegText::new(4),
            rt: SegText::new(16),
            rt_ab: None,
            groups: 0,
        }
    }

    /// Forget the station (signal gone). Returns whether there was one
    /// to forget, i.e. whether downstream needs a [`CLEAR_JSON`].
    fn forget(&mut self) -> bool {
        let had = self.pi.value.is_some();
        let groups = self.groups;
        *self = Self::new();
        self.groups = groups;
        had
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
        // Block C feeds RT (2A) below. Take it before consuming the
        // group.
        let c_or_cprime = self.slots[2];
        // Consume the group: a later D must not pair with this group's
        // (stale) B if the next A/B are lost — that would write PS
        // characters into the wrong segment.
        self.slots = [None; 4];
        self.groups += 1;

        let mut update = GroupUpdate::empty();

        // Block A: PI. Until two groups agree on it, distrust the whole
        // group. A confirmed different PI is a different station: drop
        // the old one's texts and clear them downstream.
        if self.pi.value != Some(a) {
            self.pi.see(a)?;
            let (groups, pi) = (self.groups, self.pi);
            *self = Self::new();
            self.groups = groups;
            self.pi = pi;
            update.pi_changed = Some(a);
            update.ps_changed = Some(Vec::new());
            update.rt_changed = Some(Vec::new());
        } else {
            self.pi.see(a);
        }

        // Block B parse.
        let group_type_code = (b >> 12) & 0x0F;
        let variant_b = (b >> 11) & 0x01 == 1;
        let tp = (b >> 10) & 0x01 == 1;
        let pty = ((b >> 5) & 0x1F) as u8;
        let b_low5 = (b & 0x1F) as u8;

        update.tp_changed = self.tp.see(tp);
        update.pty_changed = self.pty.see(pty);

        // Group-type-specific handling: 0A + 0B for PS + TA.
        if group_type_code == 0 {
            // Group 0A/0B layout of block B's low 5 bits:
            //   bit 4: TA flag
            //   bit 3: Music/Speech
            //   bit 2: DI (decoder identification) bit
            //   bits 1..0: PS segment index (0..3)
            let ta = (b_low5 >> 4) & 0x01 == 1;
            update.ta_changed = self.ta.see(ta);
            let seg = (b_low5 & 0x03) as usize;
            // Block D holds the two PS chars for this segment,
            // regardless of 0A vs 0B.
            let pair = d.to_be_bytes();
            self.ps_buf[seg * 2..seg * 2 + 2].copy_from_slice(&pair);
            if let Some(ps) = self.ps.push(seg, &pair, false) {
                update.ps_changed = Some(ps);
            }
        }

        // Group 2A/2B: RadioText. Block B bit 4 is the A/B text flag —
        // it toggles when the station starts a new text.
        if group_type_code == 2 {
            let seg = usize::from(b & 0x0F);
            let ab = (b_low5 >> 4) & 0x01 == 1;
            if self.rt_ab != Some((ab, variant_b)) {
                self.rt_ab = Some((ab, variant_b));
                self.rt.restart();
                self.rt_buf = [b' '; 64];
            }
            let chars: Option<Vec<u8>> = if variant_b {
                Some(d.to_be_bytes().to_vec())
            } else {
                c_or_cprime.map(|c| [c.to_be_bytes(), d.to_be_bytes()].concat())
            };
            if let Some(chars) = chars {
                let at = seg * chars.len();
                self.rt_buf[at..at + chars.len()].copy_from_slice(&chars);
                if let Some(mut rt) = self.rt.push(seg, &chars, true) {
                    while rt.last() == Some(&b' ') {
                        rt.pop();
                    }
                    update.rt_changed = Some(rt);
                }
            } else {
                // 2A without its block C: a hole in this pass.
                self.rt.next = None;
            }
        }

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

        // The late timing gate looks ~1 sample past the symbol end, so
        // pad with a couple of trailing samples for the final strobe.
        mpx.extend_from_slice(&[0.0, 0.0]);

        let mut bs = BitSync::new(fs_out);
        // At fs_out=9500, samples_per_bit = 8.0 exactly — no fractional
        // drift. Each 8 samples emits one bit.
        let mut decoded = Vec::new();
        for &s in &mpx {
            if let Some(bit) = bs.push(s, 0.0) {
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
        // Three passes: group 1 only proposes the PI (losing the first
        // pass), and a name is only reported once a pass repeats.
        for (seg, pair) in ps_chars.iter().cycle().take(12).enumerate() {
            let seg = seg % 4;
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

        // Group 1 only proposes the PI; group 2 confirms it and clears
        // the previous station's PS / RT; PTY and TP confirm next.
        let first = &updates[0];
        assert_eq!(first.pi_changed, Some(pi));
        assert_eq!(first.ps_changed.as_deref(), Some(&b""[..]));
        assert!(updates.iter().any(|u| u.pty_changed == Some(pty as u8)));
        assert!(updates.iter().any(|u| u.tp_changed == Some(true)));

        // Exactly one non-empty PS, after the second whole pass.
        let names: Vec<_> = updates
            .iter()
            .filter_map(|u| u.ps_changed.clone())
            .filter(|p| !p.is_empty())
            .collect();
        assert_eq!(names, vec![b"KEXP 90 ".to_vec()]);
    }

    #[test]
    fn group_parser_serializes_expected_json() {
        let mut out = Vec::new();
        let update = super::GroupUpdate {
            pi_changed: Some(0x1234),
            pty_changed: Some(22),
            tp_changed: Some(true),
            ta_changed: None,
            ps_changed: Some(b"KEXP 90 ".to_vec()),
            rt_changed: Some(b"Now: \"Song\"".to_vec()),
        };
        update.write_json(&mut out);
        let s = std::str::from_utf8(&out).unwrap();
        let expected = "{\"t\":\"rds\",\"pi\":4660,\"pty\":22,\"tp\":true,\"ps\":\"KEXP 90 \",\"rt\":\"Now: \\\"Song\\\"\"}\n";
        assert_eq!(s, expected, "full JSON: {s}");
    }

    /// Feed one group (A, B, C, D) and collect any update.
    fn feed(
        p: &mut super::GroupParser,
        pi: u16,
        b: u16,
        c: u16,
        d: u16,
    ) -> Option<super::GroupUpdate> {
        use super::{BlockOffset, RdsBlock};
        let mut out = None;
        for (offset, data) in [
            (BlockOffset::A, pi),
            (BlockOffset::B, b),
            (BlockOffset::C, c),
            (BlockOffset::D, d),
        ] {
            if let Some(u) = p.push_block(RdsBlock { offset, data }) {
                out = Some(u);
            }
        }
        out
    }

    /// A parser already locked to PI 0x1D2B: two group-1A groups (not
    /// otherwise decoded) confirm the PI, as on air.
    fn primed() -> super::GroupParser {
        let mut p = super::GroupParser::new();
        for _ in 0..2 {
            let _ = feed(&mut p, 0x1D2B, 1 << 12, 0, 0);
        }
        p
    }

    /// One 0A pass of an 8-char PS; returns the PS values reported.
    fn ps_pass(p: &mut super::GroupParser, name: &[u8; 8]) -> Vec<String> {
        (0..4)
            .filter_map(|seg| {
                let d = u16::from_be_bytes([name[seg * 2], name[seg * 2 + 1]]);
                feed(p, 0x1D2B, seg as u16, 0, d)?.ps_changed
            })
            .filter(|ps| !ps.is_empty())
            .map(|ps| String::from_utf8(ps).unwrap())
            .collect()
    }

    #[test]
    fn scrolling_ps_never_reports_a_mixed_frame() {
        // MIX 106.5 (found live 2026-10-14) scrolls "MIX 106.5 Today's
        // Best Mix" through the PS. Frames change mid-pass, so a buffer
        // written per segment showed "5 Toda6.". Only whole frames that
        // repeat unchanged may come out.
        let frames: [&[u8; 8]; 3] = [b"MIX 106.", b"5 Today'", b"s Best M"];
        let mut p = primed();
        let mut seen = Vec::new();
        for f in frames {
            // The frame changes mid-pass: segments 2, 3 of the new frame
            // complete a pass begun with the old one's 0, 1 ("5 Toda6.").
            for seg in [2_usize, 3] {
                let d = u16::from_be_bytes([f[seg * 2], f[seg * 2 + 1]]);
                if let Some(ps) = feed(&mut p, 0x1D2B, seg as u16, 0, d)
                    .and_then(|u| u.ps_changed)
                    .filter(|ps| !ps.is_empty())
                {
                    seen.push(String::from_utf8(ps).unwrap());
                }
            }
            for _ in 0..3 {
                seen.extend(ps_pass(&mut p, f));
            }
            for seg in [0_usize, 1] {
                let d = u16::from_be_bytes([f[seg * 2], f[seg * 2 + 1]]);
                let _ = feed(&mut p, 0x1D2B, seg as u16, 0, d);
            }
        }
        assert_eq!(seen, vec!["MIX 106.", "5 Today'", "s Best M"]);
    }

    #[test]
    fn ps_pass_with_a_lost_segment_is_not_reported() {
        let mut p = primed();
        assert!(
            ps_pass(&mut p, b"KEXP 90 ").is_empty(),
            "one pass: unconfirmed"
        );
        // Segment 2 lost: the pass is dropped, nothing confirms.
        for seg in [0_usize, 1, 3] {
            let d = u16::from_be_bytes([b"KEXP 90 "[seg * 2], b"KEXP 90 "[seg * 2 + 1]]);
            assert!(feed(&mut p, 0x1D2B, seg as u16, 0, d)
                .and_then(|u| u.ps_changed)
                .is_none());
        }
        assert_eq!(ps_pass(&mut p, b"KEXP 90 "), vec!["KEXP 90 "]);
    }

    /// 2A RadioText groups for `text`, padded to whole 4-char segments.
    fn rt_2a(p: &mut super::GroupParser, text: &[u8], ab: bool) -> Vec<String> {
        let mut t = text.to_vec();
        while !t.len().is_multiple_of(4) {
            t.push(b' ');
        }
        t.chunks(4)
            .enumerate()
            .filter_map(|(seg, ch)| {
                let b = (2 << 12) | (u16::from(ab) << 4) | seg as u16;
                let c = u16::from_be_bytes([ch[0], ch[1]]);
                let d = u16::from_be_bytes([ch[2], ch[3]]);
                feed(p, 0x1D2B, b, c, d)?.rt_changed
            })
            .filter(|rt| !rt.is_empty())
            .map(|rt| String::from_utf8(rt).unwrap())
            .collect()
    }

    #[test]
    fn radiotext_2a_is_reported_whole_and_confirmed() {
        let mut p = primed();
        let text = b"Now playing: Artist - Title\r";
        assert!(
            rt_2a(&mut p, text, false).is_empty(),
            "one pass: unconfirmed"
        );
        assert_eq!(
            rt_2a(&mut p, text, false),
            vec!["Now playing: Artist - Title"]
        );
        assert!(
            rt_2a(&mut p, text, false).is_empty(),
            "a repeat isn't re-sent"
        );
        // A/B flag toggles: a new text, confirmed afresh.
        let next = b"Next: Other Song\r";
        assert!(rt_2a(&mut p, next, true).is_empty());
        assert_eq!(rt_2a(&mut p, next, true), vec!["Next: Other Song"]);
    }

    #[test]
    fn radiotext_without_carriage_return_ends_at_the_wrap() {
        // Stations that send only the used segments, no 0x0D: the pass
        // ends when segment 0 comes round again; padding is trimmed.
        let mut p = primed();
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.extend(rt_2a(&mut p, b"MIX 106.5", false));
        }
        assert_eq!(seen, vec!["MIX 106.5"]);
    }

    #[test]
    fn a_single_corrupt_group_does_not_flip_pty() {
        let mut p = primed();
        let b = |pty: u16| (1 << 12) | (pty << 5);
        // Priming left PTY 0; PTY 9 takes two groups.
        assert_eq!(
            feed(&mut p, 0x1D2B, b(9), 0, 0).and_then(|u| u.pty_changed),
            None
        );
        assert_eq!(
            feed(&mut p, 0x1D2B, b(9), 0, 0).and_then(|u| u.pty_changed),
            Some(9)
        );
        // One group reads 17 (slipped past the CRC), then 9 again.
        assert_eq!(
            feed(&mut p, 0x1D2B, b(17), 0, 0).and_then(|u| u.pty_changed),
            None
        );
        assert_eq!(
            feed(&mut p, 0x1D2B, b(9), 0, 0).and_then(|u| u.pty_changed),
            None
        );
    }

    #[test]
    fn a_new_pi_clears_the_old_station() {
        let mut p = primed();
        ps_pass(&mut p, b"KEXP 90 ");
        assert_eq!(ps_pass(&mut p, b"KEXP 90 "), vec!["KEXP 90 "]);
        // One group with a new PI could be a bit error: ignored.
        assert!(feed(&mut p, 0x1234, 0, 0, u16::from_be_bytes(*b"MI")).is_none());
        let u = feed(&mut p, 0x1234, 0, 0, u16::from_be_bytes(*b"MI")).unwrap();
        assert_eq!(u.pi_changed, Some(0x1234));
        assert_eq!(u.ps_changed.as_deref(), Some(&b""[..]));
        assert_eq!(u.rt_changed.as_deref(), Some(&b""[..]));
    }

    // ----- P5-H3: symbol-timing recovery under sample-clock error ------

    /// Encode one 26-bit block (16 data + 10 check ⊕ offset), MSB first.
    fn encode_block(data: u16, off: BlockOffset, out: &mut Vec<bool>) {
        let shifted = u32::from(data) << 10;
        let word = (shifted | u32::from(rds_syndrome(shifted))) ^ u32::from(off.syndrome());
        for i in (0..26).rev() {
            out.push((word >> i) & 1 == 1);
        }
    }

    /// Source bit stream of `n_groups` 0A groups: PI 0x12BC, PTY 10,
    /// PS "FERRITE!" cycling over its 4 segments.
    fn synth_groups(n_groups: usize) -> Vec<bool> {
        const PI: u16 = 0x12BC;
        const PS: &[u8; 8] = b"FERRITE!";
        let mut bits = Vec::with_capacity(n_groups * 104);
        for g in 0..n_groups {
            let seg = (g % 4) as u16;
            let b = (10 << 5) | seg; // 0A, TP=0, PTY=10, TA=0
            let d = u16::from(PS[seg as usize * 2]) << 8 | u16::from(PS[seg as usize * 2 + 1]);
            encode_block(PI, BlockOffset::A, &mut bits);
            encode_block(b, BlockOffset::B, &mut bits);
            encode_block(0xE0CD, BlockOffset::C, &mut bits);
            encode_block(d, BlockOffset::D, &mut bits);
        }
        bits
    }

    /// Render the bits as differential-Manchester baseband sampled at
    /// `fs` by a receiver whose clock disagrees with the broadcaster's:
    /// `ppm(t)` is the instantaneous error (+ = receiver sees symbols
    /// that are longer, in its own samples). Each output sample is the
    /// mean of the waveform over its interval (what the block's boxcar
    /// decimator does), plus deterministic noise, rotated by a static
    /// carrier-phase offset onto (I, Q).
    fn synth_baseband(bits: &[bool], fs: f64, ppm: impl Fn(f64) -> f64) -> Vec<(f32, f32)> {
        // Differential encode.
        let mut t_prev = false;
        let tx: Vec<bool> = bits
            .iter()
            .map(|&b| {
                t_prev ^= b;
                t_prev
            })
            .collect();
        const SUB: usize = 8;
        let (rot_s, rot_c) = 0.5_f32.sin_cos(); // ~29° carrier offset
        let mut lcg: u32 = 0x1234_5678;
        let mut noise = move || {
            lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (lcg >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        };
        let mut out = Vec::new();
        // Symbol-clock phase (in symbols), integrated sub-sample by
        // sub-sample so time-varying ppm is handled. Starts mid-symbol
        // so acquisition from an arbitrary phase is exercised too.
        let mut sym = 0.37_f64;
        let dt = 1.0 / (fs * SUB as f64);
        let mut t = 0.0_f64;
        'outer: loop {
            let mut acc = 0.0_f32;
            for _ in 0..SUB {
                let k = sym.floor() as usize;
                if k >= tx.len() {
                    break 'outer;
                }
                let first_half = sym.fract() < 0.5;
                // T=1: low→high; T=0: high→low.
                let high = if tx[k] { !first_half } else { first_half };
                acc += if high { 1.0 } else { -1.0 };
                sym += f64::from(super::RDS_BAUD) * dt / (1.0 + ppm(t) * 1e-6);
                t += dt;
            }
            let v = acc / SUB as f32 + 0.8 * noise();
            out.push((v * rot_c, v * rot_s));
        }
        out
    }

    /// Run soft samples through bit sync → block sync → group parser
    /// (exactly the chain `pump` drives). Returns groups decoded per
    /// whole second plus the parser for field checks.
    fn decode(fs: f32, samples: &[(f32, f32)]) -> (Vec<u64>, u64, GroupParser) {
        let mut bs = BitSync::new(fs);
        let mut bsync = BlockSync::new();
        let mut parser = GroupParser::new();
        let per_sec = fs as usize;
        let mut buckets = Vec::new();
        let mut last = 0;
        for (n, &(i, q)) in samples.iter().enumerate() {
            if let Some(bit) = bs.push(i, q) {
                if let Some(block) = bsync.push(bit) {
                    let _ = parser.push_block(block);
                }
            }
            if (n + 1) % per_sec == 0 {
                buckets.push(parser.groups - last);
                last = parser.groups;
            }
        }
        (buckets, bsync.sync_losses, parser)
    }

    fn assert_tracks(label: &str, ppm: impl Fn(f64) -> f64) {
        // 240 kHz MPX / decim 25 = 9600 Hz, the block's internal rate.
        let fs = 9_600.0_f32;
        let secs = 70.0;
        let n_groups = (secs * f64::from(super::RDS_BAUD) / 104.0) as usize;
        let samples = synth_baseband(&synth_groups(n_groups), f64::from(fs), ppm);
        let (per_sec, losses, parser) = decode(fs, &samples);
        let ideal = f64::from(super::RDS_BAUD) / 104.0;
        // Skip the first second (acquisition); every later full second
        // must decode near the ideal group rate — no drop-outs.
        let steady = &per_sec[1..per_sec.len() - 1];
        assert!(
            steady.len() >= 60,
            "{label}: only {} s decoded",
            steady.len()
        );
        let min = *steady.iter().min().unwrap();
        let mean = steady.iter().sum::<u64>() as f64 / steady.len() as f64;
        assert!(
            min >= 10 && mean >= 0.95 * ideal && losses == 0,
            "{label}: group rate min {min}/s mean {mean:.2}/s (ideal {ideal:.2}), \
             {losses} sync losses; per-second: {per_sec:?}"
        );
        assert_eq!(parser.pi.value, Some(0x12BC), "{label}: PI");
        assert_eq!(parser.pty.value, Some(10), "{label}: PTY");
        assert_eq!(&parser.ps_buf, b"FERRITE!", "{label}: PS");
    }

    #[test]
    fn symbol_timing_tracks_plus_300_ppm_clock_error() {
        assert_tracks("+300 ppm", |_| 300.0);
    }

    #[test]
    fn symbol_timing_tracks_minus_300_ppm_clock_error() {
        assert_tracks("-300 ppm", |_| -300.0);
    }

    #[test]
    fn symbol_timing_tracks_slow_clock_drift() {
        // Drift −150 → +150 ppm over the run (thermal-drift shape).
        assert_tracks("drift", |t| -150.0 + 300.0 * t / 70.0);
    }

    #[test]
    fn symbol_timing_holds_with_nominal_clock() {
        assert_tracks("0 ppm", |_| 0.0);
    }
}
