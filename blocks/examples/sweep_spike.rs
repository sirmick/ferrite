//! Spike: the numbers a stepped sweep (`SweepSource`) is designed around.
//! Not a test — run by hand against a real radio with the pipeline stopped:
//!
//!   cargo run --release -p ferrite-blocks --features soapysdr \
//!     --example sweep_spike -- "driver=remote,remote=bigear" 105.5e6 106.5e6
//!
//! Measures, per sample rate:
//!   A. sustained delivery (MS/s, overflows) — the link budget.
//!   B. retune: how long `set_frequency` blocks, and how many *stale*
//!      samples (from the old LO) arrive after it returns, detected as the
//!      power at a known carrier's offset dropping / rising.
//!   C. switch cost: stream deactivate→activate on an open handle vs a
//!      full device re-open, each to the first delivered sample.
//!
//! Args: <device args> <centre A> <carrier freq> [antenna]. Centre B is
//! centre A + 10 MHz, so the carrier is in span at A and out of it at B.

use std::f64::consts::TAU;
use std::time::{Duration, Instant};

use num_complex::Complex;
use soapysdr::{Device, Direction::Rx, ErrorCode, RxStream};

const BLOCK: usize = 8192;

fn open(
    args: &str,
    rate: f64,
    freq: f64,
    antenna: Option<&str>,
) -> (Device, RxStream<Complex<f32>>) {
    let dev = Device::new(args).expect("open device");
    dev.set_sample_rate(Rx, 0, rate).expect("rate");
    if let Some(a) = antenna {
        dev.set_antenna(Rx, 0, a.as_bytes()).expect("antenna");
    }
    let _ = dev.set_gain_mode(Rx, 0, false);
    dev.set_frequency(Rx, 0, freq, ()).expect("freq");
    let stream = dev.rx_stream::<Complex<f32>>(&[0]).expect("stream");
    (dev, stream)
}

/// One driver read (SoapyRemote hands back MTU-sized chunks); returns
/// (n, time_ns, overflowed).
fn read_block(s: &mut RxStream<Complex<f32>>, buf: &mut [Complex<f32>]) -> (usize, i64, bool) {
    match s.read(&mut [&mut buf[..]], 1_000_000) {
        Ok(n) => (n, s.time_ns(), false),
        Err(e) if e.code == ErrorCode::Overflow => (0, 0, true),
        Err(e) if e.code == ErrorCode::Timeout => (0, 0, false),
        Err(e) => panic!("read: {e}"),
    }
}

/// Fill all of `buf` (many driver reads).
fn read_full(s: &mut RxStream<Complex<f32>>, buf: &mut [Complex<f32>]) {
    let mut at = 0;
    while at < buf.len() {
        let (n, _, _) = read_block(s, &mut buf[at..]);
        at += n;
    }
}

/// Single-bin DFT power (dB) at `off_hz` from centre.
fn bin_db(x: &[Complex<f32>], off_hz: f64, rate: f64) -> f64 {
    let w = -TAU * off_hz / rate;
    let mut acc = Complex::new(0.0f64, 0.0);
    for (n, s) in x.iter().enumerate() {
        let ph = w * n as f64;
        acc += Complex::new(f64::from(s.re), f64::from(s.im)) * Complex::new(ph.cos(), ph.sin());
    }
    10.0 * (acc.norm_sqr() / (x.len() as f64).powi(2))
        .max(1e-20)
        .log10()
}

fn drain(s: &mut RxStream<Complex<f32>>, buf: &mut [Complex<f32>], dur: Duration) {
    let t = Instant::now();
    while t.elapsed() < dur {
        read_block(s, buf);
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let args = a
        .get(1)
        .map_or("driver=remote,remote=bigear", String::as_str);
    let f_a: f64 = a.get(2).and_then(|v| v.parse().ok()).unwrap_or(105.5e6);
    let carrier: f64 = a.get(3).and_then(|v| v.parse().ok()).unwrap_or(106.5e6);
    let antenna = a.get(4).map(String::as_str);
    let f_b = f_a + 10e6;
    let off = carrier - f_a;
    let mut buf = vec![Complex::new(0.0f32, 0.0); BLOCK];

    for rate in [2e6, 6e6, 8e6, 10e6] {
        println!("\n##### rate {:.0} MS/s", rate / 1e6);
        let t_open = Instant::now();
        let (dev, mut s) = open(args, rate, f_a, antenna);
        s.activate(None).expect("activate");
        let mut first = None;
        while first.is_none() {
            let (n, _, _) = read_block(&mut s, &mut buf);
            if n > 0 {
                first = Some(t_open.elapsed());
            }
        }
        println!("open→first sample: {:?}", first.unwrap());

        // A. sustained delivery
        drain(&mut s, &mut buf, Duration::from_millis(500));
        let (mut got, mut ovf, mut ts_ok, mut last_ts) = (0usize, 0, 0, 0i64);
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(3) {
            let (n, ts, o) = read_block(&mut s, &mut buf);
            got += n;
            ovf += usize::from(o);
            if n > 0 && ts > last_ts {
                ts_ok += 1;
            }
            if n > 0 {
                last_ts = ts;
            }
        }
        let secs = t.elapsed().as_secs_f64();
        println!(
            "A delivery: {:.2} MS/s of {:.0} ({:.0}%), overflows {ovf}, time_ns monotonic on {ts_ok} reads (last {last_ts})",
            got as f64 / secs / 1e6,
            rate / 1e6,
            100.0 * got as f64 / secs / rate
        );

        // B. retune latency + stale samples, several hops each way. Power
        // is measured on whole BLOCKs; a transition needs 3 consecutive
        // blocks past the threshold (single blocks of noise cross it).
        read_full(&mut s, &mut buf);
        let ref_db = bin_db(&buf, off, rate);
        dev.set_frequency(Rx, 0, f_b, ()).unwrap();
        drain(&mut s, &mut buf, Duration::from_millis(400));
        read_full(&mut s, &mut buf);
        let quiet_db = bin_db(&buf, off, rate);
        let thresh = (ref_db + quiet_db) / 2.0;
        println!(
            "B carrier bin: in-span {ref_db:.1} dB, out {quiet_db:.1} dB, threshold {thresh:.1}"
        );
        let blk_ms = BLOCK as f64 / rate * 1e3;
        let mut lat = Vec::new();
        for hop in 0..10 {
            let (to, want_hi) = if hop % 2 == 0 {
                (f_a, true)
            } else {
                (f_b, false)
            };
            drain(&mut s, &mut buf, Duration::from_millis(300));
            let t0 = Instant::now();
            dev.set_frequency(Rx, 0, to, ()).unwrap();
            let call = t0.elapsed();
            let mut trace: Vec<f64> = Vec::new();
            let first_new = loop {
                read_full(&mut s, &mut buf);
                trace.push(bin_db(&buf, off, rate));
                let k = trace.len();
                if k >= 3 && trace[k - 3..].iter().all(|p| (*p > thresh) == want_hi) {
                    break k - 3;
                }
                if k as f64 * blk_ms > 3000.0 {
                    break usize::MAX;
                }
            };
            // settle: keep reading 12 more blocks, find when within 3 dB
            // of their tail median.
            for _ in 0..12 {
                read_full(&mut s, &mut buf);
                trace.push(bin_db(&buf, off, rate));
            }
            let mut tail: Vec<f64> = trace[trace.len() - 8..].to_vec();
            tail.sort_by(f64::total_cmp);
            let med = tail[4];
            let settled = (first_new..trace.len())
                .find(|&i| trace[i..].iter().take(4).all(|p| (p - med).abs() < 3.0))
                .unwrap_or(usize::MAX);
            let stale_ms = first_new as f64 * blk_ms;
            let settle_ms = settled.saturating_sub(first_new) as f64 * blk_ms;
            lat.push((call.as_secs_f64() * 1e3, stale_ms, settle_ms));
            println!(
                "B hop {hop} → {:.1}: set_frequency {:.1} ms, stale {stale_ms:.1} ms, settle +{settle_ms:.1} ms, trace {:?}",
                to / 1e6,
                call.as_secs_f64() * 1e3,
                trace.iter().map(|p| format!("{p:.0}")).collect::<Vec<_>>()
            );
        }
        let pick = |f: &dyn Fn(&(f64, f64, f64)) -> f64| {
            let mut v: Vec<f64> = lat.iter().map(f).collect();
            v.sort_by(f64::total_cmp);
            (v[v.len() / 2], v[v.len() - 1])
        };
        let (c50, cmax) = pick(&|l| l.0);
        let (s50, smax) = pick(&|l| l.1);
        let (t50, tmax) = pick(&|l| l.2);
        println!(
            "B median/max: set_frequency {c50:.1}/{cmax:.1} ms, stale {s50:.1}/{smax:.1} ms, settle {t50:.1}/{tmax:.1} ms (block {blk_ms:.2} ms)"
        );

        // C. switch cost on an open handle.
        let t0 = Instant::now();
        s.deactivate(None).unwrap();
        s.activate(None).unwrap();
        loop {
            let (n, _, _) = read_block(&mut s, &mut buf);
            if n > 0 {
                break;
            }
        }
        println!("C deactivate→activate→first sample: {:?}", t0.elapsed());
        // (A deactivated rate change 10 → 2 MS/s segfaulted SoapySDRServer
        // on bigear — never change rate mid-sweep; not re-tested here.)
        s.deactivate(None).unwrap();
        drop(s);
        drop(dev);
        println!("(full re-open cost = 'open→first sample' above)");
        std::thread::sleep(Duration::from_millis(500));
    }
}
