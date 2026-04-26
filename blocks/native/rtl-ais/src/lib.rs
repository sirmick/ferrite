//! Safe Rust wrapper around the vendored aisdecoder (marine AIS at
//! 161.975 + 162.025 MHz).
//!
//! ### Surface
//!
//! - [`RtlAis::new`] lazily inits the decoder on first call (subsequent
//!   constructions reuse it — the underlying state is process-wide).
//! - [`RtlAis::push_audio`] takes two synchronised real-f32 audio slices
//!   (channel A and channel B, both at 48 kHz), interleaves them as s16
//!   stereo, and forwards into the C side. Each leg is run through an
//!   independent GMSK clock-recovery PLL + AIVDM frame builder; AIS
//!   frames decoded inside aisdecoder land in a per-process queue.
//! - [`RtlAis::drain_lines`] empties that queue, returning the
//!   accumulated NMEA sentences split on `\n`. Same envelope as
//!   `MultimonDemod::drain_lines` and `Dump1090::drain_lines`.
//!
//! ### Sample-rate contract
//!
//! Hard 48 kHz on each leg. The aisdecoder receiver-side PLL uses
//! `pllinc = 0x10000 / 5` per bit at 9600 baud, which assumes 48000 / 9600
//! = 5 samples per bit; off-rate audio decodes as garbage. The
//! `AdsbDemod`-style rate check on the wrapping block surfaces the
//! mismatch in the log.
//!
//! ### Threading
//!
//! `RtlAis` is `Send` + `!Sync` — same model as the multimon and dump1090
//! wrappers. The C state is process-wide so don't construct more than
//! one instance.

#![allow(unsafe_op_in_unsafe_fn)]

mod sys {
    //! Raw bindgen output. Hidden behind the safe API above.
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(non_upper_case_globals)]
    #![allow(dead_code)]
    #![allow(clippy::all)]
    #![allow(clippy::pedantic)]
    include!(concat!(env!("OUT_DIR"), "/ais_bindings.rs"));
}

/// Native input rate the aisdecoder expects on each leg. Hard contract
/// — the GMSK PLL is sized for exactly this rate.
pub const AIS_INPUT_RATE_HZ: u32 = 48_000;

/// Safe handle around the global aisdecoder state.
pub struct RtlAis {
    /// s16 interleaved scratch the audio-conversion loop writes into
    /// before calling the shim. Reused across pushes (each tick at
    /// 48 kHz feeds ~480 stereo pairs = 1920 bytes — sized to absorb
    /// generous chunks without realloc).
    scratch: Vec<i16>,
}

impl RtlAis {
    /// Construct a handle. Lazily runs the C-side global init on first
    /// call; safe to call multiple times.
    #[must_use]
    pub fn new() -> Self {
        // SAFETY: `ais_init` has an internal idempotent guard.
        unsafe {
            sys::ais_init();
        }
        Self {
            scratch: Vec::with_capacity(8192),
        }
    }

    /// Push one block of audio. `ch_a` and `ch_b` must be the same
    /// length; mismatched lengths get truncated to the shorter. Each
    /// leg is f32 in the conventional [-1, 1] range.
    ///
    /// Conversion: f32 → s16 with saturating cast. aisdecoder is robust
    /// to the int-precision loss; the GMSK transitions it tracks are
    /// well above the noise floor on any decoded burst.
    pub fn push_audio(&mut self, ch_a: &[f32], ch_b: &[f32]) {
        let n = ch_a.len().min(ch_b.len());
        if n == 0 {
            return;
        }
        self.scratch.clear();
        self.scratch.reserve(n * 2);
        for i in 0..n {
            self.scratch.push(f32_to_i16(ch_a[i]));
            self.scratch.push(f32_to_i16(ch_b[i]));
        }
        // SAFETY: scratch holds 2*n owned valid i16s; the shim copies
        // before returning so the lifetime ends here.
        unsafe {
            sys::ais_push_audio(self.scratch.as_ptr(), n);
        }
    }

    /// Drain any complete decoded AIVDM sentences emitted since the
    /// last call. Returns one `String` per `\n`-terminated chunk.
    #[must_use]
    pub fn drain_lines(&mut self) -> Vec<String> {
        // 64 KB matches the convention used by the other decoder wraps.
        // Even a busy port (hundreds of vessels) emits at most a few
        // thousand bytes per second — comfortably under cap.
        let mut buf = vec![0_u8; 65_536];
        // SAFETY: dst pointer + cap valid; the shim writes ≤ cap bytes.
        let n = unsafe { sys::ais_drain(buf.as_mut_ptr().cast(), buf.len()) };
        buf.truncate(n);
        if buf.is_empty() {
            return Vec::new();
        }
        let s = String::from_utf8_lossy(&buf);
        s.split('\n')
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    /// Reset the message queue. Used between unrelated capture sessions
    /// (the offline analyzer A/B-ing different fixtures); not needed in
    /// live use.
    pub fn reset(&mut self) {
        // SAFETY: idempotent if not yet initialised; otherwise just
        // drains and frees the queued message strings.
        unsafe { sys::ais_reset() };
    }
}

impl Default for RtlAis {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: the aisdecoder state is process-wide; one instance must be
// created at most. Within that constraint, moving the Rust handle
// between threads is fine — same model as `MultimonDemod` / `Dump1090`.
unsafe impl Send for RtlAis {}

#[inline]
fn f32_to_i16(x: f32) -> i16 {
    let scaled = x * 32767.0;
    if scaled >= 32767.0 {
        i16::MAX
    } else if scaled <= -32768.0 {
        i16::MIN
    } else {
        scaled as i16
    }
}

#[cfg(test)]
mod tests {
    use super::{f32_to_i16, RtlAis, AIS_INPUT_RATE_HZ};

    #[test]
    fn rate_constant_is_48k() {
        assert_eq!(AIS_INPUT_RATE_HZ, 48_000);
    }

    #[test]
    fn f32_to_i16_saturates() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(-1.0), -32767); /* 32767, not 32768 */
        assert_eq!(f32_to_i16(2.0), i16::MAX);
        assert_eq!(f32_to_i16(-2.0), i16::MIN);
        assert_eq!(f32_to_i16(f32::INFINITY), i16::MAX);
        assert_eq!(f32_to_i16(f32::NEG_INFINITY), i16::MIN);
    }

    #[test]
    fn silence_does_not_panic_or_emit() {
        let mut d = RtlAis::new();
        // 0.1 s of silence at 48 kHz on each leg.
        let zeros = vec![0.0_f32; 4_800];
        d.push_audio(&zeros, &zeros);
        let lines = d.drain_lines();
        assert!(lines.is_empty(), "silence emitted {} lines", lines.len());
    }
}
