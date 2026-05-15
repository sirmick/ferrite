//! End-to-end WSPR decode against the wsprsim reference signal.
//!
//! Loads `samples/sigidwiki/WSPR_refSignal_0dB_iq-f32-375hz.iq` (one
//! 120 s slot of complex baseband at 375 Hz, interleaved f32 — the
//! rtlsdr-wsprd test vector, 0 dB SNR, GPL-3.0), applies the same
//! front-end conditioning the `WsprDemod` block does (Q sign flip +
//! −3 dB peak normalize), and asserts the vendored decode core
//! recovers the canonical `K1JT FN20 20`.
//!
//! This is the regression oracle for the FFTW→kiss_fft swap: a WSPR
//! decode is self-validating (the K=32 r=½ Fano decode + 162-symbol
//! sync won't yield a well-formed callsign from a broken FFT). Block-
//! layer concerns (spec / construct / slot timing) are covered by the
//! unit tests in `blocks/src/wspr.rs`.

#![cfg(feature = "wspr")]

use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

fn read_ref_iq() -> (Vec<f32>, Vec<f32>) {
    let path: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "samples",
        "sigidwiki",
        "WSPR_refSignal_0dB_iq-f32-375hz.iq",
    ]
    .iter()
    .collect();
    let mut bytes = Vec::new();
    File::open(&path)
        .unwrap_or_else(|e| panic!("open ref iq: {e} (path={})", path.display()))
        .read_to_end(&mut bytes)
        .unwrap();

    let floats: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    // Same conditioning WsprDemod::drain_decodes / the front-end do:
    // Q is negated (wsprsim I/Q convention), then the whole window is
    // peak-normalized to −3 dB (the detector keys its sync thresholds
    // off absolute amplitude — without this it decodes nothing).
    let mut i = Vec::with_capacity(floats.len() / 2);
    let mut q = Vec::with_capacity(floats.len() / 2);
    for pair in floats.chunks_exact(2) {
        i.push(pair[0]);
        q.push(-pair[1]);
    }
    let mut max_sig = 1e-24f32;
    for (&iv, &qv) in i.iter().zip(q.iter()) {
        max_sig = max_sig.max(iv.abs()).max(qv.abs());
    }
    let scale = 0.5 / max_sig;
    for v in i.iter_mut().chain(q.iter_mut()) {
        *v *= scale;
    }
    (i, q)
}

#[test]
fn reference_signal_decodes_to_k1jt() {
    let (i, q) = read_ref_iq();
    assert_eq!(
        i.len(),
        ferrite_wsprd::WSPR_WINDOW_SAMPLES,
        "reference vector should be exactly one 120 s @ 375 Hz window",
    );

    let spots = ferrite_wsprd::decode_window(&i, &q, 0);
    assert!(
        !spots.is_empty(),
        "no WSPR spots decoded — FFTW→kiss_fft swap or front-end conditioning regressed",
    );

    let k1jt = spots
        .iter()
        .find(|s| s.callsign == "K1JT")
        .unwrap_or_else(|| panic!("K1JT not among decodes: {spots:?}"));
    assert_eq!(k1jt.callsign, "K1JT");
    assert_eq!(k1jt.grid, "FN20");
    assert_eq!(k1jt.power_dbm, "20");
    assert_eq!(k1jt.message, "K1JT FN20 20");
    // Sanity: a clean 0 dB copy should not be a marginal Fano decode.
    assert!(
        k1jt.fano_cycles > 0 && k1jt.fano_cycles < 10_000,
        "implausible Fano cycle count {} — decode is marginal/garbage",
        k1jt.fano_cycles,
    );
}
