//! Safe Rust wrappers around vendored liquid-dsp.
//!
//! Each primitive we expose owns its underlying C handle via RAII —
//! the wrapper's `Drop` calls the matching `_destroy` so blocks don't
//! have to reach for unsafe in the common path.
//!
//! Today's surface: real + complex polyphase decimators (`Firdecim`,
//! `FirdecimCx`), an NCO-driven complex mixer (`Nco`), the analog AM/
//! SSB modem (`Ampmodem`), the FM discriminator (`FreqDem`), and the
//! multi-stage fractional resampler (`MsResamp`). New primitives get
//! added here as blocks need them.

#![allow(unsafe_op_in_unsafe_fn)]

mod sys {
    //! Raw bindgen output from `vendor/include/liquid.h`. Hidden so
    //! callers reach for the safe wrappers above; `pub(crate)` keeps
    //! it accessible from this crate's tests.
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(non_upper_case_globals)]
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/liquid_bindings.rs"));
}

/// AM/SSB modulation type passed to [`Ampmodem::new`]. Mirrors
/// liquid's `liquid_ampmodem_type` enum 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AmType {
    /// Double sideband (envelope detection).
    Dsb,
    /// Upper sideband.
    Usb,
    /// Lower sideband.
    Lsb,
}

/// AM/SSB demodulator — wraps liquid's `ampmodem`. Consumes one
/// complex baseband IQ sample per call, returns one real audio
/// sample at the **same rate**. liquid handles the full chain
/// internally: a Remez-designed Hilbert filter (`firhilbf`), a
/// DC blocker (`firfilt_rrrf_create_dc_blocker`), a complex LPF
/// for the optional carrier-recovery PLL, and the sideband
/// selection arithmetic — all for ~470 LOC of C we don't have to
/// write or maintain.
///
/// Why wrap a "large block" instead of just `firhilbf`: liquid's
/// `firhilbf` is a half-band filter that does only 2× rate change
/// (interp / decim). Same-rate complex-IQ → real-audio for SSB
/// requires the full Hartley-method assembly that `ampmodem` already
/// embodies. The image rejection comes from `firhilbf`'s internal
/// Remez design, which is meaningfully better than the windowed-
/// sinc Hamming filter SsbDemod previously hand-rolled (~24 dB).
pub struct Ampmodem {
    inner: sys::ampmodem,
}

impl Ampmodem {
    /// Build a new ampmodem.
    ///
    /// - `mod_index`: AM modulation index (the depth of the original
    ///   transmit modulation). For SSB demod the value mostly affects
    ///   the carrier-recovery PLL bandwidth; 0.5–1.0 is fine for
    ///   amateur voice. liquid's example uses 0.8.
    /// - `kind`: USB / LSB / DSB.
    /// - `suppressed_carrier`: `true` for typical SSB amateur signals
    ///   (no carrier transmitted); `false` for AM broadcast where the
    ///   carrier is present.
    pub fn new(
        mod_index: f32,
        kind: AmType,
        suppressed_carrier: bool,
    ) -> Result<Self, &'static str> {
        if !(mod_index.is_finite() && mod_index > 0.0) {
            return Err("ampmodem: mod_index must be > 0");
        }
        let liquid_type = match kind {
            AmType::Dsb => sys::liquid_ampmodem_type_LIQUID_AMPMODEM_DSB,
            AmType::Usb => sys::liquid_ampmodem_type_LIQUID_AMPMODEM_USB,
            AmType::Lsb => sys::liquid_ampmodem_type_LIQUID_AMPMODEM_LSB,
        };
        // SAFETY: parameters validated above; liquid returns NULL on
        // bad config which we map to Err so callers never see a bad
        // handle.
        let inner =
            unsafe { sys::ampmodem_create(mod_index, liquid_type, i32::from(suppressed_carrier)) };
        if inner.is_null() {
            return Err("ampmodem_create returned NULL");
        }
        Ok(Self { inner })
    }

    /// Demodulate one complex IQ sample → one real audio sample.
    pub fn demodulate(&mut self, re: f32, im: f32) -> f32 {
        let x = sys::__BindgenComplex { re, im };
        let mut y: f32 = 0.0;
        // SAFETY: `inner` is a valid handle; `y` is a writable f32
        // stack slot.
        unsafe {
            sys::ampmodem_demodulate(self.inner, x, &mut y);
        }
        y
    }

    /// Group delay introduced by the internal filter chain
    /// (Hilbert + DC blocker + LPF). Useful when aligning the
    /// demodulator output against another path that wasn't
    /// delayed.
    #[must_use]
    pub fn delay(&self) -> u32 {
        // SAFETY: `inner` is a valid handle.
        unsafe { sys::ampmodem_get_delay_demod(self.inner) }
    }

    /// Reset internal state (Hilbert delay line, DC blocker, PLL).
    pub fn reset(&mut self) {
        // SAFETY: `inner` is a valid handle.
        unsafe {
            sys::ampmodem_reset(self.inner);
        }
    }
}

impl Drop for Ampmodem {
    fn drop(&mut self) {
        // SAFETY: `inner` was returned by `ampmodem_create` and
        // hasn't been destroyed; `Drop` runs at most once.
        unsafe {
            if !self.inner.is_null() {
                sys::ampmodem_destroy(self.inner);
            }
        }
    }
}

// SAFETY: liquid's objects are single-threaded by design but have
// no thread affinity — moving the handle between threads is fine
// as long as we don't share access concurrently. `Send` (move) is
// safe; `Sync` (shared reference) is intentionally not implemented.
// Required for ferrite-blocks's `Block: Send` bound to be satisfied
// by blocks that hold an Ampmodem.
unsafe impl Send for Ampmodem {}

/// Real-valued integer-rate decimating FIR filter — wraps liquid's
/// `firdecim_rrrf`. Consumes `factor` input samples per call to
/// [`Self::execute`] and produces one filtered output. Equivalent to
/// running a real FIR at the input rate then keeping every
/// `factor`-th sample, but liquid's polyphase decomposition does
/// the work at the output rate.
pub struct Firdecim {
    inner: sys::firdecim_rrrf,
    factor: u32,
}

impl Firdecim {
    /// Build from explicit FIR taps. `factor >= 2`, `taps.len() >=
    /// factor`. The caller is responsible for the filter design (we
    /// just convolve + decimate); a windowed-sinc with cutoff
    /// ≤ 1/(2·factor) of the input rate is the typical choice.
    pub fn from_taps(factor: u32, taps: &[f32]) -> Result<Self, &'static str> {
        if factor < 2 {
            return Err("firdecim: factor must be >= 2");
        }
        let h_len = u32::try_from(taps.len()).map_err(|_| "firdecim: too many taps")?;
        if h_len < factor {
            return Err("firdecim: taps.len() must be >= factor");
        }
        // SAFETY: liquid copies the tap buffer internally; NULL on
        // bad config is mapped to Err.
        let inner = unsafe { sys::firdecim_rrrf_create(factor, taps.as_ptr() as *mut f32, h_len) };
        if inner.is_null() {
            return Err("firdecim_rrrf_create returned NULL");
        }
        Ok(Self { inner, factor })
    }

    /// Decimation factor passed at construction.
    #[must_use]
    pub const fn factor(&self) -> u32 {
        self.factor
    }

    /// Push `factor` samples in, read one filtered sample out.
    /// `input.len()` must equal `factor`.
    pub fn execute(&mut self, input: &[f32]) -> f32 {
        assert_eq!(
            input.len() as u32,
            self.factor,
            "firdecim::execute: input slice must be exactly `factor` samples"
        );
        let mut y: f32 = 0.0;
        // SAFETY: `inner` is valid; input is `factor` floats; y is a
        // writable f32 stack slot.
        unsafe {
            sys::firdecim_rrrf_execute(self.inner, input.as_ptr() as *mut f32, &mut y);
        }
        y
    }

    /// Reset filter state (zeros the delay line + decimation phase).
    pub fn reset(&mut self) {
        // SAFETY: `inner` is valid.
        unsafe {
            sys::firdecim_rrrf_reset(self.inner);
        }
    }
}

impl Drop for Firdecim {
    fn drop(&mut self) {
        // SAFETY: `inner` returned by `firdecim_rrrf_create`, dropped
        // at most once.
        unsafe {
            if !self.inner.is_null() {
                sys::firdecim_rrrf_destroy(self.inner);
            }
        }
    }
}

// SAFETY: liquid handles are single-threaded but movable.
unsafe impl Send for Firdecim {}

/// Complex-input/output integer-rate decimating FIR filter — wraps
/// liquid's `firdecim_crcf` (complex input, complex output, real
/// taps). Same polyphase semantics as [`Firdecim`].
pub struct FirdecimCx {
    inner: sys::firdecim_crcf,
    factor: u32,
}

impl FirdecimCx {
    /// Build from real FIR taps. Same constraints as [`Firdecim::from_taps`].
    /// `firdecim_crcf` uses real taps applied to complex samples
    /// (`re` and `im` filtered with the same coefficients).
    pub fn from_taps(factor: u32, taps: &[f32]) -> Result<Self, &'static str> {
        if factor < 2 {
            return Err("firdecim_cx: factor must be >= 2");
        }
        let h_len = u32::try_from(taps.len()).map_err(|_| "firdecim_cx: too many taps")?;
        if h_len < factor {
            return Err("firdecim_cx: taps.len() must be >= factor");
        }
        // SAFETY: liquid copies the tap buffer.
        let inner = unsafe { sys::firdecim_crcf_create(factor, taps.as_ptr() as *mut f32, h_len) };
        if inner.is_null() {
            return Err("firdecim_crcf_create returned NULL");
        }
        Ok(Self { inner, factor })
    }

    #[must_use]
    pub const fn factor(&self) -> u32 {
        self.factor
    }

    /// Push `factor` complex samples in, read one filtered complex
    /// out. `input` is interleaved Complex<f32> as bindgen sees it
    /// (`__BindgenComplex<f32>`); we expose it as `(re, im)` pairs
    /// per output to keep the wrapper agnostic of `num_complex`.
    /// Returns `(re, im)`.
    pub fn execute(&mut self, input: &[(f32, f32)]) -> (f32, f32) {
        assert_eq!(
            input.len() as u32,
            self.factor,
            "firdecim_cx::execute: input slice must be exactly `factor` samples"
        );
        // Repackage `(re, im)` into `__BindgenComplex<f32>`. The
        // memory layout is identical (two consecutive f32s); we
        // could transmute, but a small stack-allocated copy stays
        // miri-friendly and the cost vanishes for typical factor
        // (2..16).
        //
        // For larger blocks the caller can switch to `execute_block`
        // (not exposed here yet) which avoids per-sample marshalling.
        let mut buf: Vec<sys::__BindgenComplex<f32>> = input
            .iter()
            .map(|&(re, im)| sys::__BindgenComplex { re, im })
            .collect();
        let mut y = sys::__BindgenComplex { re: 0.0, im: 0.0 };
        // SAFETY: `inner` is valid; `buf` has `factor` complex slots;
        // `y` is a writable __BindgenComplex stack slot.
        unsafe {
            sys::firdecim_crcf_execute(self.inner, buf.as_mut_ptr(), &mut y);
        }
        (y.re, y.im)
    }

    /// Reset filter state.
    pub fn reset(&mut self) {
        // SAFETY: `inner` is valid.
        unsafe {
            sys::firdecim_crcf_reset(self.inner);
        }
    }
}

impl Drop for FirdecimCx {
    fn drop(&mut self) {
        // SAFETY: `inner` returned by `firdecim_crcf_create`, dropped
        // at most once.
        unsafe {
            if !self.inner.is_null() {
                sys::firdecim_crcf_destroy(self.inner);
            }
        }
    }
}

// SAFETY: liquid handles are single-threaded but movable.
unsafe impl Send for FirdecimCx {}

/// Numerically-controlled oscillator — wraps liquid's `nco_crcf`.
/// Maintains a phase accumulator + sin/cos lookup; consumes complex
/// IQ samples and rotates them by the current phase, with optional
/// PLL helpers we don't expose yet.
///
/// Two backends: `LIQUID_NCO` is a 1024-entry lookup table with
/// linear interpolation (fast, deterministic, no SIMD), and
/// `LIQUID_VCO` computes sin/cos directly each sample (more
/// accurate, slightly slower). We default to `LIQUID_NCO` because
/// the channelizer mixer is on the per-sample hot path and table
/// precision is fine at typical SDR rates.
pub struct Nco {
    inner: sys::nco_crcf,
}

impl Nco {
    /// Build a new NCO. `LIQUID_NCO` (table-based) is the right default
    /// for mixer use — millions of samples/sec, no need for the
    /// extra precision the direct-sin/cos `VCO` flavour gives.
    pub fn new() -> Result<Self, &'static str> {
        // SAFETY: `nco_crcf_create` allocates internally; NULL on
        // failure (we treat that as Err).
        let inner = unsafe { sys::nco_crcf_create(sys::liquid_ncotype_LIQUID_NCO) };
        if inner.is_null() {
            return Err("nco_crcf_create returned NULL");
        }
        Ok(Self { inner })
    }

    /// Set the per-sample phase increment in radians. The sign
    /// convention pairs with the chosen `mix_*_one` direction:
    ///
    /// - To pull an input tone at `+f Hz` down to baseband via
    ///   [`Self::mix_down_one`] (which itself does `y = x·exp(-jθ)`),
    ///   set `omega = +2π·f/Fs`. Positive frequency in, negative
    ///   rotation out — they cancel.
    /// - To shift a baseband signal up by `+f Hz` via a future
    ///   `mix_up_one`, the same `omega = +2π·f/Fs` would push the
    ///   spectrum up.
    pub fn set_frequency(&mut self, omega: f32) {
        // SAFETY: `inner` is a valid handle.
        unsafe {
            sys::nco_crcf_set_frequency(self.inner, omega);
        }
    }

    /// Get the current per-sample phase increment.
    #[must_use]
    pub fn frequency(&self) -> f32 {
        // SAFETY: `inner` is a valid handle.
        unsafe { sys::nco_crcf_get_frequency(self.inner) }
    }

    /// Set the absolute phase (radians). Useful for re-syncing two
    /// NCOs or driving from an external phase estimator. Independent
    /// of frequency.
    pub fn set_phase(&mut self, phi: f32) {
        // SAFETY: `inner` is a valid handle.
        unsafe {
            sys::nco_crcf_set_phase(self.inner, phi);
        }
    }

    /// Mix one IQ sample down by the current NCO phase, then advance.
    /// Equivalent to `y = x · exp(-jθ)`. Returns `(re, im)`.
    pub fn mix_down_one(&mut self, re: f32, im: f32) -> (f32, f32) {
        let x = sys::__BindgenComplex { re, im };
        let mut y = sys::__BindgenComplex { re: 0.0, im: 0.0 };
        // SAFETY: `inner` is valid; `y` is a writable stack slot.
        // `nco_crcf_mix_down` rotates by current phase, writes y,
        // then steps the phase by the configured frequency.
        unsafe {
            sys::nco_crcf_mix_down(self.inner, x, &mut y);
            sys::nco_crcf_step(self.inner);
        }
        (y.re, y.im)
    }

    /// Advance the NCO phase by one sample without mixing anything.
    /// Used inside PLL loops after `pll_step` has already updated the
    /// phase correction.
    pub fn step(&mut self) {
        // SAFETY: `inner` is a valid handle.
        unsafe {
            sys::nco_crcf_step(self.inner);
        }
    }

    /// Configure the internal PLL loop bandwidth. Larger values track
    /// faster (at the cost of more jitter); smaller values are quieter
    /// but slower to acquire.
    pub fn pll_set_bandwidth(&mut self, bw: f32) {
        // SAFETY: `inner` is a valid handle.
        unsafe {
            sys::nco_crcf_pll_set_bandwidth(self.inner, bw);
        }
    }

    /// Feed one phase-error measurement (radians) to the PLL. Liquid
    /// adjusts the NCO's phase and frequency proportionally to the
    /// bandwidth set via [`Self::pll_set_bandwidth`].
    pub fn pll_step(&mut self, phase_error: f32) {
        // SAFETY: `inner` is a valid handle.
        unsafe {
            sys::nco_crcf_pll_step(self.inner, phase_error);
        }
    }

    /// Current complex exponential output at the NCO's phase
    /// (`cos θ + j sin θ`). Does NOT advance the phase.
    pub fn cexpf(&mut self) -> (f32, f32) {
        let mut y = sys::__BindgenComplex { re: 0.0, im: 0.0 };
        // SAFETY: `inner` is valid; `y` is a writable stack slot.
        unsafe {
            sys::nco_crcf_cexpf(self.inner, &mut y);
        }
        (y.re, y.im)
    }

    /// Current phase of the NCO in radians. Useful for deriving a
    /// harmonic (e.g. doubling a locked pilot phase for a 2× reference).
    /// Does NOT advance the phase.
    #[must_use]
    pub fn phase(&self) -> f32 {
        // SAFETY: `inner` is a valid handle.
        unsafe { sys::nco_crcf_get_phase(self.inner) }
    }

    /// Reset phase to zero (frequency unchanged).
    pub fn reset(&mut self) {
        // SAFETY: `inner` is a valid handle.
        unsafe {
            sys::nco_crcf_reset(self.inner);
        }
    }
}

impl Drop for Nco {
    fn drop(&mut self) {
        // SAFETY: `inner` returned by `nco_crcf_create`, dropped at
        // most once.
        unsafe {
            if !self.inner.is_null() {
                sys::nco_crcf_destroy(self.inner);
            }
        }
    }
}

// SAFETY: liquid handles are single-threaded but movable.
unsafe impl Send for Nco {}

/// Multi-stage fractional resampler for real-float samples. Wraps
/// liquid's `msresamp_rrrf`, a cascade of half-band stages followed by
/// an arbitrary-rate Farrow interpolator. Handles any output:input
/// ratio — use this when an integer decimator/interpolator can't close
/// the gap (e.g. 200 kHz → 48 kHz audio, ratio 0.24).
///
/// Liquid writes a *variable* number of output samples per execute
/// call; callers allocate `⌈1 + 2·r·nx⌉` output slots to be safe.
/// [`Self::max_output_for`] does the sizing.
pub struct MsResamp {
    inner: sys::msresamp_rrrf,
    rate: f32,
}

impl MsResamp {
    /// Create a resampler at the given rate = output/input (e.g. `0.24`
    /// for 200k → 48k). `stopband_db` is stop-band attenuation for the
    /// internal filters; 60 dB is a safe default for audio.
    pub fn new(rate: f32, stopband_db: f32) -> Result<Self, &'static str> {
        if !(rate > 0.0 && rate.is_finite()) {
            return Err("msresamp: rate must be > 0");
        }
        if !(stopband_db > 0.0 && stopband_db.is_finite()) {
            return Err("msresamp: stopband_db must be > 0");
        }
        // SAFETY: scalar args, liquid returns NULL on internal alloc
        // failure or bad config which we surface as Err.
        let inner = unsafe { sys::msresamp_rrrf_create(rate, stopband_db) };
        if inner.is_null() {
            return Err("msresamp_rrrf_create returned NULL");
        }
        Ok(Self { inner, rate })
    }

    /// Execute one batch. Returns the number of output samples written
    /// into `output`. Caller must size `output` at
    /// `max_output_for(input.len())` to avoid an overflow assertion
    /// from liquid.
    pub fn execute(&mut self, input: &[f32], output: &mut [f32]) -> usize {
        if input.is_empty() {
            return 0;
        }
        let max_out = self.max_output_for(input.len());
        let out_cap = output.len().min(max_out);
        let mut ny: std::os::raw::c_uint = 0;
        // SAFETY: `inner` valid until Drop; liquid reads `input.len()`
        // f32s from `input`, writes at most `max_out` f32s into
        // `output`. `ny` out-parameter is the actual count.
        unsafe {
            sys::msresamp_rrrf_execute(
                self.inner,
                input.as_ptr() as *mut f32,
                input.len() as std::os::raw::c_uint,
                output.as_mut_ptr(),
                &mut ny,
            );
        }
        let written = ny as usize;
        // Defensive clamp: liquid shouldn't exceed the cap but the API
        // returns a raw count so keep the slice honest.
        written.min(out_cap)
    }

    /// Upper bound on output samples for a given input count. Use to
    /// size the `output` slice before `execute`.
    #[must_use]
    pub fn max_output_for(&self, input_len: usize) -> usize {
        // The header comment says ⌈1 + 2·r·nx⌉ is safe; we add a small
        // +8 margin to absorb liquid's occasional one-off surplus at
        // state transitions. Cheap; this is an allocation size, not a
        // per-sample path.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let bound = (1.0 + 2.0 * self.rate as f64 * input_len as f64).ceil() as usize + 8;
        bound
    }

    /// Configured output/input rate.
    #[must_use]
    pub const fn rate(&self) -> f32 {
        self.rate
    }

    pub fn reset(&mut self) {
        // SAFETY: `inner` valid until Drop.
        unsafe {
            sys::msresamp_rrrf_reset(self.inner);
        }
    }
}

impl Drop for MsResamp {
    fn drop(&mut self) {
        // SAFETY: `inner` returned by msresamp_rrrf_create, dropped at
        // most once.
        unsafe {
            if !self.inner.is_null() {
                sys::msresamp_rrrf_destroy(self.inner);
            }
        }
    }
}

// SAFETY: liquid's msresamp is single-threaded but movable.
unsafe impl Send for MsResamp {}

/// Analog-FM discriminator — wraps liquid's `freqdem_rrrf`-flavoured
/// `freqdem` (input is `liquid_float_complex`, output is `float`). One
/// complex IQ sample in → one real modulating-signal sample out, 1:1.
///
/// The modulation factor `kf` is `peak_deviation / sample_rate` — e.g.
/// broadcast FM at 240 kS/s with ±75 kHz peak gives `kf = 0.3125`. The
/// demodulator scales output so a carrier modulated at the peak lands
/// at output amplitude ±1.0.
pub struct FreqDem {
    inner: sys::freqdem,
    kf: f32,
}

impl FreqDem {
    pub fn new(kf: f32) -> Result<Self, &'static str> {
        if !(kf > 0.0 && kf.is_finite() && kf < 0.5) {
            return Err("freqdem: kf must be in (0, 0.5)");
        }
        // SAFETY: scalar arg; NULL on invalid config → Err.
        let inner = unsafe { sys::freqdem_create(kf) };
        if inner.is_null() {
            return Err("freqdem_create returned NULL");
        }
        Ok(Self { inner, kf })
    }

    /// Demodulate one complex IQ sample to one real output.
    pub fn demodulate(&mut self, re: f32, im: f32) -> f32 {
        let mut y: f32 = 0.0;
        // SAFETY: `inner` valid until Drop. Liquid takes the complex
        // input by value and writes one float through `_m`. Pass the
        // (re, im) pair via the liquid_float_complex layout: the
        // bindgen-produced repr is `[f32; 2]` in the same order.
        unsafe {
            let r = sys::__BindgenComplex { re, im };
            sys::freqdem_demodulate(self.inner, r, &mut y);
        }
        y
    }

    /// Configured modulation factor `kf = peak_deviation / sample_rate`.
    #[must_use]
    pub const fn kf(&self) -> f32 {
        self.kf
    }

    pub fn reset(&mut self) {
        // SAFETY: `inner` valid until Drop.
        unsafe {
            sys::freqdem_reset(self.inner);
        }
    }
}

impl Drop for FreqDem {
    fn drop(&mut self) {
        // SAFETY: `inner` returned by freqdem_create, dropped at most
        // once.
        unsafe {
            if !self.inner.is_null() {
                sys::freqdem_destroy(self.inner);
            }
        }
    }
}

// SAFETY: liquid's freqdem is single-threaded but movable.
unsafe impl Send for FreqDem {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firdecim_decimates_real_dc_to_unity() {
        // Hann-windowed-sinc LPF normalised to DC=1; decimation by 4
        // of an all-ones input should settle to 1.0 once the delay
        // line fills.
        use core::f32::consts::PI;
        let factor = 4u32;
        let n = 41usize;
        let m = (n - 1) as f32 / 2.0;
        let cutoff = 0.4_f32 / factor as f32;
        let mut taps: Vec<f32> = (0..n)
            .map(|i| {
                let k = i as f32 - m;
                let ideal = if k.abs() < 1e-7 {
                    2.0 * cutoff
                } else {
                    (2.0 * PI * cutoff * k).sin() / (PI * k)
                };
                let w = 0.5 - 0.5 * (2.0 * PI * i as f32 / (n - 1) as f32).cos();
                ideal * w
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        for t in &mut taps {
            *t /= sum;
        }
        let mut dec = Firdecim::from_taps(factor, &taps).expect("create");
        // Push 4 samples per execute call until steady state.
        let chunk = vec![1.0_f32; factor as usize];
        let mut last = 0.0;
        for _ in 0..40 {
            last = dec.execute(&chunk);
        }
        assert!(
            (last - 1.0).abs() < 1e-4,
            "DC steady-state should be 1.0, got {last}"
        );
    }

    #[test]
    fn firdecim_cx_passes_iq_dc() {
        // Same idea, complex side: a constant (1, 1) IQ stream
        // through a unit-DC-gain decimator should steady-state at
        // (1, 1).
        use core::f32::consts::PI;
        let factor = 4u32;
        let n = 41usize;
        let m = (n - 1) as f32 / 2.0;
        let cutoff = 0.4_f32 / factor as f32;
        let mut taps: Vec<f32> = (0..n)
            .map(|i| {
                let k = i as f32 - m;
                let ideal = if k.abs() < 1e-7 {
                    2.0 * cutoff
                } else {
                    (2.0 * PI * cutoff * k).sin() / (PI * k)
                };
                let w = 0.5 - 0.5 * (2.0 * PI * i as f32 / (n - 1) as f32).cos();
                ideal * w
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        for t in &mut taps {
            *t /= sum;
        }
        let mut dec = FirdecimCx::from_taps(factor, &taps).expect("create");
        let chunk = vec![(1.0_f32, 1.0_f32); factor as usize];
        let mut last = (0.0, 0.0);
        for _ in 0..40 {
            last = dec.execute(&chunk);
        }
        assert!(
            (last.0 - 1.0).abs() < 1e-4 && (last.1 - 1.0).abs() < 1e-4,
            "DC steady-state should be (1,1), got {last:?}"
        );
    }

    #[test]
    fn nco_mixes_tone_down_to_dc() {
        // Generate a +1 kHz IQ tone at 48 kHz, set the NCO to mix
        // it down to baseband (omega = -2π·f/Fs), and verify the
        // output is steady-state DC near (1, 0). Drift over many
        // samples would expose a phase-accumulator bug — run for
        // 8192 samples to make sure the table NCO doesn't wander.
        use core::f32::consts::TAU;
        let fs = 48_000.0_f32;
        let f = 1_000.0_f32;
        let n = 8192usize;
        let mut nco = Nco::new().expect("create");
        // `mix_down` already negates internally — `y = x·exp(-jθ)` —
        // so the NCO frequency must be the *positive* tone frequency
        // (in radians/sample) to land the rotation at zero net phase.
        nco.set_frequency(TAU * f / fs);

        let mut re_sum = 0.0_f32;
        let mut im_sum = 0.0_f32;
        for i in 0..n {
            let t = i as f32 / fs;
            let x_re = (TAU * f * t).cos();
            let x_im = (TAU * f * t).sin();
            let (y_re, y_im) = nco.mix_down_one(x_re, x_im);
            re_sum += y_re;
            im_sum += y_im;
        }
        // Steady-state mean = (1.0, 0.0) for a perfect mixer.
        let mean_re = re_sum / n as f32;
        let mean_im = im_sum / n as f32;
        assert!(
            (mean_re - 1.0).abs() < 0.01 && mean_im.abs() < 0.01,
            "NCO didn't mix tone to DC: mean=({mean_re}, {mean_im})"
        );
    }

    #[test]
    fn ampmodem_constructs_for_each_mode() {
        // Smoke: every variant of the AmType enum survives a
        // create + a couple of demodulate calls + drop. If this
        // works the wrapper API is wired correctly.
        for kind in [AmType::Dsb, AmType::Usb, AmType::Lsb] {
            let mut m = Ampmodem::new(0.8, kind, true).expect("create");
            for _ in 0..32 {
                let _ = m.demodulate(0.5, 0.1);
            }
        }
    }

    #[test]
    fn ampmodem_recovers_usb_tone_rejects_image() {
        // Feed a pure positive-frequency baseband IQ tone (USB) into
        // a USB demod — expect strong output. Feed the same
        // frequency negative-rotated (LSB) — expect near-zero output
        // from the USB demod. Image-rejection ratio in dB is the win
        // we're after vs. our hand-rolled SsbDemod which managed
        // ~24 dB. liquid's internal Remez-designed firhilbf should
        // do meaningfully better.
        use core::f32::consts::TAU;
        let fs = 48_000.0_f32;
        let f = 1_500.0_f32;
        let n = 8192usize;
        let mut usb = Ampmodem::new(0.8, AmType::Usb, true).expect("create");

        // USB-only tone — exp(+jωt).
        let mut out_match = vec![0.0_f32; n];
        for i in 0..n {
            let t = i as f32 / fs;
            out_match[i] = usb.demodulate((TAU * f * t).cos(), (TAU * f * t).sin());
        }
        usb.reset();
        // Same frequency, negative rotation — LSB tone, USB demod
        // should reject it.
        let mut out_image = vec![0.0_f32; n];
        for i in 0..n {
            let t = i as f32 / fs;
            out_image[i] = usb.demodulate((TAU * f * t).cos(), -(TAU * f * t).sin());
        }
        // Skip filter transient (group delay + a generous margin).
        let skip = (usb.delay() as usize) + 256;
        let rms = |x: &[f32]| -> f32 {
            let s: f32 = x.iter().map(|v| v * v).sum();
            (s / x.len() as f32).sqrt()
        };
        let r_match = rms(&out_match[skip..]);
        let r_image = rms(&out_image[skip..]);
        let ratio_db = 20.0 * (r_match / r_image.max(1e-9)).log10();
        // Bar set above hand-rolled SsbDemod's ~24 dB so we'd notice
        // a regression. liquid typically does 40–60 dB on this kind
        // of signal; 30 dB threshold gives ample margin without
        // becoming a flaky precision tripwire.
        assert!(
            ratio_db > 30.0,
            "USB image rejection should beat 30 dB, got {ratio_db:.1} dB \
             (match={r_match:.4}, image={r_image:.4})"
        );
    }
}
