//! RDS throughput probe on a real off-air capture (audit P5-H3).
//!
//! Runs the wbfm preset's RDS chain block-by-block —
//! `FmDemod(240 kHz, 75 kHz dev) → RdsDemod(240 kHz)` (the preset's
//! `RssiProbe` / `TeeRealF32` in between are pass-throughs) — on a cf32
//! capture of the channelizer output and prints groups/s in 1-s
//! buckets, block-sync losses, and the final PI / PS / PTY / RT.
//! Ideal throughput is 1187.5 / 104 ≈ 11.4 groups/s.
//!
//! The fixture (38 MB) is not in the repo, so this is `#[ignore]`d and
//! reads its path from `FERRITE_RDS_FIXTURE`:
//!
//! ```text
//! FERRITE_RDS_FIXTURE=~/.cache/ferrite-fixtures/rds_98500k_240k.cf32 \
//!   cargo test --release -p ferrite-blocks --test rds_fixture_probe -- --ignored --nocapture
//! ```
//!
//! Optional `FERRITE_RDS_PPM` resamples the IQ by `1 + ppm·1e-6`
//! (linear interpolation) to emulate a sample-clock error on top of
//! whatever the capture already carries.

#![allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]

use ferrite_blocks::block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
use ferrite_blocks::{Block, FmDemod, FmDemodParams, RdsDemod, RdsDemodParams};
use num_complex::Complex;

const FS: f32 = 240_000.0;

fn read_cf32(path: &str) -> Vec<Complex<f32>> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    bytes
        .chunks_exact(8)
        .map(|c| {
            Complex::new(
                f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
            )
        })
        .collect()
}

fn resample(iq: &[Complex<f32>], ppm: f64) -> Vec<Complex<f32>> {
    let step = 1.0 + ppm * 1e-6;
    let mut out = Vec::with_capacity(iq.len());
    let mut t = 0.0_f64;
    while (t as usize) + 1 < iq.len() {
        let i = t as usize;
        let f = (t - i as f64) as f32;
        out.push(iq[i] * (1.0 - f) + iq[i + 1] * f);
        t += step;
    }
    out
}

fn fm_demod(iq: &[Complex<f32>]) -> Vec<f32> {
    let mut fm = FmDemod::new(FmDemodParams {
        sample_rate_hz: FS,
        max_deviation_hz: 75_000.0,
    })
    .unwrap();
    let mut out = vec![0.0_f32; iq.len()];
    let mut ins = [InputPort {
        name: "in",
        meta: PortMeta::default(),
        buf: InBuf::IqF32(iq),
    }];
    let mut outs = [OutputPort {
        name: "out",
        meta: PortMeta::default(),
        buf: OutBuf::RealF32(&mut out),
    }];
    let w = fm
        .process(&mut BlockIo {
            inputs: &mut ins,
            outputs: &mut outs,
        })
        .unwrap();
    assert_eq!(w.consumed[0], iq.len());
    out
}

/// Feed one chunk through `RdsDemod` (looping until consumed) and
/// return the event bytes it emitted.
fn rds_feed(rds: &mut RdsDemod, mpx: &[f32]) -> Vec<u8> {
    let mut events = Vec::new();
    let mut cur = 0;
    while cur < mpx.len() {
        let mut data = vec![0.0_f32; 4096];
        let mut ev = vec![0u8; 4096];
        let mut ins = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(&mpx[cur..]),
        }];
        let mut outs = [
            OutputPort {
                name: "data",
                meta: PortMeta::default(),
                buf: OutBuf::RealF32(&mut data),
            },
            OutputPort {
                name: "events",
                meta: PortMeta::default(),
                buf: OutBuf::Events(&mut ev),
            },
        ];
        let w = rds
            .process(&mut BlockIo {
                inputs: &mut ins,
                outputs: &mut outs,
            })
            .unwrap();
        events.extend_from_slice(&ev[..w.produced[1]]);
        assert!(w.consumed[0] > 0, "RdsDemod made no progress");
        cur += w.consumed[0];
    }
    events
}

#[test]
#[ignore = "needs FERRITE_RDS_FIXTURE (38 MB off-air capture)"]
fn rds_fixture_group_rate() {
    let Ok(path) = std::env::var("FERRITE_RDS_FIXTURE") else {
        eprintln!("FERRITE_RDS_FIXTURE unset — skipping");
        return;
    };
    let mut iq = read_cf32(&path);
    if let Ok(ppm) = std::env::var("FERRITE_RDS_PPM") {
        let ppm: f64 = ppm.parse().expect("FERRITE_RDS_PPM");
        iq = resample(&iq, ppm);
        eprintln!("resampled by {ppm:+} ppm");
    }
    let mpx = fm_demod(&iq);
    let secs = mpx.len() / FS as usize;
    eprintln!("{path}: {:.2} s of IQ", mpx.len() as f32 / FS);

    let mut rds = RdsDemod::new(RdsDemodParams { sample_rate_hz: FS }).unwrap();
    let mut prev = rds.diag();
    let mut per_sec = Vec::new();
    let mut events = 0usize;
    eprintln!(" sec  groups  blocks  losses  I/Q(dB)");
    for (s, chunk) in mpx.chunks(FS as usize).enumerate() {
        let ev = rds_feed(&mut rds, chunk);
        events += ev.iter().filter(|&&b| b == b'\n').count();
        let d = rds.diag();
        let g = d.groups - prev.groups;
        let iq_db = 10.0 * (rds.i_power() / rds.q_power().max(1e-20)).log10();
        eprintln!(
            "{s:4}  {g:6}  {:6}  {:6}  {iq_db:7.1}",
            d.blocks_ok - prev.blocks_ok,
            d.sync_losses - prev.sync_losses
        );
        if chunk.len() == FS as usize {
            per_sec.push(g);
        }
        prev = d;
    }
    let d = rds.diag();
    // Skip the first second (PLL + filter warm-up + sync acquisition).
    let steady = &per_sec[1.min(per_sec.len())..];
    let mean = steady.iter().sum::<u64>() as f64 / steady.len().max(1) as f64;
    let zero = steady.iter().filter(|&&g| g == 0).count();
    let min = steady.iter().min().copied().unwrap_or(0);
    eprintln!(
        "\nsummary: {secs} s, groups={} ({mean:.2}/s steady, ideal 11.42), min/s={min}, \
         zero-group seconds={zero}, blocks_ok={}, sync_losses={}, events={events}",
        d.groups, d.blocks_ok, d.sync_losses
    );
    eprintln!(
        "PI={} PTY={:?} PS={:?} RT={:?}",
        d.pi.map_or_else(|| "-".into(), |p| format!("0x{p:04X}")),
        d.pty,
        d.ps,
        d.rt.trim_end()
    );
}
