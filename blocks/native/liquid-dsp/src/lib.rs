//! Safe Rust wrappers around vendored liquid-dsp.
//!
//! Each primitive we expose owns its underlying C handle via RAII —
//! the wrapper's `Drop` calls the matching `_destroy` so blocks don't
//! have to reach for unsafe in the common path.
//!
//! M2 surface is intentionally minimal: only `Firfilt<f32>` (real FIR
//! filter), enough to prove the wrapper pattern compiles, links, and
//! actually filters samples on both targets. Subsequent milestones
//! grow the surface — see `docs/decoder-roadmap/` for the order.

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

use std::marker::PhantomData;

/// Real-input / real-coefficient / real-output FIR filter — wraps
/// liquid's `firfilt_rrrf`. Holds an opaque handle whose lifetime is
/// tied to the wrapper's; `Drop` runs `firfilt_rrrf_destroy`.
pub struct Firfilt<T> {
    inner: sys::firfilt_rrrf,
    /// Phantom to lock the wrapper to a sample type; today we only
    /// have the rrrf variant but the API shape leaves room for `crcf`
    /// (real-tap complex-sample) and `cccf` (complex everywhere).
    _marker: PhantomData<T>,
}

impl Firfilt<f32> {
    /// Design a Kaiser-windowed lowpass FIR filter with `n` taps.
    /// `fc` is the normalised cutoff (0 < fc < 0.5; 0.25 ≈ Fs/4).
    /// `as_db` is the stop-band attenuation in dB; 60 dB is a typical
    /// audio-quality target.
    ///
    /// The result has unity DC gain — we ask liquid for the raw
    /// `sinc·window` taps (which sum to ≈ 2·fc·N) and then call
    /// `set_scale(1/H(0))` so a step input lands at 1.0 once the
    /// delay line settles. Without this every consumer would have to
    /// re-discover the same fact and apply their own scale.
    pub fn lowpass_kaiser(n: u32, fc: f32, as_db: f32) -> Result<Self, &'static str> {
        if n < 3 {
            return Err("firfilt: need at least 3 taps");
        }
        if !(fc > 0.0 && fc < 0.5) {
            return Err("firfilt: cutoff must be in (0, 0.5)");
        }
        // SAFETY: liquid's `_create_kaiser` allocates and returns NULL
        // on failure (we treat that as an `Err`). All inputs are
        // validated above.
        let inner = unsafe { sys::firfilt_rrrf_create_kaiser(n, fc, as_db, 0.0) };
        if inner.is_null() {
            return Err("firfilt_rrrf_create_kaiser returned NULL");
        }
        // Read the DC gain via the freqresponse helper. liquid exposes
        // `liquid_float_complex` which on every platform we ship to is
        // C `_Complex float` — two consecutive f32s in memory. We
        // shape it as `[f32; 2]` so we don't drag a complex-number
        // crate into the substrate just for this one read.
        let mut h: [f32; 2] = [0.0; 2];
        // SAFETY: `inner` is a valid handle; `h` is a writable
        // 2-float buffer of matching layout to `liquid_float_complex`.
        unsafe {
            sys::firfilt_rrrf_freqresponse(inner, 0.0, h.as_mut_ptr().cast());
        }
        let dc_gain = h[0];
        if dc_gain.abs() > 1e-9 {
            // SAFETY: `inner` is a valid handle.
            unsafe {
                sys::firfilt_rrrf_set_scale(inner, 1.0 / dc_gain);
            }
        }
        Ok(Self {
            inner,
            _marker: PhantomData,
        })
    }

    /// Push one sample into the filter and read one sample out.
    pub fn execute_one(&mut self, x: f32) -> f32 {
        let mut y: f32 = 0.0;
        // SAFETY: `inner` is a valid handle (constructor invariant);
        // `&mut y` is a unique writable pointer to a local stack slot.
        unsafe { sys::firfilt_rrrf_execute_one(self.inner, x, &mut y) };
        y
    }

    /// Push a slice of samples and write the same number of samples out.
    /// Caller-supplied output slice must be at least as long as input.
    pub fn execute(&mut self, input: &[f32], output: &mut [f32]) {
        assert!(
            output.len() >= input.len(),
            "firfilt::execute: output slice too small ({} < {})",
            output.len(),
            input.len()
        );
        for (i, &x) in input.iter().enumerate() {
            output[i] = self.execute_one(x);
        }
    }

    /// Reset filter state (zeros the delay line).
    pub fn reset(&mut self) {
        // SAFETY: `inner` is a valid handle.
        unsafe {
            sys::firfilt_rrrf_reset(self.inner);
        }
    }
}

/// WASM smoke entry point — a no-cost facade behind the `wasm`
/// feature that lets the wasm-pack output be exercised from JS:
/// builds a Kaiser-windowed lowpass with the given params, pushes
/// `samples` ones into it, returns the steady-state output. JS test
/// scripts call this and assert the value is ≈ 1.0, which proves
/// liquid's filter design + execute paths run end to end inside the
/// browser WASM runtime.
#[cfg(feature = "wasm")]
#[wasm_bindgen::prelude::wasm_bindgen]
#[must_use]
pub fn liquid_lowpass_dc_gain(n: u32, fc: f32, as_db: f32, samples: u32) -> f32 {
    let Ok(mut filt) = Firfilt::<f32>::lowpass_kaiser(n, fc, as_db) else {
        return f32::NAN;
    };
    let mut last = 0.0;
    for _ in 0..samples {
        last = filt.execute_one(1.0);
    }
    last
}

impl<T> Drop for Firfilt<T> {
    fn drop(&mut self) {
        // SAFETY: `inner` was returned by liquid's create function and
        // hasn't been destroyed; `Drop` runs at most once per instance.
        unsafe {
            if !self.inner.is_null() {
                sys::firfilt_rrrf_destroy(self.inner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowpass_kaiser_constructs_and_destroys() {
        // The smoke test: build it, push some samples through, drop.
        // If this passes, the C library is linked, libm is reachable,
        // and the calling convention works on the host target.
        let mut filt = Firfilt::<f32>::lowpass_kaiser(11, 0.2, 60.0).expect("create");
        for i in 0..256 {
            let _ = filt.execute_one(i as f32);
        }
    }

    #[test]
    fn lowpass_attenuates_above_cutoff() {
        // A 21-tap Kaiser LPF at fc=0.1 (Fs/10) should pass DC nearly
        // unattenuated and stop a Nyquist-rate input cold. Worth a
        // sanity test rather than just constructor coverage — proves
        // we're actually filtering, not just shuffling samples.
        let mut filt = Firfilt::<f32>::lowpass_kaiser(31, 0.1, 60.0).expect("create");
        // Settle the delay line on a DC input.
        let mut last_dc = 0.0;
        for _ in 0..256 {
            last_dc = filt.execute_one(1.0);
        }
        assert!(
            (last_dc - 1.0).abs() < 0.01,
            "lowpass should pass DC near unity, got {last_dc}"
        );

        filt.reset();
        // Nyquist-rate alternating signal — filter should attenuate.
        let mut peak = 0.0_f32;
        for i in 0..256 {
            let x = if i % 2 == 0 { 1.0 } else { -1.0 };
            let y = filt.execute_one(x).abs();
            if i > 64 && y > peak {
                peak = y;
            }
        }
        assert!(
            peak < 0.05,
            "lowpass should reject Nyquist signal, got peak={peak}"
        );
    }

    #[test]
    fn rejects_invalid_inputs() {
        assert!(Firfilt::<f32>::lowpass_kaiser(0, 0.1, 60.0).is_err());
        assert!(Firfilt::<f32>::lowpass_kaiser(11, 0.0, 60.0).is_err());
        assert!(Firfilt::<f32>::lowpass_kaiser(11, 0.5, 60.0).is_err());
    }
}
