//! Offline WSPR decode: read a 120 s window of interleaved f32 I/Q at
//! 375 Hz and dump every spot. Spike oracle for the FFTW→kiss_fft swap
//! and the smoke target for the e2e suite.
//!
//! Usage: `decode-wspr-iq <file.iq>` where the file is
//! `WSPR_WINDOW_SAMPLES` pairs of little-endian f32 (I, Q, I, Q, …).

use std::io::Read;

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        eprintln!("usage: decode-wspr-iq <interleaved-f32-iq-375hz.iq>");
        std::process::exit(2);
    });
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .unwrap_or_else(|e| {
            eprintln!("read {path}: {e}");
            std::process::exit(1);
        });

    let floats: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    // Same front-end conditioning rtlsdr-wsprd's readRawIQfile does
    // before wspr_decode: Q is negated (wsprsim's I/Q convention) and
    // the whole window is normalized to -3 dB (0.5 / max|sample|).
    // Without the normalize the detector's sync thresholds reject
    // everything. This is front-end work — it lives here / in the
    // WsprDemod block, not in the vendored decode core.
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
    eprintln!(
        "loaded {} complex samples ({:.1} s @ {} Hz)",
        i.len(),
        i.len() as f32 / ferrite_wsprd::WSPR_IQ_RATE_HZ as f32,
        ferrite_wsprd::WSPR_IQ_RATE_HZ,
    );

    let spots = ferrite_wsprd::decode_window(&i, &q, 0);
    if spots.is_empty() {
        eprintln!("no WSPR spots decoded");
        std::process::exit(1);
    }
    for s in &spots {
        println!(
            "freq={:+.1}Hz snr={:.0}dB dt={:.1}s drift={:.1}Hz cyc={:<5} | {} | call={} grid={} pwr={}",
            s.freq_hz, s.snr_db, s.dt_s, s.drift_hz, s.fano_cycles, s.message, s.callsign, s.grid, s.power_dbm
        );
    }
}
