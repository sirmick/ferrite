//! Safe Rust wrapper around the vendored `wsprd` WSPR decoder.
//!
//! The C entry point is `wspr_decode(idat, qdat, samples, options,
//! decodes, n_results)` — it takes a full 2-minute WSPR window as
//! complex baseband I/Q at **375 Hz** (`samples == 120 * 375 == 45000`)
//! and runs detection + Fano convolutional decode, returning up to
//! `MAX_UNIQUES` unique spots.
//!
//! The 12 kHz-audio → 375 Hz-I/Q front-end (USB sub-band mix + 32×
//! decimate) lives on the Rust side (the `WsprDemod` block), so the
//! vendored C stays the decode core only — same split as the ft8 crate
//! keeping `audio.c`/`wave.c` out.
//!
//! Native + wasm32: the FFTW dependency is shimmed to `kiss_fft` (see
//! `shim/fftw3.h`); everything here is plain FFI with no host-libc
//! assumptions beyond what wasi-libc provides.

use std::os::raw::{c_char, c_int};

/// WSPR analysis sample rate (Hz). Fixed by the protocol + decoder.
pub const WSPR_IQ_RATE_HZ: u32 = 375;
/// Samples in one WSPR window: 120 s × 375 Hz.
pub const WSPR_WINDOW_SAMPLES: usize = 120 * WSPR_IQ_RATE_HZ as usize;
/// Upstream `MAX_UNIQUES` — caller-side `decoder_results` array length.
const MAX_UNIQUES: usize = 100;

#[repr(C)]
struct DecoderOptions {
    freq: c_int,
    rcall: [c_char; 13],
    rloc: [c_char; 7],
    quickmode: c_int,
    usehashtable: c_int,
    npasses: c_int,
    subtraction: c_int,
}

#[repr(C)]
#[derive(Clone)]
struct DecoderResults {
    freq: f64,
    sync: f32,
    snr: f32,
    dt: f32,
    drift: f32,
    jitter: c_int,
    message: [c_char; 23],
    call: [c_char; 13],
    loc: [c_char; 7],
    pwr: [c_char; 3],
    cycles: c_int,
}

impl Default for DecoderResults {
    fn default() -> Self {
        // All-zero is a valid empty result row (C side overwrites the
        // first `n_results`).
        unsafe { std::mem::zeroed() }
    }
}

extern "C" {
    fn wspr_decode(
        idat: *mut f32,
        qdat: *mut f32,
        samples: c_int,
        options: DecoderOptions,
        decodes: *mut DecoderResults,
        n_results: *mut c_int,
    ) -> c_int;
}

/// Run the C decoder over a prepared window. `wsprd.c` has multi-MB
/// stack frames (`float ps[512][~347]` + per-pass candidate scratch),
/// so on a default 2 MB worker/test thread it overflows. Off-wasm we
/// run it on a thread with a roomy explicit stack — that also keeps a
/// ~100 ms decode off the DSP scheduler thread. On wasm there are no
/// threads; the stack budget is a link-time setting owned by the
/// runtime, so call straight through.
fn run_decode(
    mut idat: Vec<f32>,
    mut qdat: Vec<f32>,
    options: DecoderOptions,
) -> (Vec<DecoderResults>, c_int) {
    let work = move || {
        let mut results = vec![DecoderResults::default(); MAX_UNIQUES];
        let mut n_results: c_int = 0;
        // SAFETY: idat/qdat are exactly WSPR_WINDOW_SAMPLES long;
        // results holds MAX_UNIQUES rows; n_results is a valid slot.
        unsafe {
            wspr_decode(
                idat.as_mut_ptr(),
                qdat.as_mut_ptr(),
                WSPR_WINDOW_SAMPLES as c_int,
                options,
                results.as_mut_ptr(),
                &mut n_results,
            );
        }
        (results, n_results)
    };

    #[cfg(not(target_arch = "wasm32"))]
    {
        std::thread::Builder::new()
            .name("wsprd-decode".into())
            .stack_size(16 * 1024 * 1024)
            .spawn(work)
            .expect("spawn wsprd-decode thread")
            .join()
            .expect("wsprd-decode thread panicked")
    }
    #[cfg(target_arch = "wasm32")]
    {
        work()
    }
}

/// One decoded WSPR spot.
#[derive(Debug, Clone, PartialEq)]
pub struct WsprDecode {
    /// Decoded message text (`CALL GRID dBm`, or hashed/compound forms).
    pub message: String,
    /// Callsign field (may be empty for some message types).
    pub callsign: String,
    /// Maidenhead locator (4-char), empty when not present.
    pub grid: String,
    /// Reported power in dBm (as the protocol's text field).
    pub power_dbm: String,
    /// Audio-band frequency offset of the signal, Hz.
    pub freq_hz: f64,
    /// Estimated SNR in a 2.5 kHz reference bandwidth, dB.
    pub snr_db: f32,
    /// Time offset of the decode within the window, seconds.
    pub dt_s: f32,
    /// Carrier drift over the transmission, Hz.
    pub drift_hz: f32,
    /// Fano decoder cycle count — low = clean copy, high = marginal.
    pub fano_cycles: i32,
}

fn c_str_field(buf: &[c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// Decode one WSPR window. `i`/`q` are complex baseband at
/// [`WSPR_IQ_RATE_HZ`]; they should be [`WSPR_WINDOW_SAMPLES`] long
/// (shorter is zero-padded, longer is truncated — the decoder expects
/// exactly one 120 s slot).
///
/// `dial_freq_hz` only tags the result metadata (used by spot
/// reporting); it does not affect the decode.
pub fn decode_window(i: &[f32], q: &[f32], dial_freq_hz: i32) -> Vec<WsprDecode> {
    let mut idat = vec![0.0f32; WSPR_WINDOW_SAMPLES];
    let mut qdat = vec![0.0f32; WSPR_WINDOW_SAMPLES];
    let n = i.len().min(q.len()).min(WSPR_WINDOW_SAMPLES);
    idat[..n].copy_from_slice(&i[..n]);
    qdat[..n].copy_from_slice(&q[..n]);

    let options = DecoderOptions {
        freq: dial_freq_hz,
        rcall: [0; 13],
        rloc: [0; 7],
        quickmode: 0,
        // No on-disk hashtable: in a daemon we never want wsprd
        // writing `hashtable.txt` into cwd, and under wasm `fopen`
        // returns NULL anyway. In-window callsign hashing still works.
        usehashtable: 0,
        npasses: 2,
        subtraction: 1,
    };

    let (results, n_results) = run_decode(idat, qdat, options);

    let count = (n_results.max(0) as usize).min(MAX_UNIQUES);
    results[..count]
        .iter()
        .map(|r| WsprDecode {
            message: c_str_field(&r.message),
            callsign: c_str_field(&r.call),
            grid: c_str_field(&r.loc),
            power_dbm: c_str_field(&r.pwr),
            freq_hz: r.freq,
            snr_db: r.snr,
            dt_s: r.dt,
            drift_hz: r.drift,
            fano_cycles: r.cycles,
        })
        .collect()
}
