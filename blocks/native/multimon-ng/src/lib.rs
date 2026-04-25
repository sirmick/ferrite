//! Safe Rust wrapper around vendored multimon-ng decoders.
//!
//! Each decoder is exposed as a [`MultimonDemod`] instance bound to a
//! specific [`Decoder`] kind. The C side keeps the heavy DSP (bit
//! sync, sync-word matching, BCH decode for POCSAG, character-set
//! decode); we keep the safe API and the output drain.
//!
//! ### Pumping samples
//!
//! Each [`MultimonDemod::push`] copies the slice across the FFI boundary
//! and calls the decoder's `demod()` callback once. The decoder writes
//! any decoded message lines through `_verbprintf` (re-implemented in
//! the shim) into a thread-local buffer; immediately after the call,
//! [`MultimonDemod::drain_lines`] returns the accumulated complete
//! lines and clears the buffer.
//!
//! ### Sample-rate contract
//!
//! Each decoder pins its native sample rate (e.g. POCSAG1200 wants
//! 22050 Hz). [`Decoder::sample_rate_hz`] surfaces that to callers so
//! the wrapping `Block` can resample upstream NBFM-demodulated audio
//! to the right rate before pushing.
//!
//! ### Threading
//!
//! Decoder state (`*mut demod_state`) is owned per-instance and only
//! touched from the thread that built it. The capture buffer is
//! `__thread` in C, so multiple instances on different threads are
//! independent. Within one thread, the runtime's serialised tick loop
//! plus the immediate-drain pattern means two instances never collide
//! on the buffer either.
//!
//! ### License
//!
//! multimon-ng (in `vendor/`) is GPL-2-or-later by Tom Sailer, Elias
//! Oenal et al. This wrapper is GPL-3-or-later under the project's
//! overall license; the two are compatible.

#![allow(unsafe_op_in_unsafe_fn)]

mod sys {
    //! Raw bindgen output. Hidden behind the safe API above.
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(non_upper_case_globals)]
    #![allow(dead_code)]
    #![allow(clippy::all)]
    #![allow(clippy::pedantic)]
    include!(concat!(env!("OUT_DIR"), "/multimon_bindings.rs"));
}

use std::mem::MaybeUninit;

/// Which decoder this instance runs. Adding a decoder = vendor source
/// in `build.rs::decoder_sources()` + bindgen `allowlist_var` + a
/// variant here + a `pub fn from_kind` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoder {
    /// 1200 baud POCSAG paging — 22050 Hz audio in.
    Pocsag1200,
}

impl Decoder {
    /// Native input sample rate this decoder expects.
    #[must_use]
    pub fn sample_rate_hz(self) -> u32 {
        // SAFETY: each decoder's `demod_param` is a `const` static
        // emitted by multimon — we just read its samplerate field.
        unsafe {
            match self {
                Self::Pocsag1200 => sys::demod_poc12.samplerate,
            }
        }
    }

    /// Human-readable name from the decoder's `demod_param`.
    #[must_use]
    pub fn name(self) -> &'static str {
        // SAFETY: `name` points to a string literal owned by the C
        // static, so `'static` is correct.
        unsafe {
            let p = match self {
                Self::Pocsag1200 => sys::demod_poc12.name,
            };
            let bytes = std::ffi::CStr::from_ptr(p).to_bytes();
            std::str::from_utf8_unchecked(bytes)
        }
    }

    fn dem_par(self) -> *const sys::demod_param {
        // addr-of a `const` C static — no unsafe needed.
        match self {
            Self::Pocsag1200 => &raw const sys::demod_poc12,
        }
    }
}

/// One running multimon-ng decoder instance.
///
/// Holds the heavy `demod_state` on the heap (it's a few KB —
/// `Box::new_uninit` keeps it off the stack and stable in memory so
/// multimon's pointer-based callbacks remain valid).
pub struct MultimonDemod {
    kind: Decoder,
    state: Box<sys::demod_state>,
}

impl MultimonDemod {
    /// Build and initialise a new decoder of the given kind.
    pub fn new(kind: Decoder) -> Self {
        let dem_par = kind.dem_par();
        // Allocate zeroed `demod_state` on the heap, set its
        // back-pointer to the per-decoder param block, then call the
        // decoder's `init` callback to populate the per-decoder l1/l2
        // sub-state.
        let mut state: Box<MaybeUninit<sys::demod_state>> = Box::new_uninit();
        // SAFETY: zero-init is a valid bit-pattern for every field of
        // `demod_state` (it's a `union` of POD-only structs).
        unsafe {
            std::ptr::write_bytes(state.as_mut_ptr(), 0, 1);
        }
        // SAFETY: `state` is now zero-initialised, fully valid for
        // assume_init. It needs the back-pointer set before init().
        let mut state: Box<sys::demod_state> = unsafe { state.assume_init() };
        state.dem_par = dem_par;
        // SAFETY: `dem_par` is a non-null pointer to a `const` static;
        // `state` is a valid heap allocation. The `init` callback
        // touches only `state` and the static, both live for as long
        // as needed.
        unsafe {
            if let Some(init_fn) = (*dem_par).init {
                init_fn(state.as_mut() as *mut _);
            }
        }
        Self { kind, state }
    }

    /// Decoder kind this instance is running.
    #[must_use]
    pub const fn kind(&self) -> Decoder {
        self.kind
    }

    /// Native sample rate the decoder expects.
    #[must_use]
    pub fn sample_rate_hz(&self) -> u32 {
        self.kind.sample_rate_hz()
    }

    /// Push one block of f32 audio samples through the decoder. Pre-
    /// resample to [`Self::sample_rate_hz`] upstream — multimon's
    /// per-decoder bit timing assumes the exact native rate.
    pub fn push(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        let buf = sys::buffer {
            sbuffer: std::ptr::null(),
            fbuffer: samples.as_ptr(),
        };
        // SAFETY: `state` is initialised, `samples` lives for the
        // duration of this call, and `length` matches the slice. The
        // decoder reads exclusively through the `fbuffer` field
        // because its `demod_param.float_samples` is `true`.
        unsafe {
            if let Some(demod_fn) = (*self.kind.dem_par()).demod {
                let len = i32::try_from(samples.len()).unwrap_or(i32::MAX);
                demod_fn(self.state.as_mut() as *mut _, buf, len);
            }
        }
    }

    /// Drain any complete decoded message lines the decoder has
    /// emitted since the last call. Returns one `String` per
    /// newline-terminated line. Trailing partial bytes (if any) are
    /// kept in the buffer for the next drain.
    #[must_use]
    pub fn drain_lines(&mut self) -> Vec<String> {
        // 64 KB is the C-side buffer cap; one extra byte for safety.
        let mut scratch = vec![0_u8; 65_536];
        // SAFETY: `multimon_drain` writes at most `cap` bytes into
        // `dst`. We size the buffer to match, so no overrun.
        let n = unsafe { sys::multimon_drain(scratch.as_mut_ptr().cast(), scratch.len()) };
        scratch.truncate(n);
        if scratch.is_empty() {
            return Vec::new();
        }
        // Split on '\n'; drop the trailing empty fragment from a
        // perfectly-terminated buffer. Lossy UTF-8 — multimon should
        // be ASCII-only but defensive against malformed input.
        let s = String::from_utf8_lossy(&scratch);
        s.split('\n')
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Reset the per-thread output buffer. Used on block reset to
    /// drop any half-formed line that won't continue.
    pub fn reset_output(&mut self) {
        // SAFETY: no preconditions; the shim function just zeroes
        // the per-thread buffer length.
        unsafe { sys::multimon_reset_buffer() };
    }
}

impl Drop for MultimonDemod {
    fn drop(&mut self) {
        // SAFETY: `state` is still valid; deinit only touches it +
        // the const param block. After deinit returns, `Box` frees
        // the allocation as usual.
        unsafe {
            if let Some(deinit_fn) = (*self.kind.dem_par()).deinit {
                deinit_fn(self.state.as_mut() as *mut _);
            }
        }
    }
}

// SAFETY: `MultimonDemod` owns its `demod_state` exclusively; multimon
// decoders aren't internally threaded and don't touch any non-const
// global once initialised. Moving an instance between threads is fine
// as long as we don't share access (no `Sync`).
unsafe impl Send for MultimonDemod {}

#[cfg(test)]
mod tests {
    use super::{Decoder, MultimonDemod};

    #[test]
    fn pocsag1200_sample_rate_is_22050() {
        assert_eq!(Decoder::Pocsag1200.sample_rate_hz(), 22_050);
    }

    #[test]
    fn pocsag1200_name_round_trips() {
        assert_eq!(Decoder::Pocsag1200.name(), "POCSAG1200");
    }

    #[test]
    fn empty_silence_emits_no_lines() {
        // 1 s of silence at 22050 Hz: nothing to decode, no lines.
        let mut d = MultimonDemod::new(Decoder::Pocsag1200);
        d.push(&vec![0.0_f32; 22_050]);
        assert!(d.drain_lines().is_empty());
    }
}
