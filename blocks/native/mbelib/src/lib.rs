//! mbelib voice synthesis for Ferrite's digital-voice decoders.
//!
//! One [`Vocoder`] per voice stream (each DMR timeslot): it keeps the
//! previous frame's parameters, which the synthesis interpolates from.
//! [`Vocoder::decode_ambe2450`] takes one AMBE+2 3600×2450 frame (DMR,
//! P25 Phase 2, NXDN, dPMR) in mbelib's deinterleaved `ambe_fr[4][24]`
//! layout and returns 160 samples (20 ms at 8 kHz) normalised to ±1.
//!
//! Backends, by `cfg(target_arch)`:
//! * native — mbelib compiled in (`build.rs`), `extern "C"` shim calls;
//! * wasm32 — the same shim ABI, served by a *sibling* Emscripten
//!   module (`mbe.{mjs,wasm}`) that the browser runner fetches lazily,
//!   only when a flowgraph actually decodes voice in the browser. Until
//!   it is loaded (or if it isn't built), `Vocoder::new` returns `None`
//!   and callers emit silence — metadata decoding is unaffected.
//!
//! Patent note: mbelib's README (vendor/README.md) warns that compiled
//! AMBE/IMBE decoders may be covered by patents. The blocks crate only
//! pulls this crate in behind its `vocoder` feature, so a build can
//! drop it with one flag.

/// Samples per AMBE frame (20 ms at 8 kHz).
pub const FRAME_SAMPLES: usize = 160;
/// mbelib's float synthesis is int16 / 7 (its own short conversion
/// multiplies by 7); this maps it to ±1.
const SCALE: f32 = 7.0 / 32_768.0;

#[cfg(not(target_arch = "wasm32"))]
mod backend {
    use std::os::raw::{c_float, c_int, c_uchar};

    #[repr(C)]
    pub struct Raw {
        _p: [u8; 0],
    }
    pub type Handle = *mut Raw;

    extern "C" {
        fn ferrite_mbe_create() -> Handle;
        fn ferrite_mbe_decode_ambe2450(
            h: Handle,
            fr96: *const c_uchar,
            out160: *mut c_float,
        ) -> c_int;
        fn ferrite_mbe_destroy(h: Handle);
    }

    pub fn create() -> Option<Handle> {
        // SAFETY: no arguments; returns a fresh heap object or null.
        let h = unsafe { ferrite_mbe_create() };
        (!h.is_null()).then_some(h)
    }
    pub fn decode(h: Handle, fr: &[u8; 96], out: &mut [f32; super::FRAME_SAMPLES]) -> i32 {
        // SAFETY: `h` came from `create`, both buffers are the sizes the
        // shim reads/writes (96 bytes in, 160 floats out).
        unsafe { ferrite_mbe_decode_ambe2450(h, fr.as_ptr(), out.as_mut_ptr()) }
    }
    pub fn destroy(h: Handle) {
        // SAFETY: `h` came from `create` and is dropped exactly once.
        unsafe { ferrite_mbe_destroy(h) }
    }
}

#[cfg(target_arch = "wasm32")]
mod backend {
    use wasm_bindgen::prelude::*;

    pub type Handle = u32;

    #[wasm_bindgen(module = "/js/mbe_bridge.js")]
    extern "C" {
        fn mbe_create() -> u32;
        fn mbe_decode(h: u32, fr96: &[u8]) -> Vec<f32>;
        fn mbe_destroy(h: u32);
    }

    pub fn create() -> Option<Handle> {
        let h = mbe_create();
        (h != 0).then_some(h)
    }
    pub fn decode(h: Handle, fr: &[u8; 96], out: &mut [f32; super::FRAME_SAMPLES]) -> i32 {
        let v = mbe_decode(h, fr);
        if v.len() < super::FRAME_SAMPLES {
            out.fill(0.0);
            return -1;
        }
        out.copy_from_slice(&v[..super::FRAME_SAMPLES]);
        // The bridge appends errs2 as a 161st value.
        #[allow(clippy::cast_possible_truncation)]
        v.get(super::FRAME_SAMPLES).map_or(0, |&e| e as i32)
    }
    pub fn destroy(h: Handle) {
        mbe_destroy(h);
    }
}

/// One voice stream's synthesiser state.
pub struct Vocoder {
    h: backend::Handle,
}

// The native handle is a plain heap struct used from one thread at a
// time (the runtime moves blocks between worker threads, never shares).
unsafe impl Send for Vocoder {}

impl Vocoder {
    /// `None` when the vocoder is unavailable — in the browser, until the
    /// sibling module has loaded (or when it isn't built).
    #[must_use]
    pub fn new() -> Option<Self> {
        backend::create().map(|h| Self { h })
    }

    /// Decode one AMBE+2 frame. `fr` is `ambe_fr[4][24]`, row-major, one
    /// bit per byte. Returns the error count mbelib corrected (or < 0).
    pub fn decode_ambe2450(&mut self, fr: &[u8; 96], out: &mut [f32; FRAME_SAMPLES]) -> i32 {
        let errs = backend::decode(self.h, fr, out);
        for s in out.iter_mut() {
            *s = (*s * SCALE).clamp(-1.0, 1.0);
        }
        errs
    }
}

impl Drop for Vocoder {
    fn drop(&mut self) {
        backend::destroy(self.h);
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{Vocoder, FRAME_SAMPLES};

    #[test]
    fn decodes_a_frame_without_panicking() {
        let mut v = Vocoder::new().expect("native vocoder");
        let mut out = [0.0_f32; FRAME_SAMPLES];
        let fr = [0_u8; 96];
        let e = v.decode_ambe2450(&fr, &mut out);
        assert!(e >= 0);
        assert!(out.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
    }
}
