//! Spike: can an open device change sample rate safely if the Rx stream
//! is *closed* (dropped) first, rather than merely deactivated? (A
//! deactivated-stream rate change segfaulted SoapySDRServer.) Times
//! close-stream → set rate → new stream → first sample, cycling the
//! receiver ↔ panorama rates. Hand-run; pipeline stopped.
//!
//!   cargo run --release -p ferrite-blocks --features soapysdr \
//!     --example rate_switch_spike -- "driver=remote,remote=bigear"

use std::time::{Duration, Instant};

use num_complex::Complex;
use soapysdr::{Device, Direction::Rx};

fn first_sample(dev: &Device) -> (soapysdr::RxStream<Complex<f32>>, Duration) {
    let t = Instant::now();
    let mut s = dev.rx_stream::<Complex<f32>>(&[0]).expect("stream");
    s.activate(None).expect("activate");
    let mut buf = vec![Complex::new(0.0f32, 0.0); 8192];
    loop {
        if s.read(&mut [&mut buf[..]], 1_000_000).unwrap_or(0) > 0 {
            return (s, t.elapsed());
        }
    }
}

fn main() {
    let args = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "driver=remote,remote=bigear".into());
    let t = Instant::now();
    let dev = Device::new(args.as_str()).expect("open");
    println!("Device::new {:?}", t.elapsed());
    dev.set_frequency(Rx, 0, 98.5e6, ()).unwrap();
    let mut buf = vec![Complex::new(0.0f32, 0.0); 8192];
    for (i, rate) in [2.4e6, 10e6, 2.4e6, 10e6, 2e6, 8e6, 2.4e6, 10e6, 2.4e6]
        .iter()
        .enumerate()
    {
        let t = Instant::now();
        dev.set_sample_rate(Rx, 0, *rate).expect("rate");
        let set = t.elapsed();
        let (mut s, first) = first_sample(&dev);
        // stream for a bit to prove it delivers at the new rate
        let t2 = Instant::now();
        let mut got = 0usize;
        while t2.elapsed() < Duration::from_millis(700) {
            got += s.read(&mut [&mut buf[..]], 1_000_000).unwrap_or(0);
        }
        let mss = got as f64 / t2.elapsed().as_secs_f64() / 1e6;
        let actual = dev.sample_rate(Rx, 0).unwrap_or(0.0);
        let t3 = Instant::now();
        s.deactivate(None).ok();
        drop(s); // closeStream
        println!(
            "#{i} → {:.1} MS/s: set_sample_rate {set:?}, new stream→first sample {first:?}, delivering {mss:.2} MS/s (driver says {:.2}), close {:?}",
            rate / 1e6,
            actual / 1e6,
            t3.elapsed()
        );
    }
    let t = Instant::now();
    drop(dev);
    println!("unmake {:?}", t.elapsed());
}
