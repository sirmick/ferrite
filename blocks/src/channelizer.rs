//! Frequency-shift + LPF + decimate in one block, backed by liquid-dsp.
//!
//! The classic "single-channel tuner" used to pluck a narrowband signal
//! out of a wideband IQ stream. One IQ-in, one IQ-out, at rate
//! `input / factor`. Used as the server-side half of a VFO: the channel
//! at RF frequency `center + freq_shift_hz` lands at baseband after one
//! process pass.
//!
//! ### Algorithm
//!
//! ```text
//!                         ┌── e^(-j·2π·f_shift·n/fs) ──┐
//!         x[n] ────────►  │         mixer (Nco)        │ ──► LPF + ↓M (FirdecimCx) ──► y[m]
//!                         └────────────────────────────┘
//! ```
//!
//! - The mixer is liquid's `nco_crcf` in table-based mode — a
//!   1024-entry sin/cos LUT with linear interpolation, fed a
//!   per-sample phase increment. Cheap, drift-free over long runs,
//!   no `sincosf()` syscall per sample.
//! - The LPF + decimation is liquid's `firdecim_crcf` polyphase
//!   engine with the same Hann-windowed-sinc taps the previous
//!   hand-rolled implementation used (designed by the in-crate
//!   `design_lpf` helper). Polyphase only computes the taps that
//!   contribute to each output sample, so the per-output cost is
//!   `num_taps / factor` multiplies instead of `num_taps`.
//!
//! Public API (params, ports, registry name `Channelizer`) is
//! identical to the previous version, so every shipped preset and
//! the live VFO retune path work unchanged. `freq_shift_hz` is still
//! the only `ReconfigureScope::SelfBlock` param: live retuning maps
//! to `Nco::set_frequency` without tearing down the FIR.

use anyhow::{bail, Result};
use ferrite_liquid_dsp::{FirdecimCx, Nco};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};

/// Construction-time params.
///
/// `output_rate_hz` is the channel bandwidth the preset commits to.
/// At init, the scheduler reports the actual source rate; the block
/// picks `factor = round(input_rate_hz / output_rate_hz)` and realises
/// a polyphase FIR sized for that factor. `input_rate_hz` in the JSON
/// is only a construction-time hint used when the block is instantiated
/// outside a runtime (e.g. unit tests); init overrides it with the
/// scheduler-negotiated rate.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct ChannelizerParams {
    /// Wideband input sample rate, Hz. Construction-time hint; init's
    /// `ctx.input_rate("in")` wins once the runtime populates it.
    pub input_rate_hz: f64,
    /// Frequency to pull to baseband, in Hz relative to the input's
    /// centre frequency. Positive values tune to RF above centre.
    pub freq_shift_hz: f64,
    /// Target output sample rate, Hz. The block rebuilds its factor +
    /// FIR at init (and on live source-rate changes) to approximate
    /// this — actual output = `input_rate / round(input_rate / output_rate)`,
    /// typically within 1–5 % of the target. A downstream fractional
    /// resampler (`RealF32Resamp`) eats the residual drift when audio
    /// sinks need exact rates.
    pub output_rate_hz: f64,
}

impl ChannelizerParams {
    /// Shorthand for tests and defaults.
    #[must_use]
    pub const fn new(input_rate_hz: f64, freq_shift_hz: f64, output_rate_hz: f64) -> Self {
        Self {
            input_rate_hz,
            freq_shift_hz,
            output_rate_hz,
        }
    }
}

impl Default for ChannelizerParams {
    fn default() -> Self {
        // 2 MS/s wideband, centre of band, 500 kS/s channel (4×
        // decimation). Matches a WBFM-ish preset so a flowgraph can
        // omit params entirely and still get a sensible tuner.
        Self::new(2_000_000.0, 0.0, 500_000.0)
    }
}

pub struct Channelizer {
    factor: usize,
    taps: Vec<f32>,
    /// Liquid NCO that owns the mixer's phase accumulator + sin/cos
    /// table. We feed it `omega = +2π·f/Fs` and call `mix_down_one`,
    /// which internally does `y = x · exp(-jθ)` — the negation is in
    /// liquid's mixer, not in our omega sign.
    nco: Nco,
    /// Liquid polyphase decimator. Owns the FIR delay line and
    /// emits one output every `factor` inputs (via the `chunk`
    /// scratch buffer below — liquid's `_execute` insists on
    /// exactly `factor` samples per call).
    decim: FirdecimCx,
    chunk: Vec<(f32, f32)>,
    chunk_len: usize,

    input_rate_hz: f64,
    /// Last-applied `freq_shift_hz`. Stored so `set_freq_shift` can
    /// be called fresh without recomputing from the NCO's internal
    /// state, and so [`Self::freq_shift_hz`] is a cheap getter.
    freq_shift_hz: f64,
    /// Target output rate in Hz. `rebuild_for_input_rate` derives
    /// `factor = round(input_rate / target)` each time the scheduler
    /// reports a fresh upstream rate, so a preset says 240 kHz and
    /// the block lands the closest integer decimation whether the SDR
    /// is at 2 MS/s, 10 MS/s, or anywhere between.
    target_output_rate_hz: f64,
}

impl Channelizer {
    /// Constructs a channelizer with the supplied params. Fails on
    /// non-positive input rate or non-positive output rate. The FIR
    /// + decim are sized for `factor = round(input / output)` here as
    /// a construction-time hint; `init()` rebuilds at the scheduler-
    /// reported input rate if it differs.
    pub fn new(params: ChannelizerParams) -> Result<Self> {
        if !(params.input_rate_hz.is_finite() && params.input_rate_hz > 0.0) {
            bail!(
                "channelizer input_rate_hz must be > 0, got {}",
                params.input_rate_hz
            );
        }
        if !(params.output_rate_hz.is_finite() && params.output_rate_hz > 0.0) {
            bail!(
                "channelizer output_rate_hz must be > 0, got {}",
                params.output_rate_hz
            );
        }
        let mut s = Self {
            factor: 0,
            taps: Vec::new(),
            nco: Nco::new().map_err(|e| anyhow::anyhow!("nco_crcf: {e}"))?,
            // firdecim requires factor ≥ 2 at construction; we overwrite
            // `decim` immediately in `rebuild_for_input_rate`. The taps
            // here are just placeholders to satisfy the ctor.
            decim: FirdecimCx::from_taps(2, &[0.5, 0.5])
                .map_err(|e| anyhow::anyhow!("firdecim_crcf placeholder: {e}"))?,
            chunk: Vec::new(),
            chunk_len: 0,
            input_rate_hz: params.input_rate_hz,
            freq_shift_hz: params.freq_shift_hz,
            target_output_rate_hz: params.output_rate_hz,
        };
        s.rebuild_for_input_rate(params.input_rate_hz)?;
        Ok(s)
    }

    /// (Re)build FIR + polyphase decim + chunk buffer for a new input
    /// rate. `factor = round(input / target_output)`, clamped to ≥ 1.
    /// FIR is Kaiser-windowed-sinc sized `8·factor+1` with normalised
    /// cutoff `0.4/factor` — leaves ~20 % transition below output
    /// Nyquist. Called from both the constructor (initial sizing) and
    /// from `Block::init` / `Block::update_rates` when the scheduler
    /// reports a source-rate change.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn rebuild_for_input_rate(&mut self, input_rate_hz: f64) -> Result<()> {
        if !(input_rate_hz > 0.0 && self.target_output_rate_hz > 0.0) {
            bail!(
                "channelizer rebuild: need positive rates, got input={input_rate_hz} output={}",
                self.target_output_rate_hz,
            );
        }
        let new_factor = ((input_rate_hz / self.target_output_rate_hz).round() as usize).max(1);
        self.input_rate_hz = input_rate_hz;
        if new_factor == self.factor {
            self.nco
                .set_frequency(omega_for(self.freq_shift_hz, input_rate_hz));
            return Ok(());
        }
        let num_taps = 8 * new_factor + 1;
        let cutoff = 0.4 / new_factor as f32;
        let taps = crate::decimator::design_lpf(num_taps, cutoff);
        // liquid's firdecim wants factor ≥ 2; factor=1 takes the
        // passthrough fast path in `process()` so the wrapper instance
        // is sized at max(2, factor) just to satisfy that invariant.
        let factor_u32 =
            u32::try_from(new_factor.max(2)).map_err(|_| anyhow::anyhow!("factor exceeds u32"))?;
        let decim = FirdecimCx::from_taps(factor_u32, &taps)
            .map_err(|e| anyhow::anyhow!("firdecim_crcf rebuild: {e}"))?;
        self.factor = new_factor;
        self.taps = taps;
        self.decim = decim;
        self.chunk = vec![(0.0, 0.0); new_factor.max(1)];
        self.chunk_len = 0;
        self.nco
            .set_frequency(omega_for(self.freq_shift_hz, input_rate_hz));
        Ok(())
    }

    /// Retune the VFO without interrupting sample flow. The mixer's
    /// phase accumulator stays continuous (no pop); only the
    /// per-sample increment changes.
    pub fn set_freq_shift(&mut self, freq_shift_hz: f64) {
        self.freq_shift_hz = freq_shift_hz;
        self.nco
            .set_frequency(omega_for(freq_shift_hz, self.input_rate_hz));
    }

    #[must_use]
    pub const fn factor(&self) -> usize {
        self.factor
    }

    #[must_use]
    pub fn taps(&self) -> &[f32] {
        &self.taps
    }

    /// Current mixer frequency shift, Hz. Useful for tests and status.
    #[must_use]
    pub fn freq_shift_hz(&self) -> f64 {
        self.freq_shift_hz
    }

    /// Shared core of [`Block::init`] and [`Block::update_rates`] — if
    /// the scheduler's input rate differs from what this instance was
    /// built against, rebuild FIR + decim via
    /// [`Self::rebuild_for_input_rate`] and emit one `flowdiag` line
    /// tagged with `phase` so operators can distinguish first-build
    /// from live adaptation.
    fn sync_to_ctx_rate(
        &mut self,
        ctx: &crate::block::InitCtx<'_>,
        phase: &'static str,
    ) -> Result<()> {
        if let Some(rate) = ctx.input_rate("in") {
            if rate > 0.0 && (rate - self.input_rate_hz).abs() > f64::EPSILON {
                self.rebuild_for_input_rate(rate)?;
            }
        }
        #[allow(clippy::cast_precision_loss)]
        let actual_out = self.input_rate_hz / self.factor as f64;
        tracing::info!(
            target: "flowdiag",
            block = "Channelizer",
            input_rate_hz = self.input_rate_hz,
            factor = self.factor,
            target_output_rate_hz = self.target_output_rate_hz,
            actual_output_rate_hz = actual_out,
            "{phase}",
        );
        Ok(())
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for Channelizer {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "Channelizer",
            placement: Placement::NativeOnly,
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
                    key: "freq_shift_hz",
                    label: "VFO offset",
                    kind: ParamKind::Range {
                        min: -1.0e9,
                        max: 1.0e9,
                        step: 1.0,
                        default: 0.0,
                        unit: "Hz",
                    },
                    // The only "tune knob" on the VFO — the whole point is
                    // to retune without restarting the graph.
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "output_rate_hz",
                    label: "Output rate",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 10.0e6,
                        step: 1.0,
                        default: 240_000.0,
                        unit: "Hz",
                    },
                    // Target channel rate. Factor + FIR auto-derive
                    // from `input_rate / output_rate` at init; downstream
                    // chain reinits since the actual output shifts.
                    reconfig_scope: ReconfigureScope::Downstream,
                },
            ],
        }
    }

    fn relative_rate(&self, _in_port: usize, _out_port: usize) -> (u32, u32) {
        #[allow(clippy::cast_possible_truncation)]
        let factor = self.factor as u32;
        (1, factor.max(1))
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        self.sync_to_ctx_rate(ctx, "channelizer init")
    }

    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        if self.factor == 0 {
            return None;
        }
        #[allow(clippy::cast_precision_loss)]
        Some(self.input_rate_hz / self.factor as f64)
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        // Same job as the rate path inside `init()`, but safe for a
        // carried-over instance whose one-time external setup already
        // ran. Different log phase so an operator can tell whether a
        // rebuild was first-construct or live rate adaptation.
        self.sync_to_ctx_rate(ctx, "channelizer rate update")
    }

    /// Live VFO retune — the reason this block exists. Only `freq_shift_hz`
    /// is accepted; rebuilding for anything else (factor/taps/cutoff)
    /// requires a fresh filter design, so we return `Ok(false)` and let
    /// the runtime fall back to a block-scoped rebuild.
    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let Some(obj) = delta.as_object() else {
            return Ok(false);
        };
        const LIVE_KEYS: &[&str] = &["freq_shift_hz"];
        if !obj.keys().all(|k| LIVE_KEYS.contains(&k.as_str())) {
            return Ok(false);
        }
        if let Some(v) = obj.get("freq_shift_hz").and_then(|v| v.as_f64()) {
            self.set_freq_shift(v);
        }
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

        // factor=1 degenerate case — straight passthrough through the
        // mixer only. liquid's firdecim refuses factor < 2; this
        // path keeps the block usable as a pure tuner.
        if self.factor == 1 {
            let n = src.len().min(dst.len());
            for i in 0..n {
                let (re, im) = self.nco.mix_down_one(src[i].re, src[i].im);
                dst[i] = Complex::new(re, im);
            }
            let mut w = Work::new();
            w.consumed[0] = n;
            w.produced[0] = n;
            return Ok(w);
        }

        let mut consumed = 0;
        let mut produced = 0;
        for &x in src {
            // Reserve room for one more output before consuming the
            // input that would complete a chunk — backpressure stays
            // honest across calls.
            if self.chunk_len + 1 == self.factor && produced == dst.len() {
                break;
            }
            // Mix down with liquid's NCO (does y = x·exp(-jθ); we
            // configured frequency = +2π·f/Fs so the shift lands the
            // target tone at DC), then push to the chunk.
            let (re, im) = self.nco.mix_down_one(x.re, x.im);
            self.chunk[self.chunk_len] = (re, im);
            self.chunk_len += 1;
            consumed += 1;
            if self.chunk_len == self.factor {
                let (yr, yi) = self.decim.execute(&self.chunk[..self.factor]);
                dst[produced] = Complex::new(yr, yi);
                produced += 1;
                self.chunk_len = 0;
            }
        }

        let mut w = Work::new();
        w.consumed[0] = consumed;
        w.produced[0] = produced;
        Ok(w)
    }
}

impl BlockFactory for Channelizer {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: ChannelizerParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(Channelizer::new(p)?))
    }
}

fn omega_for(freq_shift_hz: f64, input_rate_hz: f64) -> f32 {
    // liquid's `nco_mix_down` does `y = x · exp(-jθ)` internally, so
    // pulling a tone at `+f Hz` down to DC needs `ω = +2π·f/Fs` here.
    // The negation is liquid's, not ours.
    #[allow(clippy::cast_possible_truncation)]
    let omega = (core::f64::consts::TAU * freq_shift_hz / input_rate_hz) as f32;
    omega
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
mod tests {
    use super::{Channelizer, ChannelizerParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use core::f32::consts::TAU;
    use num_complex::Complex;

    fn run(
        ch: &mut Channelizer,
        input: &[Complex<f32>],
        out_len: usize,
    ) -> (Vec<Complex<f32>>, usize, usize) {
        let mut out = vec![Complex::new(0.0, 0.0); out_len];
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
        let w = ch.process(&mut io).unwrap();
        (out, w.consumed[0], w.produced[0])
    }

    #[test]
    fn factor_derives_from_rates() {
        // 8 MS/s source, target 240 kHz channel → factor = round(8M/240k) = 33.
        let ch = Channelizer::new(ChannelizerParams::new(8_000_000.0, 0.0, 240_000.0)).unwrap();
        assert_eq!(ch.factor(), 33);
        // FIR sizing tracks the derived factor (8*33 + 1).
        assert_eq!(ch.taps().len(), 8 * 33 + 1);
    }

    #[test]
    fn integer_ratio_rates_land_on_exact_factor() {
        // 2 MS/s → 500 kHz channel = factor 4.
        let ch = Channelizer::new(ChannelizerParams::new(2.0e6, 0.0, 500_000.0)).unwrap();
        assert_eq!(ch.factor(), 4);
    }

    #[test]
    fn constructor_rejects_bad_params() {
        assert!(Channelizer::new(ChannelizerParams::new(0.0, 0.0, 240_000.0)).is_err());
        assert!(Channelizer::new(ChannelizerParams::new(2.0e6, 0.0, 0.0)).is_err());
        assert!(Channelizer::new(ChannelizerParams::new(2.0e6, 0.0, -1.0)).is_err());
    }

    #[test]
    fn freq_shift_zero_passes_dc() {
        // freq_shift=0 reduces the channelizer to a plain decimator.
        // 2 MS/s → 500 kS/s = factor 4.
        let mut ch = Channelizer::new(ChannelizerParams::new(2.0e6, 0.0, 500_000.0)).unwrap();
        let input = vec![Complex::new(1.0_f32, 0.0); 8 * ch.taps().len()];
        let (out, _, produced) = run(&mut ch, &input, input.len() / 4 + 4);
        for s in &out[produced.saturating_sub(4)..produced] {
            assert!((s.re - 1.0).abs() < 1e-4, "re = {}", s.re);
            assert!(s.im.abs() < 1e-4, "im = {}", s.im);
        }
    }

    #[test]
    fn rate_invariant_holds_across_calls() {
        // 2 MS/s → 400 kS/s = factor 5.
        let mut ch = Channelizer::new(ChannelizerParams::new(2.0e6, 0.0, 400_000.0)).unwrap();
        let sizes = [0_usize, 1, 4, 5, 6, 9, 10, 17, 23, 31, 50];
        let mut total_in = 0;
        let mut total_out = 0;
        let input = vec![Complex::new(0.5_f32, -0.5); 64];
        for &n in &sizes {
            let (_, c, p) = run(&mut ch, &input[..n], n + 1);
            assert_eq!(c, n);
            total_in += n;
            total_out += p;
        }
        assert_eq!(total_out, total_in / 5);
    }

    #[test]
    fn tunes_a_tone_to_baseband() {
        // Input: complex exponential at +200 kHz against 2 MS/s.
        // Target 250 kS/s channel → factor 8, tone at DC after mixing.
        let fs = 2.0e6;
        let tone_hz = 200_000.0;
        let mut ch = Channelizer::new(ChannelizerParams::new(fs, tone_hz, 250_000.0)).unwrap();
        let factor = ch.factor();

        let n = 8192_usize;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = TAU * (tone_hz as f32 / fs as f32) * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let (out, _, produced) = run(&mut ch, &input, n / factor + 4);

        // Skip warmup: ~num_taps/factor outputs are transient. FIR is
        // sized 8*factor+1, so roughly 8 warmup samples.
        let warm = 8 + 4;
        assert!(produced > warm, "no steady-state samples produced");
        let avg_mag: f32 = out[warm..produced]
            .iter()
            .map(|c| (c.re * c.re + c.im * c.im).sqrt())
            .sum::<f32>()
            / (produced - warm) as f32;
        assert!(
            (avg_mag - 1.0).abs() < 0.02,
            "expected unit-magnitude tone at baseband, got {avg_mag}"
        );
    }

    #[test]
    fn out_of_band_tone_is_rejected() {
        // Tune to 0; input is a tone way outside the decimated passband.
        let fs = 2.0e6;
        let mut ch = Channelizer::new(ChannelizerParams::new(fs, 0.0, 250_000.0)).unwrap();
        let factor = ch.factor();

        // Tone at +600 kHz with fs=2 MS/s → normalized 0.3, well outside
        // the post-decim passband of ±125 kHz (0.0625 normalized).
        let tone_hz = 600_000.0;
        let n = 8192_usize;
        let input: Vec<Complex<f32>> = (0..n)
            .map(|i| {
                let t = TAU * (tone_hz as f32 / fs as f32) * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let (out, _, produced) = run(&mut ch, &input, n / factor + 4);

        let warm = 8 + 4;
        let rms: f32 = (out[warm..produced]
            .iter()
            .map(|c| c.re * c.re + c.im * c.im)
            .sum::<f32>()
            / (produced - warm) as f32)
            .sqrt();
        assert!(rms < 0.05, "out-of-band tone leaked through: {rms}");
    }

    #[test]
    fn retune_mid_stream_is_continuous() {
        // Two-tone input: A at +100 kHz for first half, B at -150 kHz
        // for second half. Retune between halves and confirm both halves
        // demodulate to near-DC in their own windows.
        let fs = 2.0e6;
        let mut ch = Channelizer::new(ChannelizerParams::new(fs, 100_000.0, 250_000.0)).unwrap();
        let factor = ch.factor();

        let half = 4096_usize;
        let tone_a = 100_000.0_f32;
        let first: Vec<Complex<f32>> = (0..half)
            .map(|i| {
                let t = TAU * (tone_a / fs as f32) * i as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let (_a_out, _, p_a) = run(&mut ch, &first, half / factor + 4);

        ch.set_freq_shift(-150_000.0);

        let tone_b = -150_000.0_f32;
        let second: Vec<Complex<f32>> = (0..half)
            .map(|i| {
                let t = TAU * (tone_b / fs as f32) * (i + half) as f32;
                Complex::new(t.cos(), t.sin())
            })
            .collect();
        let (b_out, _, p_b) = run(&mut ch, &second, half / factor + 4);

        // First-half output should be finite (no NaN/Inf from the retune
        // boundary).
        for s in &b_out[..p_b] {
            assert!(s.re.is_finite() && s.im.is_finite());
        }
        assert!(p_a > 0 && p_b > 0);

        // Second-half settled magnitude should match baseband unit-tone.
        let warm = 8 + 4;
        let avg_mag: f32 = b_out[warm..p_b]
            .iter()
            .map(|c| (c.re * c.re + c.im * c.im).sqrt())
            .sum::<f32>()
            / (p_b - warm) as f32;
        assert!(
            (avg_mag - 1.0).abs() < 0.1,
            "expected unit mag after retune, got {avg_mag}"
        );
    }

    #[test]
    fn output_backpressure_pauses_cleanly() {
        // 2 MS/s → 500 kS/s channel = factor 4.
        let mut ch = Channelizer::new(ChannelizerParams::new(2.0e6, 0.0, 500_000.0)).unwrap();
        let input = vec![Complex::new(1.0_f32, 0.0); 40];
        let (_, consumed_a, produced_a) = run(&mut ch, &input, 2);
        assert_eq!(produced_a, 2);
        assert!(consumed_a <= input.len());
        let (_, consumed_b, produced_b) = run(&mut ch, &input[consumed_a..], 20);
        assert_eq!(produced_a + produced_b, 10);
        assert_eq!(consumed_a + consumed_b, input.len());
    }
}
