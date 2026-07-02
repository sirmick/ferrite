//! Synthesis-driven end-to-end test for `RdsDemod`.
//!
//! Builds a clean MPX signal at 240 kHz: a 19 kHz stereo pilot plus a
//! 57 kHz suppressed-carrier subcarrier amplitude-modulated by a
//! 1 kHz square wave (standing in for the real biphase data envelope
//! — the demod's `data` output is the I-channel post-coherent-mix,
//! not yet decoded biphase symbols). Runs the signal through
//! `RdsDemod` via the public `Block::process` API and asserts on the
//! shape + energy of the produced data stream.
//!
//! Why both this and the in-module unit test in `rds_demod.rs`: the
//! unit test pokes at the block's private power trackers
//! (`i_power()` / `q_power()`) which would shift if the internals
//! get reworked. This integration test only goes through public ports
//! and the public `output_rate_hz()` accessor, so a public-API
//! regression breaks the right place.
//!
//! Note: RDS is a Phase 1 scaffold. The `events` port that will carry
//! decoded RDS groups is wired but inert for now; the hard guarantee
//! today is the data envelope coming out of `data`. This test asserts
//! that.

#![allow(clippy::cast_precision_loss)]

use core::f32::consts::TAU;

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{Block, RdsDemod, RdsDemodParams};

const FS: f32 = 240_000.0;
const PILOT_HZ: f32 = 19_000.0;
const RDS_SUBCARRIER_HZ: f32 = 57_000.0;
const DATA_HZ: f32 = 1_000.0;

fn synth_mpx_at(fs: f32, seconds: f32) -> Vec<f32> {
    let n = (fs * seconds) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / fs;
            let pilot = 0.1 * (TAU * PILOT_HZ * t).sin();
            // Square-wave amplitude-modulating the 57 kHz carrier
            // (DSB-SC). Stands in for biphase-shaped RDS data — the
            // demod's coherent mixer + LPF sees the envelope as a
            // baseband square wave, which is what the test asserts on.
            let sq = if (TAU * DATA_HZ * t).sin() >= 0.0 {
                1.0
            } else {
                -1.0
            };
            let sub = sq * (TAU * RDS_SUBCARRIER_HZ * t).sin();
            pilot + 0.3 * sub
        })
        .collect()
}

fn run_rds_at(fs: f32, mpx: &[f32]) -> (Vec<f32>, f32) {
    let mut block = RdsDemod::new(RdsDemodParams { sample_rate_hz: fs }).expect("rds demod");
    let mut out = vec![0.0_f32; mpx.len()]; // worst-case sized; truncated below
    let mut inputs = [InputPort {
        name: "in",
        meta: PortMeta::default(),
        buf: InBuf::RealF32(mpx),
    }];
    let mut outputs = [OutputPort {
        name: "data",
        meta: PortMeta::default(),
        buf: OutBuf::RealF32(&mut out),
    }];
    let mut io = BlockIo {
        inputs: &mut inputs,
        outputs: &mut outputs,
    };
    let work = block.process(&mut io).unwrap();
    out.truncate(work.produced[0]);
    (out, block.output_rate_hz())
}

/// Drive `mpx` through one `RdsDemod` with a fixed-size `data` dst,
/// looping the way the scheduler does (re-present the unconsumed tail
/// each call). `dst_len = 1` exercises the backpressure path on every
/// emitted sample. Returns the concatenated data output.
fn run_rds_chunked(fs: f32, mpx: &[f32], dst_len: usize) -> Vec<f32> {
    let mut block = RdsDemod::new(RdsDemodParams { sample_rate_hz: fs }).expect("rds demod");
    let mut collected = Vec::new();
    let mut consumed = 0usize;
    let mut guard = 0;
    while consumed < mpx.len() && guard < 10_000_000 {
        guard += 1;
        let mut out = vec![0.0_f32; dst_len];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(&mpx[consumed..]),
        }];
        let mut outputs = [OutputPort {
            name: "data",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let work = block.process(&mut io).unwrap();
        if work.consumed[0] == 0 && work.produced[0] == 0 {
            break;
        }
        collected.extend_from_slice(&out[..work.produced[0]]);
        consumed += work.consumed[0];
    }
    collected
}

fn rms(xs: &[f32]) -> f32 {
    let s: f64 = xs.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    ((s / xs.len() as f64).sqrt()) as f32
}

#[test]
fn rds_demod_recovers_data_envelope_from_synth_mpx() {
    let mpx = synth_mpx_at(FS, 1.0); // 1 s
    let (data, out_rate) = run_rds_at(FS, &mpx);

    // Output rate should be close to 8 × baud (= 8 × 1187.5 = 9500 Hz).
    // Permissive ±1 kHz — the actual decim is integer at 240k/9500 ≈
    // 25, giving 9600 Hz exactly. The contract is "near 9500", not
    // exact.
    assert!(
        (out_rate - 9_500.0).abs() < 1_000.0,
        "output_rate_hz unexpected: {out_rate}",
    );

    // Sample count should match the decimation ratio.
    let expected_decim = (FS / out_rate).round() as usize;
    let expected_n_low = mpx.len() / (expected_decim + 1);
    let expected_n_high = mpx.len() / expected_decim.saturating_sub(1).max(1);
    assert!(
        data.len() >= expected_n_low && data.len() <= expected_n_high,
        "produced {} samples, expected ~{} (decim={})",
        data.len(),
        mpx.len() / expected_decim,
        expected_decim,
    );

    // No NaN / Inf anywhere.
    for &x in &data {
        assert!(x.is_finite(), "non-finite sample in RDS data output");
    }

    // Skip the PLL acquisition + filter warmup transient.
    let warm = data.len() / 4;
    let tail = &data[warm..];
    let r = rms(tail);

    // Once the pilot PLL has locked and the LPF has settled, the
    // square-wave envelope at 1 kHz survives the 2.4 kHz LPF and
    // shows up as a meaningful baseband signal. Threshold is
    // permissive — we want to catch "PLL never locked" / "all-zero
    // output", not pin down a specific amplitude.
    assert!(
        r > 0.05,
        "RDS data RMS too low post-warmup: {r:.4} — PLL probably didn't lock",
    );
}

#[test]
fn rds_demod_decodes_across_sample_rates() {
    // The coherent 57 kHz reference must align with the RDS band at ANY
    // sample rate, not just 240 kHz. The old explicit delay line added a
    // spurious 80-sample offset that only happened to be a whole number
    // of 57 kHz cycles at exactly 240 kHz; at 200/250 kHz it rotated the
    // reference and collapsed the recovered envelope (this test failed at
    // 250 kHz before the delay line was removed).
    for &fs in &[200_000.0_f32, 240_000.0, 250_000.0] {
        let mpx = synth_mpx_at(fs, 1.0);
        let (data, _out_rate) = run_rds_at(fs, &mpx);
        for &x in &data {
            assert!(x.is_finite(), "{fs} Hz: non-finite RDS data sample");
        }
        let tail = &data[data.len() / 4..];
        let r = rms(tail);
        assert!(
            r > 0.05,
            "{fs} Hz: RDS envelope RMS {r:.4} too low — coherent reference misaligned",
        );
    }
}

#[test]
fn rds_demod_backpressure_is_lossless() {
    // A 1-slot `data` dst forces the backpressure path on every emitted
    // sample. With the up-front room check (no pump-then-back-out) the
    // per-sample-chunked run must be byte-identical to the unconstrained
    // run — the old back-out double-pumped a sample, restored the average
    // into the accumulator sum, and dropped `accum_q`/`decim_phase`,
    // corrupting the stream.
    let fs = 240_000.0_f32;
    let mpx = synth_mpx_at(fs, 0.3);
    let (reference, _) = run_rds_at(fs, &mpx);
    let chunked = run_rds_chunked(fs, &mpx, 1);
    assert_eq!(
        chunked.len(),
        reference.len(),
        "backpressure changed the output length ({} vs {})",
        chunked.len(),
        reference.len()
    );
    for (i, (a, b)) in chunked.iter().zip(reference.iter()).enumerate() {
        assert!(
            (a - b).abs() < 1e-6,
            "backpressure corrupted sample {i}: {a} vs {b}",
        );
    }
}
