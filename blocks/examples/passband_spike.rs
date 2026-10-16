//! Spike: passband shape (edge roll-off, DC spur) of the radio at a rate,
//! measured on a quiet frequency — sets how much of each sweep step is
//! usable. Hand-run against a real radio with the pipeline stopped:
//!
//!   cargo run --release -p ferrite-blocks --features soapysdr \
//!     --example passband_spike -- "driver=remote,remote=bigear" 10e6 115.5e6

use num_complex::Complex;
use rustfft::FftPlanner;
use soapysdr::{Device, Direction::Rx};

const N: usize = 4096;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let args = a
        .get(1)
        .map_or("driver=remote,remote=bigear", String::as_str);
    let rate: f64 = a.get(2).and_then(|v| v.parse().ok()).unwrap_or(10e6);
    let freq: f64 = a.get(3).and_then(|v| v.parse().ok()).unwrap_or(115.5e6);
    let dev = Device::new(args).expect("open");
    dev.set_sample_rate(Rx, 0, rate).unwrap();
    let _ = dev.set_gain_mode(Rx, 0, false);
    dev.set_frequency(Rx, 0, freq, ()).unwrap();
    println!(
        "rate {:.1} MS/s, IF bandwidth {:?}",
        rate / 1e6,
        dev.bandwidth(Rx, 0)
    );
    let mut s = dev.rx_stream::<Complex<f32>>(&[0]).unwrap();
    s.activate(None).unwrap();
    let fft = FftPlanner::<f32>::new().plan_fft_forward(N);
    let win: Vec<f32> = (0..N)
        .map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / N as f32).cos())
        .collect();
    let mut acc = vec![0.0f64; N];
    let mut buf = vec![Complex::new(0.0f32, 0.0); N];
    let frames = 600;
    for k in 0..frames + 50 {
        let mut at = 0;
        while at < N {
            at += s.read(&mut [&mut buf[at..]], 1_000_000).unwrap_or(0);
        }
        if k < 50 {
            continue;
        }
        for (x, w) in buf.iter_mut().zip(&win) {
            *x *= *w;
        }
        fft.process(&mut buf);
        for (a, x) in acc.iter_mut().zip(&buf) {
            *a += f64::from(x.norm_sqr());
        }
    }
    s.deactivate(None).ok();
    // fftshift → dB, then report 40 slices of the span relative to the
    // median of the middle half.
    let db: Vec<f64> = (0..N)
        .map(|i| 10.0 * (acc[(i + N / 2) % N] / f64::from(frames)).log10())
        .collect();
    let mut mid: Vec<f64> = db[N / 4..3 * N / 4].to_vec();
    mid.sort_by(f64::total_cmp);
    let floor = mid[mid.len() / 2];
    let slices = 40;
    for j in 0..slices {
        let lo = j * N / slices;
        let hi = (j + 1) * N / slices;
        let mut v: Vec<f64> = db[lo..hi].to_vec();
        v.sort_by(f64::total_cmp);
        let off = (lo as f64 / N as f64 - 0.5) * rate / 1e6;
        println!(
            "{off:+6.2} MHz  median {:+6.1} dB  max {:+6.1} dB",
            v[v.len() / 2] - floor,
            v[v.len() - 1] - floor
        );
    }
    // DC neighbourhood at full resolution
    let c = N / 2;
    let bin = rate / N as f64;
    print!("DC ±8 bins ({bin:.0} Hz/bin):");
    for v in &db[c - 8..=c + 8] {
        print!(" {:+.0}", v - floor);
    }
    println!();
}
