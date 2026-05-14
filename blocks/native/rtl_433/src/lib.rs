//! Safe Rust wrapper around vendored rtl_433.
//!
//! One [`Rtl433Demod`] instance wraps an upstream `r_cfg_t` (with its
//! `dm_state`, `pulse_detect`, and ~220 registered device decoders) and
//! presents a sample-in / event-out interface. The heavy DSP — magnitude
//! estimate, low-pass filtering, FM demod, pulse detection, per-protocol
//! bit slicing, and per-device decode — all lives on the C side. We
//! provide the safe API and the JSON event drain.
//!
//! ### Block shape
//!
//! - **Input:** `iq_f32` at the decoder's working rate (250 kS/s by
//!   default — upstream's pick). The upstream `Channelizer` handles the
//!   freq-shift + decimation from the source's wider rate.
//! - **Output:** decoded device records as JSON strings; the wrapping
//!   `Block` in `ferrite-blocks` emits them as `events` and to the
//!   `decoder::rtl_433` tracing target.
//! - **Placement:** dual-compiled — `cargo build` for the server,
//!   `wasm-pack build` for the browser. Bundle size is large (~4 MB
//!   WASM) but the WsBridge lets a preset run the decoder server-side
//!   if the user prefers.
//! - **Audio tee:** the block does *not* expose audio. Presets that
//!   want an audible "is something chirping?" track tee the IQ to a
//!   parallel `FmDemod → Resample → AudioNrMono → AudioSink` chain
//!   gated on `when: { audio: true }`. Matches `pager.json` shape.
//!
//! ### Decoder-set selection
//!
//! Each upstream `r_device` carries a `disabled` field:
//!
//! - `0` = default-enabled (the stable common decoders, ~220 of them)
//! - `1` = default-disabled (experimental / niche / noisy)
//! - `2` = disabled (broken or deprecated)
//! - `3` = disabled and hidden
//!
//! The wrapper exposes this as a 3-way [`DecoderSet`] knob. One call to
//! upstream's `register_all_protocols(state, threshold)` at block init
//! enables every decoder at-or-below the chosen threshold.
//!
//! ### License
//!
//! rtl_433 (vendored in `vendor/`) is GPL-2-or-later by Christian
//! Zuckschwerdt et al. The shim + wrapper here are GPL-3-or-later under
//! the project's overall license; the two are compatible.

#![allow(unsafe_op_in_unsafe_fn)]

mod sys {
    //! Raw bindgen output. Hidden behind the safe API above.
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(non_upper_case_globals)]
    #![allow(dead_code)]
    #![allow(clippy::all)]
    #![allow(clippy::pedantic)]
    include!(concat!(env!("OUT_DIR"), "/rtl433_bindings.rs"));
}

use std::ffi::CStr;
use std::ptr::NonNull;

use num_complex::Complex;

/// Which subset of upstream decoders to register at block init.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DecoderSet {
    /// Threshold = 0. ~220 stable decoders only — the upstream default
    /// when running `rtl_433` with no `-R` flags. Lowest false-positive
    /// rate.
    #[default]
    Default,
    /// Threshold = 1. Adds experimental / niche / noisy decoders that
    /// upstream ships disabled-by-default. The historical `-G 1`
    /// behaviour.
    Extended,
    /// Threshold = 3. Adds the rest (broken / deprecated / hidden).
    /// Mostly here for parity with upstream's full decoder list — not
    /// recommended for live use.
    All,
}

impl DecoderSet {
    /// The `disabled <=` threshold passed to `register_all_protocols`.
    #[must_use]
    pub const fn threshold(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::Extended => 1,
            Self::All => 3,
        }
    }
}

/// One running rtl_433 decoder instance.
///
/// Wraps the opaque C-side state. Owns the upstream `r_cfg_t`, the
/// pulse detector, all registered `r_device` instances, and the JSON
/// event ring. Drop runs `rtl433_free` to release the lot.
pub struct Rtl433Demod {
    state: NonNull<sys::rtl433_state_t>,
    sample_rate_hz: u32,
}

impl Rtl433Demod {
    /// Build and initialise a decoder at the given input sample rate.
    /// 250 kHz is upstream's default and the right value for most ISM
    /// bands; bumps to 1 MHz pull in a handful of high-baud devices at
    /// ~4× the CPU cost.
    ///
    /// Returns `None` on allocation failure (~44 MB heap needed for the
    /// upstream `dm_state` — fine on a server, occasionally tight in
    /// browser WASM under memory pressure).
    #[must_use]
    pub fn new(sample_rate_hz: u32, decoder_set: DecoderSet) -> Option<Self> {
        // SAFETY: `rtl433_init` only touches its own allocations; safe
        // to call from any thread provided the result isn't shared
        // (see `Send` impl below).
        let raw = unsafe { sys::rtl433_init(sample_rate_hz, decoder_set.threshold()) };
        let state = NonNull::new(raw)?;
        Some(Self {
            state,
            sample_rate_hz,
        })
    }

    /// Native sample rate this decoder was constructed for.
    #[must_use]
    pub const fn sample_rate_hz(&self) -> u32 {
        self.sample_rate_hz
    }

    /// Reset the pulse detector + FM demodulator state. Used between
    /// unrelated capture segments; not normally needed during
    /// streaming. Drops any pending events from the ring as well.
    pub fn reset(&mut self) {
        // SAFETY: state pointer is valid for the lifetime of self.
        unsafe { sys::rtl433_reset(self.state.as_ptr()) };
    }

    /// Push `n` complex IQ samples (Ferrite ±1.0 convention). `Complex<f32>`
    /// is `#[repr(C)]` with `re` then `im`, matching the interleaved
    /// float layout the shim expects, so the slice goes through the
    /// FFI untouched. The shim scales internally to int16 and runs the
    /// upstream pulse-detect + decode dispatch. Decoded events
    /// accumulate in the internal ring; call [`Self::drain_event`]
    /// after each push to harvest them.
    pub fn push_iq(&mut self, iq: &[Complex<f32>]) {
        if iq.is_empty() {
            return;
        }
        // SAFETY: `iq.as_ptr()` is valid for `iq.len()` Complex<f32>
        // elements, each two f32. The C side reads exactly
        // `iq.len() * 2` floats and never holds the pointer across
        // the call. `Complex<f32>` is `#[repr(C)]`; cast is safe.
        unsafe {
            sys::rtl433_push_iq(self.state.as_ptr(), iq.as_ptr().cast::<f32>(), iq.len());
        }
    }

    /// Drain one decoded event from the ring as a UTF-8 string. Returns
    /// `None` if the ring is empty. Subsequent calls return additional
    /// events until the ring drains; call until `None` after each push.
    ///
    /// Each event is a self-contained JSON object with at minimum a
    /// `model` field and per-device fields (temperature, battery,
    /// id, etc.). Upstream's documentation lists the schema per
    /// decoder.
    #[must_use]
    pub fn drain_event(&mut self) -> Option<String> {
        // 8 KB is twice the shim's per-slot cap; should never truncate.
        let mut buf = vec![0u8; 8192];
        // SAFETY: `buf.as_mut_ptr()` is valid for `buf.len()` bytes.
        let n = unsafe {
            sys::rtl433_drain_event(self.state.as_ptr(), buf.as_mut_ptr().cast(), buf.len())
        };
        if n <= 0 {
            return None;
        }
        let n = n as usize;
        // The shim NUL-terminates; CStr is the safe way to find the
        // length even though we already know it.
        let s = CStr::from_bytes_with_nul(&buf[..=n]).ok()?;
        Some(s.to_string_lossy().into_owned())
    }
}

impl Drop for Rtl433Demod {
    fn drop(&mut self) {
        // SAFETY: state pointer was returned by `rtl433_init` and
        // hasn't been freed yet (Drop runs once).
        unsafe { sys::rtl433_free(self.state.as_ptr()) };
    }
}

// SAFETY: `Rtl433Demod` owns its state exclusively; the C side has no
// internal threading and no global mutable state once initialised.
// Moving between threads is fine; we don't implement `Sync` because two
// concurrent `push_iq` calls would race on the per-instance ring.
unsafe impl Send for Rtl433Demod {}

#[cfg(test)]
mod tests {
    use super::{DecoderSet, Rtl433Demod};

    #[test]
    fn empty_silence_emits_no_events() {
        use num_complex::Complex;
        // 0.1 s of silence at 250 kHz — pulse detect should see no
        // packages and the ring should stay empty.
        let mut d = Rtl433Demod::new(250_000, DecoderSet::Default)
            .expect("rtl433_init under test allocator");
        let zeros = vec![Complex::new(0.0_f32, 0.0_f32); 25_000];
        d.push_iq(&zeros);
        assert!(d.drain_event().is_none());
    }

    #[test]
    fn decoder_set_thresholds_match_upstream() {
        assert_eq!(DecoderSet::Default.threshold(), 0);
        assert_eq!(DecoderSet::Extended.threshold(), 1);
        assert_eq!(DecoderSet::All.threshold(), 3);
    }
}
