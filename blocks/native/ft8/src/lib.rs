//! Safe Rust wrappers for vendored `ft8_lib`.
//!
//! Two layers:
//! - `ffi` — `extern "C"` declarations matching the upstream API + the
//!   small glue shim under `shim/glue.c`. `monitor_t` is opaque on
//!   the Rust side; we never mirror upstream's concrete layout.
//! - `Monitor` / `Decoder` — safe wrappers. A `Monitor` ingests 12 kHz
//!   mono `f32` audio; on a slot boundary the caller calls
//!   [`Monitor::decode_slot`] which returns the list of decoded
//!   messages this slot.
//!
//! The streaming pattern from upstream's `demo/decode_ft8.c`:
//!
//! ```text
//! monitor_init(&mon, &cfg);
//! while (audio):
//!     monitor_process(&mon, &samples[i]);  // every block_size samples
//! decode(&mon);                            // at slot boundary
//! ```
//!
//! Mapping to this crate:
//!
//! ```ignore
//! use ferrite_ft8::{Monitor, MonitorConfig, Protocol};
//! let mut mon = Monitor::new(&MonitorConfig::ft8_default()).unwrap();
//! while have_samples {
//!     mon.process_chunk(&samples)?;       // chunked at block_size internally
//! }
//! for decoded in mon.decode_slot(20) {
//!     println!("{:.0} Hz  ±{:.2} s  SNR {:.0}  {}",
//!         decoded.freq_hz, decoded.time_offset_s, decoded.snr_db, decoded.text);
//! }
//! ```

#![allow(non_camel_case_types, non_snake_case, non_upper_case_globals)]

use std::ffi::{c_char, c_float, c_int, c_void};
use std::ptr::NonNull;

#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// --- raw FFI -------------------------------------------------------------

/// FT4 / FT8 protocol selector. Matches upstream's `ftx_protocol_t`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Protocol {
    /// FT4 — 7.5-second slot, 4-FSK.
    Ft4 = 0,
    /// FT8 — 15-second slot, 8-FSK. The common case.
    Ft8 = 1,
}

/// Mirror of upstream's `monitor_config_t`. All fields match upstream
/// names + types; `repr(C)` ensures we hand it through to
/// `ferrite_monitor_create` byte-for-byte.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct MonitorConfig {
    pub f_min: c_float,
    pub f_max: c_float,
    pub sample_rate: c_int,
    pub time_osr: c_int,
    pub freq_osr: c_int,
    pub protocol: Protocol,
}

impl MonitorConfig {
    /// Reasonable defaults for FT8 against a 12 kHz audio rate. Mirrors
    /// what `demo/decode_ft8.c` uses: 100–3000 Hz analysis band, 2× time
    /// + 2× frequency oversampling.
    #[must_use]
    pub const fn ft8_default() -> Self {
        Self {
            f_min: 100.0,
            f_max: 3000.0,
            sample_rate: 12000,
            time_osr: 2,
            freq_osr: 2,
            protocol: Protocol::Ft8,
        }
    }

    /// FT4 variant — 7.5 s slot, same audio rate.
    #[must_use]
    pub const fn ft4_default() -> Self {
        Self {
            protocol: Protocol::Ft4,
            ..Self::ft8_default()
        }
    }
}

/// Opaque waterfall struct (`ftx_waterfall_t` upstream). The decoder
/// API takes a const pointer to one of these; we obtain it from
/// `ferrite_monitor_waterfall`.
#[repr(C)]
pub struct ftx_waterfall_t {
    _opaque: [u8; 0],
}

/// Candidate as found by `ftx_find_candidates`. Mirror of upstream's
/// `ftx_candidate_t` — used by value, so the layout matters.
#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct ftx_candidate_t {
    pub score: i16,
    pub time_offset: i16,
    pub freq_offset: i16,
    pub time_sub: u8,
    pub freq_sub: u8,
}

/// Decoded message status — mirror of upstream's
/// `ftx_decode_status_t`. We surface `freq` + `time` (the candidate's
/// resolved frequency in Hz and time offset in s) plus the LDPC
/// iteration count for diagnostics.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct ftx_decode_status_t {
    pub freq: c_float,
    pub time: c_float,
    pub ldpc_errors: c_int,
    pub crc_extracted: u16,
    pub crc_calculated: u16,
}

/// Encoded payload of a decoded FT8 message. Upstream uses the 10-byte
/// payload buffer + a 16-bit hash for dedup; the human-readable text
/// is produced separately via `ftx_message_decode`.
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct ftx_message_t {
    pub payload: [u8; 10],
    pub hash: u16,
}

/// Per-message return code from `ftx_message_decode`. We only check
/// for `FTX_MESSAGE_RC_OK == 0`; the others are diagnostic.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FtxMessageRc {
    Ok = 0,
    ErrorCallsign1 = 1,
    ErrorCallsign2 = 2,
    ErrorSuffix = 3,
    ErrorGrid = 4,
    ErrorType = 5,
}

/// Parallel-array offset table populated by `ftx_message_decode`. We
/// don't surface this to users — we just need a buffer to pass.
#[repr(C)]
#[derive(Copy, Clone, Default)]
pub struct ftx_message_offsets_t {
    pub types: [c_int; 3],
    pub offsets: [i16; 3],
}

extern "C" {
    // --- glue (shim/glue.c) ---
    fn ferrite_monitor_create(cfg: *const MonitorConfig) -> *mut c_void;
    fn ferrite_monitor_destroy(mon: *mut c_void);
    fn ferrite_monitor_reset(mon: *mut c_void);
    fn ferrite_monitor_process(mon: *mut c_void, frame: *const c_float);
    fn ferrite_monitor_block_size(mon: *const c_void) -> c_int;
    fn ferrite_monitor_blocks_filled(mon: *const c_void) -> c_int;
    fn ferrite_monitor_blocks_max(mon: *const c_void) -> c_int;
    fn ferrite_monitor_waterfall(mon: *const c_void) -> *const ftx_waterfall_t;
    fn ferrite_monitor_min_bin(mon: *const c_void) -> c_int;
    fn ferrite_monitor_symbol_period(mon: *const c_void) -> c_float;
    fn ferrite_monitor_freq_osr(mon: *const c_void) -> c_int;
    fn ferrite_monitor_time_osr(mon: *const c_void) -> c_int;

    // --- decoder (vendor/ft8/decode.h) ---
    fn ftx_find_candidates(
        power: *const ftx_waterfall_t,
        num_candidates: c_int,
        heap: *mut ftx_candidate_t,
        min_score: c_int,
    ) -> c_int;
    fn ftx_decode_candidate(
        power: *const ftx_waterfall_t,
        cand: *const ftx_candidate_t,
        max_iterations: c_int,
        message: *mut ftx_message_t,
        status: *mut ftx_decode_status_t,
    ) -> bool;

    // --- message text decode (vendor/ft8/message.h) ---
    // hash_if = NULL is accepted for the simple decode path.
    fn ftx_message_decode(
        msg: *const ftx_message_t,
        hash_if: *mut c_void,
        message: *mut c_char,
        offsets: *mut ftx_message_offsets_t,
    ) -> FtxMessageRc;
}

/// Safe owner of an `ft8_lib` `monitor_t`. RAII'd via
/// `ferrite_monitor_destroy`.
pub struct Monitor {
    inner: NonNull<c_void>,
    block_size: usize,
    /// Waterfall geometry captured at construction, used to resolve a
    /// candidate's bin/sub indices into absolute audio Hz / seconds.
    /// `ftx_decode_candidate` leaves `status.freq`/`status.time` at 0
    /// on the public decode path, so we compute them ourselves the way
    /// upstream's demo does.
    min_bin: i32,
    symbol_period: f32,
    freq_osr: i32,
    time_osr: i32,
}

unsafe impl Send for Monitor {}

/// One decoded FT8 / FT4 message, with diagnostics — what an offline
/// log line would carry plus the structured candidate metadata.
#[derive(Debug, Clone)]
pub struct Decoded {
    /// Audio-passband frequency of the signal (Hz), e.g. ~1500. Add
    /// the receiver dial frequency to recover absolute RF.
    pub freq_hz: f32,
    /// Time offset of the first symbol within the slot (seconds).
    pub time_offset_s: f32,
    /// SNR estimate in dB — derived from the candidate score (upstream
    /// publishes it on the score field after CRC validation).
    pub snr_db: f32,
    /// Human-readable message body, e.g. `"CQ W9XYZ EN37"`.
    pub text: String,
    /// Number of LDPC iterations the decoder spent — useful for
    /// triage when borderline messages flicker.
    pub ldpc_errors: i32,
}

impl Monitor {
    /// Initialise a fresh monitor for the given config. Returns
    /// `None` on allocation failure (rare; only if libc malloc itself
    /// fails).
    #[must_use]
    pub fn new(cfg: &MonitorConfig) -> Option<Self> {
        // SAFETY: `cfg` is a valid `repr(C)` mirror of upstream's struct;
        // the glue function never reads past the layout we declared.
        let raw = unsafe { ferrite_monitor_create(std::ptr::from_ref(cfg)) };
        let inner = NonNull::new(raw)?;
        // block_size is a C `int` that always reports a small positive
        // FFT block size (a few hundred samples) — the sign-loss cast
        // is bounded and harmless.
        #[allow(clippy::cast_sign_loss)]
        let block_size = unsafe { ferrite_monitor_block_size(raw.cast_const()) } as usize;
        // SAFETY: `raw` is the just-created monitor; these getters only
        // read scalar fields off it and never retain the pointer.
        let (min_bin, symbol_period, freq_osr, time_osr) = unsafe {
            let r = raw.cast_const();
            (
                ferrite_monitor_min_bin(r),
                ferrite_monitor_symbol_period(r),
                ferrite_monitor_freq_osr(r),
                ferrite_monitor_time_osr(r),
            )
        };
        Some(Self {
            inner,
            block_size,
            min_bin,
            symbol_period,
            freq_osr: freq_osr.max(1),
            time_osr: time_osr.max(1),
        })
    }

    /// Block size in `f32` samples. The monitor consumes one block at
    /// a time; the safe API chunks `process_chunk` for you.
    #[must_use]
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Number of completed FFT blocks currently sitting in the
    /// waterfall. Hits `blocks_max` once a full slot has been
    /// ingested — that's when [`Self::decode_slot`] will yield
    /// candidates.
    #[must_use]
    #[allow(clippy::cast_sign_loss)] // C int, always >= 0 in this API.
    pub fn blocks_filled(&self) -> usize {
        unsafe { ferrite_monitor_blocks_filled(self.inner.as_ptr().cast_const()) as usize }
    }

    /// Maximum slot size the monitor was sized for, in FFT blocks.
    /// FT8 = 92 blocks (15 s @ 6.25 Hz tone spacing).
    #[must_use]
    #[allow(clippy::cast_sign_loss)] // C int, always >= 0 in this API.
    pub fn blocks_max(&self) -> usize {
        unsafe { ferrite_monitor_blocks_max(self.inner.as_ptr().cast_const()) as usize }
    }

    /// Reset the rolling waterfall — call between slot boundaries so
    /// the next decode operates on fresh samples only.
    pub fn reset(&mut self) {
        unsafe { ferrite_monitor_reset(self.inner.as_ptr()) }
    }

    /// Feed one block of `block_size` samples. The monitor pushes the
    /// resulting STFT row into its waterfall. Out-of-range chunks
    /// (length != `block_size`) are rejected so a caller bug doesn't
    /// scribble past the C-side stack frame.
    ///
    /// # Errors
    /// Returns `Err` if `samples.len() != self.block_size()`.
    pub fn process_block(&mut self, samples: &[f32]) -> Result<(), &'static str> {
        if samples.len() != self.block_size {
            return Err("ft8::Monitor::process_block: chunk size != monitor.block_size");
        }
        unsafe { ferrite_monitor_process(self.inner.as_ptr(), samples.as_ptr()) };
        Ok(())
    }

    /// Drain the waterfall via `ftx_find_candidates` + `ftx_decode_candidate`,
    /// returning every successfully-decoded message. `max_candidates`
    /// caps the candidate heap (40 is what upstream's demo uses; lower
    /// is faster, higher catches more weak signals).
    #[must_use]
    #[allow(clippy::cast_sign_loss)]
    // n_found >= 0 verified before use.
    // min_bin (~hundreds) and the OSR factors (2) are tiny ints — the
    // i32→f32 conversions in the freq/time formula are exact here.
    #[allow(clippy::cast_precision_loss)]
    pub fn decode_slot(&self, max_candidates: usize) -> Vec<Decoded> {
        let wf = unsafe { ferrite_monitor_waterfall(self.inner.as_ptr().cast_const()) };
        if wf.is_null() {
            return Vec::new();
        }
        // Min-heap of candidates ranked by Costas-sync score.
        let mut heap: Vec<ftx_candidate_t> = vec![
            ftx_candidate_t {
                score: 0,
                time_offset: 0,
                freq_offset: 0,
                time_sub: 0,
                freq_sub: 0,
            };
            max_candidates.max(1)
        ];
        // `heap.len()` is bounded by `max_candidates` (caller-supplied
        // small int); the i32 cast can't truncate in practice.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let heap_len_c = heap.len() as c_int;
        let n_found = unsafe {
            ftx_find_candidates(
                wf,
                heap_len_c,
                heap.as_mut_ptr(),
                10, // min_score: upstream's demo uses 10 as the prune threshold.
            )
        };
        if n_found <= 0 {
            return Vec::new();
        }

        let mut out = Vec::with_capacity(n_found as usize);
        for cand in &heap[..n_found as usize] {
            let mut msg = ftx_message_t::default();
            let mut status = ftx_decode_status_t::default();
            // 20 LDPC iters: the demo's default. Higher rescues weaker
            // signals at quadratic cost — over ~50 iters yields stop
            // mattering.
            let ok = unsafe {
                ftx_decode_candidate(
                    wf,
                    std::ptr::from_ref(cand),
                    20,
                    std::ptr::from_mut(&mut msg),
                    std::ptr::from_mut(&mut status),
                )
            };
            if !ok {
                continue;
            }
            // Decode payload to text. 35-byte buffer per upstream
            // FTX_MAX_MESSAGE_LENGTH; we add a NUL guard for safety.
            let mut buf = [0i8; 64];
            let mut offsets = ftx_message_offsets_t::default();
            let rc = unsafe {
                ftx_message_decode(
                    std::ptr::from_ref(&msg),
                    std::ptr::null_mut(),
                    buf.as_mut_ptr().cast::<c_char>(),
                    std::ptr::from_mut(&mut offsets),
                )
            };
            if rc != FtxMessageRc::Ok {
                continue;
            }
            let text = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr().cast::<c_char>()) }
                .to_string_lossy()
                .trim()
                .to_string();
            if text.is_empty() {
                continue;
            }
            // `ftx_decode_candidate` leaves `status.freq`/`status.time`
            // at 0 on this public path, so resolve them from the
            // candidate's bin/sub indices exactly as upstream's demo
            // does (vendor/decode_ft8_reference.c:153-154):
            //   freq_hz  = (min_bin + freq_offset + freq_sub/freq_osr) / symbol_period
            //   time_sec = (time_offset + time_sub/time_osr) * symbol_period
            let freq_hz = (self.min_bin as f32
                + f32::from(cand.freq_offset)
                + f32::from(cand.freq_sub) / self.freq_osr as f32)
                / self.symbol_period;
            let time_offset_s = (f32::from(cand.time_offset)
                + f32::from(cand.time_sub) / self.time_osr as f32)
                * self.symbol_period;
            out.push(Decoded {
                freq_hz,
                time_offset_s,
                // Convert candidate score into an SNR estimate the way
                // upstream's printf path does (score / 2 - 24 dB).
                snr_db: f32::from(cand.score) * 0.5 - 24.0,
                text,
                ldpc_errors: status.ldpc_errors,
            });
        }
        // Drop duplicate hashes (multiple candidates can decode to the
        // same message body); we'd rather emit each text once per slot.
        out.sort_by(|a, b| {
            b.snr_db
                .partial_cmp(&a.snr_db)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.dedup_by(|a, b| a.text == b.text);
        out
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        // SAFETY: `self.inner` came from `ferrite_monitor_create`; the
        // glue function pairs that with `ferrite_monitor_destroy`, which
        // calls upstream's `monitor_free` and frees the wrapper. No
        // double-free path because `Self` owns the pointer exclusively
        // (no Clone, no Copy).
        unsafe { ferrite_monitor_destroy(self.inner.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::{version, Monitor, MonitorConfig, Protocol};

    #[test]
    fn version_is_set() {
        assert!(!version().is_empty());
    }

    #[test]
    fn ft8_monitor_inits_and_drops_clean() {
        // No samples processed — just constructor + RAII drop. Proves
        // the C build links and `monitor_init` doesn't immediately
        // segfault on a stock config.
        let cfg = MonitorConfig::ft8_default();
        assert_eq!(cfg.protocol, Protocol::Ft8);
        let m = Monitor::new(&cfg).expect("Monitor::new returned None");
        assert!(m.block_size() > 0, "block_size should be positive");
        assert!(m.blocks_max() > 0, "blocks_max should be positive");
        assert_eq!(m.blocks_filled(), 0);
        // drop runs ferrite_monitor_destroy
    }

    #[test]
    fn process_block_silence_increases_filled_count() {
        // Feed a few blocks of zeros; the waterfall row counter
        // should advance. We don't expect any decodes (silence has no
        // signal), but the path through monitor_process is exercised.
        let mut m = Monitor::new(&MonitorConfig::ft8_default()).unwrap();
        let block = vec![0.0_f32; m.block_size()];
        for _ in 0..5 {
            m.process_block(&block).unwrap();
        }
        assert_eq!(m.blocks_filled(), 5);
        let decoded = m.decode_slot(20);
        assert!(decoded.is_empty(), "silence should not decode to anything");
    }

    #[test]
    fn rejects_wrong_block_size() {
        let mut m = Monitor::new(&MonitorConfig::ft8_default()).unwrap();
        let bad = vec![0.0_f32; m.block_size() + 1];
        assert!(m.process_block(&bad).is_err());
    }
}
