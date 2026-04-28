//! Read a 2 MS/s IQ file and run it through `Dump1090`. Prints any
//! decoded ADS-B / Mode S frames (the same line stream the live
//! `decoder::adsb` log target carries) plus a one-line stats summary.
//!
//! Useful for offline triage when the live preset isn't surfacing
//! aircraft — capture a few seconds of 1090 MHz IQ via
//! `flowgraphs/capture-adsb.json` (or any other 2 MS/s IQ sink) and
//! point this at it. If the live decoder count and the offline count
//! disagree on the same fixture, the bug is in the live runtime path
//! rather than in the audio chain — same A/B split as
//! `analyze-packet-wav` for APRS.
//!
//! Two formats supported:
//!   - `cs8` — interleaved signed-int8 IQ (RTL-SDR's native format
//!     after centring, `i*128 - 128`)
//!   - `cu8` — interleaved unsigned-int8 IQ (RTL-SDR raw, `127` = zero)
//!   - `cf32` — interleaved 32-bit float IQ (Soapy capture format)
//!
//! Format is autodetected from extension; override with `--format`.
//!
//! Usage:
//!   `cargo run --release -p ferrite-dump1090 --bin analyze-adsb-iq -- /tmp/adsb.cu8`
//!   `cargo run --release -p ferrite-dump1090 --bin analyze-adsb-iq -- --format cf32 /tmp/adsb.iq`

use std::env;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use ferrite_dump1090::Dump1090;
use num_complex::Complex;

#[derive(Clone, Copy)]
enum Format {
    Cu8,
    Cs8,
    Cf32,
}

fn parse_format(s: &str) -> Option<Format> {
    match s {
        "cu8" | "u8" => Some(Format::Cu8),
        "cs8" | "s8" => Some(Format::Cs8),
        "cf32" | "f32" => Some(Format::Cf32),
        _ => None,
    }
}

fn detect_format(path: &Path) -> Format {
    match path.extension().and_then(|e| e.to_str()) {
        Some("cs8") | Some("s8") => Format::Cs8,
        Some("cf32") | Some("f32") | Some("iq") => Format::Cf32,
        // RTL-SDR's default is unsigned int8; assume that for unknown
        // extensions and `cu8` so there's a working default for the
        // most common case.
        _ => Format::Cu8,
    }
}

fn read_iq(path: &PathBuf, fmt: Format) -> std::io::Result<Vec<Complex<f32>>> {
    let mut r = BufReader::new(File::open(path)?);
    let mut bytes = Vec::new();
    r.read_to_end(&mut bytes)?;
    Ok(match fmt {
        Format::Cu8 => bytes
            .chunks_exact(2)
            .map(|c| {
                let i = (f32::from(c[0]) - 128.0) / 128.0;
                let q = (f32::from(c[1]) - 128.0) / 128.0;
                Complex::new(i, q)
            })
            .collect(),
        Format::Cs8 => bytes
            .chunks_exact(2)
            .map(|c| {
                let i = f32::from(c[0] as i8) / 128.0;
                let q = f32::from(c[1] as i8) / 128.0;
                Complex::new(i, q)
            })
            .collect(),
        Format::Cf32 => bytes
            .chunks_exact(8)
            .map(|c| {
                let i = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                let q = f32::from_le_bytes([c[4], c[5], c[6], c[7]]);
                Complex::new(i, q)
            })
            .collect(),
    })
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut format_override: Option<Format> = None;
    let mut path: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--format" if i + 1 < args.len() => {
                format_override = parse_format(&args[i + 1]);
                if format_override.is_none() {
                    eprintln!("unknown --format {:?} (want cu8|cs8|cf32)", args[i + 1]);
                    std::process::exit(2);
                }
                i += 2;
            }
            other => {
                path = Some(PathBuf::from(other));
                i += 1;
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: {} [--format cu8|cs8|cf32] <path/to/iq>", args[0]);
        std::process::exit(2);
    };
    let fmt = format_override.unwrap_or_else(|| detect_format(&path));

    let samples = read_iq(&path, fmt).unwrap_or_else(|e| {
        eprintln!("read {}: {e}", path.display());
        std::process::exit(1);
    });
    println!(
        "loaded {n} samples = {dur:.1} s of 2 MS/s IQ ({fmt})",
        n = samples.len(),
        dur = samples.len() as f32 / 2_000_000.0,
        fmt = match fmt {
            Format::Cu8 => "cu8",
            Format::Cs8 => "cs8",
            Format::Cf32 => "cf32",
        }
    );

    let mut d = Dump1090::new();
    // Chunk size matches the live ferrite tick — at 400 µs ticks and
    // 2 MS/s, ~800 samples per call. The wrapper internally batches up
    // to dump1090's MODES_DATA_LEN so caller chunk size doesn't change
    // results, but we run small chunks here to exercise the same path.
    let chunk_size: usize = std::env::var("CHUNK")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(800);

    println!("\n--- decoder output (decoder::adsb stream) ---");
    let mut total_lines = 0_usize;
    for chunk in samples.chunks(chunk_size) {
        d.push_iq(chunk);
        for line in d.drain_lines() {
            println!("{line}");
            total_lines += 1;
        }
    }
    // Final drain for any frame that landed in the last batch.
    for line in d.drain_lines() {
        println!("{line}");
        total_lines += 1;
    }
    println!("\n--- end ({total_lines} decoder lines total) ---");
}
