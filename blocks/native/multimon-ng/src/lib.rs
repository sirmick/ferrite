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

/// Minimum sample count the wrapper hands to a multimon `demod` call.
/// Multimon's correlator-based demods (AFSK1200/2400, FSK9600) read up
/// to `CORRLEN` (≈18 at 22050 Hz) samples per inner-loop iteration but
/// only check `length > 0` per iteration — small caller chunks cause
/// the loop to over-read past the slice end and the bit detector
/// silently fails. 4096 samples (~180 ms at 22050 Hz) sits comfortably
/// above every shipped decoder's threshold (empirically ≥~2700) without
/// adding visible latency for paging / packet bursts that are 200 ms+.
const MIN_BATCH_SAMPLES: usize = 4096;

/// Guard zeros inserted *at* the read boundary (offset
/// `MIN_BATCH_SAMPLES`) before each `demod_fn` call so the C correlator's
/// last-iteration over-read (up to `CORRLEN - SUBSAMP` ≈ 16 samples past
/// `length`) lands on initialised zero memory instead of into the next
/// batch's residual samples. Without this — or with the guard at the
/// *end* of scratch when residual > 0 — the over-read re-processes the
/// first ~18 samples of the next batch, scrambling bit-clock recovery at
/// every batch edge. 32 covers every shipped multimon decoder's CORRLEN
/// with comfortable margin.
const BATCH_GUARD_SAMPLES: usize = 32;

/// Which decoder this instance runs. Adding a decoder = vendor source
/// in `build.rs::decoder_sources()` + bindgen `allowlist_var` + a
/// variant here + a `pub fn from_kind` arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoder {
    /// 512 baud POCSAG paging — 22050 Hz audio in.
    Pocsag512,
    /// 1200 baud POCSAG paging — 22050 Hz audio in. Most common rate.
    Pocsag1200,
    /// 2400 baud POCSAG paging — 22050 Hz audio in.
    Pocsag2400,
    /// Motorola FLEX paging (1600/3200/6400 bps 4-FSK) — 22050 Hz
    /// audio in. Largely replaced POCSAG on US carrier networks.
    Flex,
    /// FLEX with multimon-ng's "next" decoder — same wire format,
    /// different sync / error-recovery internals. Run alongside
    /// [`Self::Flex`] for best capture rate.
    FlexNext,
    /// AFSK 1200 baud — APRS / AX.25 packet radio. The 144.39 MHz
    /// (US) / 144.80 (EU) ham band is full of these. 22050 Hz audio.
    Afsk1200,
    /// AFSK 2400 baud — higher-rate ham packet, MFJ-2400 timing.
    Afsk2400,
    /// AFSK 2400 baud — alternate timing variant 2 (TCM3105).
    Afsk2400_2,
    /// AFSK 2400 baud — alternate timing variant 3 (PSKDET).
    Afsk2400_3,
    /// FSK 9600 baud — G3RUH packet, used on amateur satellites
    /// (PSAT, AISAT-1) and high-rate terrestrial APRS.
    Fsk9600,
    /// Goertzel-based Morse / CW decoder. Audio in via SSB+BFO or
    /// straight tone audio at 22050 Hz; emits decoded text.
    MorseCw,
    /// US Emergency Alert System / SAME headers — the data burst at
    /// the head of a NOAA Weather Radio (162.4–162.55 MHz) alert or
    /// a TV/radio EAS test. Decodes header + locator codes.
    Eas,
    /// Multimon-ng's DTMF decoder. Ferrite ships its own hand-rolled
    /// `DtmfDecoder` block as the primary; this variant exists so a
    /// future parity-test flowgraph can run both side-by-side on the
    /// same audio fixture.
    Dtmf,
}

impl Decoder {
    /// Native input sample rate this decoder expects.
    #[must_use]
    pub fn sample_rate_hz(self) -> u32 {
        // SAFETY: each decoder's `demod_param` is a `const` static
        // emitted by multimon — we just read its samplerate field.
        unsafe {
            match self {
                Self::Pocsag512 => sys::demod_poc5.samplerate,
                Self::Pocsag1200 => sys::demod_poc12.samplerate,
                Self::Pocsag2400 => sys::demod_poc24.samplerate,
                Self::Flex => sys::demod_flex.samplerate,
                Self::FlexNext => sys::demod_flex_next.samplerate,
                Self::Afsk1200 => sys::demod_afsk1200.samplerate,
                Self::Afsk2400 => sys::demod_afsk2400.samplerate,
                Self::Afsk2400_2 => sys::demod_afsk2400_2.samplerate,
                Self::Afsk2400_3 => sys::demod_afsk2400_3.samplerate,
                Self::Fsk9600 => sys::demod_fsk9600.samplerate,
                Self::MorseCw => sys::demod_morse.samplerate,
                Self::Eas => sys::demod_eas.samplerate,
                Self::Dtmf => sys::demod_dtmf.samplerate,
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
                Self::Pocsag512 => sys::demod_poc5.name,
                Self::Pocsag1200 => sys::demod_poc12.name,
                Self::Pocsag2400 => sys::demod_poc24.name,
                Self::Flex => sys::demod_flex.name,
                Self::FlexNext => sys::demod_flex_next.name,
                Self::Afsk1200 => sys::demod_afsk1200.name,
                Self::Afsk2400 => sys::demod_afsk2400.name,
                Self::Afsk2400_2 => sys::demod_afsk2400_2.name,
                Self::Afsk2400_3 => sys::demod_afsk2400_3.name,
                Self::Fsk9600 => sys::demod_fsk9600.name,
                Self::MorseCw => sys::demod_morse.name,
                Self::Eas => sys::demod_eas.name,
                Self::Dtmf => sys::demod_dtmf.name,
            };
            let bytes = std::ffi::CStr::from_ptr(p).to_bytes();
            std::str::from_utf8_unchecked(bytes)
        }
    }

    fn dem_par(self) -> *const sys::demod_param {
        // addr-of a `const` C static — no unsafe needed.
        match self {
            Self::Pocsag512 => &raw const sys::demod_poc5,
            Self::Pocsag1200 => &raw const sys::demod_poc12,
            Self::Pocsag2400 => &raw const sys::demod_poc24,
            Self::Flex => &raw const sys::demod_flex,
            Self::FlexNext => &raw const sys::demod_flex_next,
            Self::Afsk1200 => &raw const sys::demod_afsk1200,
            Self::Afsk2400 => &raw const sys::demod_afsk2400,
            Self::Afsk2400_2 => &raw const sys::demod_afsk2400_2,
            Self::Afsk2400_3 => &raw const sys::demod_afsk2400_3,
            Self::Fsk9600 => &raw const sys::demod_fsk9600,
            Self::MorseCw => &raw const sys::demod_morse,
            Self::Eas => &raw const sys::demod_eas,
            Self::Dtmf => &raw const sys::demod_dtmf,
        }
    }
}

/// POCSAG family runtime knobs, settable from Rust. These are
/// vendor globals defined in `pocsag.c` — there's only one set per
/// process, so settings made here affect every `Decoder::Pocsag*`
/// instance in the runtime.
pub mod pocsag {
    use super::sys;

    /// Show messages even when the BCH(31,21) decoder couldn't fully
    /// repair them. Off by default in multimon (avoids gibberish);
    /// useful while bringing up a new install — at least proves the
    /// decoder is locking on a sync word even if the message bytes
    /// are too garbled to assemble.
    pub fn set_show_partial_decodes(enabled: bool) {
        // SAFETY: thin C-shim setter that just writes the int global
        // declared in vendor/pocsag.c. multimon reads it during
        // decode from the same thread; no race in the runtime's
        // serialised tick model.
        unsafe {
            sys::multimon_pocsag_set_show_partial(i32::from(enabled));
        }
    }

    /// Polarity preference: `0` = auto (try both, decoder picks the
    /// stronger), `1` = normal only, `2` = inverted only. Auto is
    /// almost always right; pin to one to halve the per-burst CPU
    /// once you know the network's polarity.
    pub fn set_polarity(mode: u8) {
        // SAFETY: same single-int-global write justification as above.
        unsafe {
            sys::multimon_pocsag_set_polarity(i32::from(mode));
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
    /// Reusable f32→i16 conversion buffer for the few multimon
    /// decoders that still want int16 PCM (`MORSE_CW`, `X10`). Empty
    /// for the float-input majority. Sized lazily on first push.
    int_scratch: Vec<i16>,
    /// Reusable scratch for the f32 path — multimon's float input is
    /// calibrated against the ±i16::MAX amplitude scale (the float
    /// path was added later as a convenience over the original int16
    /// API and kept the same numerical range), so the wrapper scales
    /// caller-friendly ±1.0 samples up before pushing.
    float_scratch: Vec<f32>,
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
        Self {
            kind,
            state,
            int_scratch: Vec::new(),
            float_scratch: Vec::new(),
        }
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
    /// per-decoder bit timing assumes the exact native rate. Caller's
    /// chunk size is irrelevant: the wrapper accumulates internally
    /// and forwards in [`MIN_BATCH_SAMPLES`]-sized batches (see below).
    ///
    /// Most multimon decoders read floats directly. A few legacy ones
    /// (`MORSE_CW` is the only one we currently wrap, but the same
    /// applies to X10) pre-date multimon's float path and still want
    /// int16 PCM. For those we convert into a scratch buffer and pass
    /// `sbuffer` instead — the wrapper hides the dispatch so callers
    /// only ever see f32.
    ///
    /// Why a minimum batch size: multimon's AFSK1200/2400/9600 demods
    /// over-read their input buffer by up to `CORRLEN` samples per
    /// inner-loop iteration (correlator window > stride). With tiny
    /// caller chunks (e.g. ~10 samples per scheduler tick) the
    /// correlator reads garbage past the slice end and the bit
    /// detector silently fails. Buffering to ≥4 k samples (~180 ms at
    /// 22050 Hz) puts every correlator window safely inside valid
    /// memory and matches the chunk size the offline analyzer uses.
    pub fn push(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        // SAFETY: `dem_par` points at a const C static; `float_samples`
        // is a plain bool field. No aliasing concerns.
        let wants_float = unsafe { (*self.kind.dem_par()).float_samples };

        if wants_float {
            // multimon's float path expects samples in the same numeric
            // range as its native int16 path (±32767-ish), not the ±1
            // convention every other Ferrite block uses.
            self.float_scratch
                .extend(samples.iter().map(|&x| x * f32::from(i16::MAX)));
            self.flush_float_batches();
        } else {
            self.int_scratch.extend(
                samples
                    .iter()
                    .map(|&x| (x * 32_767.0).clamp(-32_768.0, 32_767.0) as i16),
            );
            self.flush_int_batches();
        }
    }

    fn flush_float_batches(&mut self) {
        let dem_par = self.kind.dem_par();
        // SAFETY: const C static.
        let demod_fn = unsafe { (*dem_par).demod };
        let Some(demod_fn) = demod_fn else { return };
        while self.float_scratch.len() >= MIN_BATCH_SAMPLES {
            // Insert guard zeros AT the read boundary (offset
            // MIN_BATCH_SAMPLES) so the C correlator's tail over-read past
            // the batch lands in zeros, not in residual real samples that
            // will be processed in the next iteration. Appending guards at
            // the END of scratch (the previous approach) only worked when
            // residual was zero — with non-aligned chunk sizes the
            // over-read silently re-processed samples and corrupted bit
            // timing at every batch edge.
            self.float_scratch.splice(
                MIN_BATCH_SAMPLES..MIN_BATCH_SAMPLES,
                std::iter::repeat_n(0.0_f32, BATCH_GUARD_SAMPLES),
            );
            let buf = sys::buffer {
                sbuffer: std::ptr::null(),
                fbuffer: self.float_scratch.as_ptr(),
            };
            // SAFETY: `fbuffer` points at scratch[0]; the C demod reads up
            // to MIN_BATCH+BATCH_GUARD samples (all initialised) and
            // advances state by MIN_BATCH/SUBSAMP iterations.
            unsafe {
                demod_fn(self.state.as_mut() as *mut _, buf, MIN_BATCH_SAMPLES as i32);
            }
            // Remove the inserted guard zeros, then drain the processed
            // batch — leaves residual samples (if any) at scratch[0..].
            self.float_scratch
                .drain(MIN_BATCH_SAMPLES..MIN_BATCH_SAMPLES + BATCH_GUARD_SAMPLES);
            self.float_scratch.drain(..MIN_BATCH_SAMPLES);
        }
    }

    fn flush_int_batches(&mut self) {
        let dem_par = self.kind.dem_par();
        let demod_fn = unsafe { (*dem_par).demod };
        let Some(demod_fn) = demod_fn else { return };
        while self.int_scratch.len() >= MIN_BATCH_SAMPLES {
            // Same boundary-guard fix as `flush_float_batches`.
            self.int_scratch.splice(
                MIN_BATCH_SAMPLES..MIN_BATCH_SAMPLES,
                std::iter::repeat_n(0_i16, BATCH_GUARD_SAMPLES),
            );
            let buf = sys::buffer {
                sbuffer: self.int_scratch.as_ptr(),
                fbuffer: std::ptr::null(),
            };
            unsafe {
                demod_fn(self.state.as_mut() as *mut _, buf, MIN_BATCH_SAMPLES as i32);
            }
            self.int_scratch
                .drain(MIN_BATCH_SAMPLES..MIN_BATCH_SAMPLES + BATCH_GUARD_SAMPLES);
            self.int_scratch.drain(..MIN_BATCH_SAMPLES);
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
    fn every_decoder_uses_22050_input() {
        // multimon-ng pins 22050 Hz across every shipped decoder,
        // which is *the* reason a single resampler upstream serves
        // every Ferrite wrapper block. If a future port surfaces a
        // decoder at a different rate, this assertion will catch it
        // before a preset silently feeds garbage.
        for d in [
            Decoder::Pocsag512,
            Decoder::Pocsag1200,
            Decoder::Pocsag2400,
            Decoder::Flex,
            Decoder::FlexNext,
            Decoder::Afsk1200,
            Decoder::Afsk2400,
            Decoder::Afsk2400_2,
            Decoder::Afsk2400_3,
            Decoder::Fsk9600,
            Decoder::MorseCw,
            Decoder::Eas,
            Decoder::Dtmf,
        ] {
            assert_eq!(d.sample_rate_hz(), 22_050, "{:?}", d);
        }
    }

    #[test]
    fn names_round_trip() {
        assert_eq!(Decoder::Pocsag512.name(), "POCSAG512");
        assert_eq!(Decoder::Pocsag1200.name(), "POCSAG1200");
        assert_eq!(Decoder::Pocsag2400.name(), "POCSAG2400");
        assert_eq!(Decoder::Flex.name(), "FLEX");
        assert_eq!(Decoder::FlexNext.name(), "FLEX_NEXT");
        assert_eq!(Decoder::Afsk1200.name(), "AFSK1200");
        assert_eq!(Decoder::Afsk2400.name(), "AFSK2400");
        assert_eq!(Decoder::Afsk2400_2.name(), "AFSK2400_2");
        assert_eq!(Decoder::Afsk2400_3.name(), "AFSK2400_3");
        assert_eq!(Decoder::Fsk9600.name(), "FSK9600");
        assert_eq!(Decoder::MorseCw.name(), "MORSE_CW");
        assert_eq!(Decoder::Eas.name(), "EAS");
        assert_eq!(Decoder::Dtmf.name(), "DTMF");
    }

    #[test]
    fn empty_silence_emits_no_lines() {
        // 1 s of silence at 22050 Hz: nothing to decode, no lines.
        let mut d = MultimonDemod::new(Decoder::Pocsag1200);
        d.push(&vec![0.0_f32; 22_050]);
        assert!(d.drain_lines().is_empty());
    }
}
