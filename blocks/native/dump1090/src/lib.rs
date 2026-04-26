//! Safe Rust wrapper around antirez's vendored dump1090 (Mode S / ADS-B
//! at 1090 MHz).
//!
//! ### Surface
//!
//! - [`Dump1090::new`] lazily initialises the global decoder state on
//!   first call (subsequent constructions reuse it — the underlying C
//!   `Modes` global is process-wide). Cheap to call.
//! - [`Dump1090::push_iq`] takes complex-float IQ at 2 MS/s, converts
//!   it to RTL-SDR's u8 interleaved format inline (`(re*127 + 128)`,
//!   `(im*127 + 128)`), and forwards into the shim's batching ring.
//!   Frames decoded inside the C side land in a per-thread text buffer.
//! - [`Dump1090::drain_lines`] empties that buffer, returning the
//!   accumulated text split on `\n`. Same envelope as
//!   `MultimonDemod::drain_lines` so the ferrite-blocks side stays
//!   symmetric.
//!
//! ### Sample-rate contract
//!
//! Hard 2 MS/s. dump1090's preamble detector is hand-tuned to the
//! 8-bit-per-microsecond grid this rate produces; off-rate IQ decodes
//! as garbage. The `AdsbDemod` block in `ferrite-blocks` enforces this
//! at init time and warns once if the upstream rate disagrees.
//!
//! ### Threading
//!
//! Each `Dump1090` instance is `Send` + `!Sync` — the shim's per-thread
//! `dump1090_buffer` is exactly the right shape for the runtime's tick
//! loop (one decoder = one thread, no contention). Two instances on
//! the same thread would clobber each other's output buffer; the C
//! `Modes` global is also unique per process, so don't construct more
//! than one. (We document this rather than enforce — same trade-off as
//! `ferrite-multimon-ng`'s `MultimonDemod`.)

#![allow(unsafe_op_in_unsafe_fn)]

use num_complex::Complex;

mod sys {
    //! Raw bindgen output. Hidden behind the safe API above.
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(non_upper_case_globals)]
    #![allow(dead_code)]
    #![allow(clippy::all)]
    #![allow(clippy::pedantic)]
    include!(concat!(env!("OUT_DIR"), "/dump1090_bindings.rs"));
}

/// Native input rate the decoder expects. Hard contract — dump1090's
/// preamble correlator and bit slicer are sized for exactly this rate.
pub const ADSB_INPUT_RATE_HZ: u32 = 2_000_000;

/// Safe handle around the global dump1090 state.
///
/// Construction is cheap after the first instance — `dump1090_init` is
/// idempotent. There is no per-instance state on the Rust side; the
/// type exists so `Drop` and the `Send` mark have somewhere to live and
/// so callers can't accidentally call the C entry points before init.
pub struct Dump1090 {
    /// `u8` scratch the IQ-conversion loop writes into before calling
    /// the shim. Sized at construction (16384 samples = 32768 bytes).
    /// Reused across pushes to keep per-call alloc out of the hot path.
    iq_scratch: Vec<u8>,
}

impl Dump1090 {
    /// Construct a handle. Lazily runs the C-side global init on first
    /// call; safe to call multiple times (second-call no-op).
    #[must_use]
    pub fn new() -> Self {
        // SAFETY: `dump1090_init` has an internal idempotent guard. No
        // arguments, no preconditions.
        unsafe {
            sys::dump1090_init();
        }
        Self {
            iq_scratch: Vec::with_capacity(32_768),
        }
    }

    /// Push one block of IQ samples. Caller's chunk size is irrelevant
    /// — the shim accumulates internally and processes one
    /// `MODES_DATA_LEN` (~256 KB) batch at a time, mirroring how the
    /// original `rtlsdrCallback` fed the demod.
    ///
    /// Conversion: `Complex<f32>` in `[-1, 1]` → u8 with 128 = zero.
    /// Saturation on out-of-range floats (matches RTL-SDR's clipping
    /// behaviour at strong signals).
    pub fn push_iq(&mut self, samples: &[Complex<f32>]) {
        if samples.is_empty() {
            return;
        }
        // Two bytes per complex sample. Reuse the scratch — pushes can
        // be tens of thousands of samples on a busy ADS-B band.
        self.iq_scratch.clear();
        self.iq_scratch.reserve(samples.len() * 2);
        for s in samples {
            self.iq_scratch.push(f32_to_u8(s.re));
            self.iq_scratch.push(f32_to_u8(s.im));
        }
        // SAFETY: `iq_scratch` points at owned valid bytes; the shim
        // copies before returning so the lifetime ends here.
        unsafe {
            sys::dump1090_push_iq_u8(self.iq_scratch.as_ptr(), self.iq_scratch.len());
        }
    }

    /// Drain any complete decoded lines emitted since the last call.
    /// Returns one `String` per `\n`-terminated chunk. Trailing partial
    /// bytes (if any) stay in the C-side buffer for the next call.
    #[must_use]
    pub fn drain_lines(&mut self) -> Vec<String> {
        // 64 KB matches the C-side cap; one call is enough to drain
        // even a busy band's per-batch output (a packed batch produces
        // ~10 frames × ~30 lines = ~300 lines max, all under 4 KB).
        let mut scratch = vec![0_u8; 65_536];
        // SAFETY: dst pointer + cap are valid; the shim writes at most
        // `cap` bytes and returns the count.
        let n = unsafe { sys::dump1090_drain(scratch.as_mut_ptr().cast(), scratch.len()) };
        scratch.truncate(n);
        if scratch.is_empty() {
            return Vec::new();
        }
        let s = String::from_utf8_lossy(&scratch);
        s.split('\n')
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Reset the per-thread output buffer + the aircraft tracking list.
    /// Used between unrelated capture sessions (e.g. the offline
    /// analyzer A/B-ing different fixtures); not needed in live use.
    pub fn reset(&mut self) {
        // SAFETY: idempotent if not yet inited; otherwise just frees
        // the aircraft list and clears the text ring.
        unsafe { sys::dump1090_reset() };
    }
}

impl Default for Dump1090 {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: the `Modes` C global is process-wide; one instance must be
// created at most. Within that constraint, moving the Rust handle
// between threads is fine — the per-thread output buffer follows the
// thread the demod runs on. We mark Send and deliberately not Sync so
// shared references don't accidentally let two threads call push_iq
// against the same Modes state.
unsafe impl Send for Dump1090 {}

/// Convert one `f32` sample (nominal `[-1, 1]`) into RTL-SDR's u8
/// representation where `128` represents zero. Saturating cast — out-of-
/// range values clip rather than wrap so a strong burst doesn't fold
/// into low magnitudes.
#[inline]
fn f32_to_u8(x: f32) -> u8 {
    let scaled = x.mul_add(127.0, 128.0);
    if scaled <= 0.0 {
        0
    } else if scaled >= 255.0 {
        255
    } else {
        scaled as u8
    }
}

#[cfg(test)]
mod tests {
    use super::{f32_to_u8, Dump1090, ADSB_INPUT_RATE_HZ};
    use num_complex::Complex;

    #[test]
    fn rate_constant_is_two_mhz() {
        assert_eq!(ADSB_INPUT_RATE_HZ, 2_000_000);
    }

    #[test]
    fn f32_to_u8_centres_zero_at_128() {
        assert_eq!(f32_to_u8(0.0), 128);
        assert_eq!(f32_to_u8(1.0), 255);
        assert_eq!(f32_to_u8(-1.0), 1);
        // Saturate, don't wrap.
        assert_eq!(f32_to_u8(2.0), 255);
        assert_eq!(f32_to_u8(-2.0), 0);
        assert_eq!(f32_to_u8(f32::INFINITY), 255);
        assert_eq!(f32_to_u8(f32::NEG_INFINITY), 0);
    }

    #[test]
    fn silence_does_not_panic_or_emit() {
        let mut d = Dump1090::new();
        // Two batches' worth of zero IQ. dump1090's preamble detector
        // requires energy above noise — silence should produce nothing.
        let zeros = vec![Complex::new(0.0_f32, 0.0_f32); 600_000];
        d.push_iq(&zeros);
        let lines = d.drain_lines();
        assert!(lines.is_empty(), "silence emitted {} lines", lines.len());
    }
}
