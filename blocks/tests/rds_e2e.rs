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

fn synth_mpx(seconds: f32) -> Vec<f32> {
    let n = (FS * seconds) as usize;
    (0..n)
        .map(|i| {
            let t = i as f32 / FS;
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

fn run_rds(mpx: &[f32]) -> (Vec<f32>, f32) {
    let mut block = RdsDemod::new(RdsDemodParams { sample_rate_hz: FS }).expect("rds demod");
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

fn rms(xs: &[f32]) -> f32 {
    let s: f64 = xs.iter().map(|&x| f64::from(x) * f64::from(x)).sum();
    ((s / xs.len() as f64).sqrt()) as f32
}

#[test]
fn rds_demod_recovers_data_envelope_from_synth_mpx() {
    let mpx = synth_mpx(1.0); // 1 s
    let (data, out_rate) = run_rds(&mpx);

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
