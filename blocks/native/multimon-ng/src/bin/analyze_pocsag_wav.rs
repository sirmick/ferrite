//! Read a 22050 Hz mono 16-bit WAV and run it through every POCSAG
//! baud-rate decoder. Prints decoded lines + a few activity stats so
//! we can tell `decoder is alive but no signal` apart from `decoder
//! broken`.
//!
//! Usage: `cargo run --release -p ferrite-multimon-ng --bin analyze-pocsag-wav -- /tmp/ferrite-pocsag.wav`

use std::env;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use ferrite_multimon_ng::{pocsag, Decoder, MultimonDemod};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("usage: {} <path/to/22050-mono-s16.wav>", args[0]);
        std::process::exit(2);
    }
    let path = PathBuf::from(&args[1]);

    let samples = read_wav_mono_s16(&path).unwrap_or_else(|e| {
        eprintln!("read {}: {e}", path.display());
        std::process::exit(1);
    });

    println!(
        "loaded {n} samples = {dur:.1} s of audio",
        n = samples.len(),
        dur = samples.len() as f32 / 22_050.0
    );

    let max = samples.iter().fold(0.0_f32, |m, &v| m.max(v.abs()));
    let rms = (samples.iter().map(|&v| v * v).sum::<f32>() / samples.len() as f32).sqrt();
    println!("peak amp = {max:.4}  RMS = {rms:.4}");

    // Detect bursts — slide a 100 ms window over `|samples|`, mark
    // windows whose mean envelope exceeds 3× the noise floor as a
    // burst. Tells us whether the carrier had any activity at all.
    let burst_count = count_bursts(&samples, 22_050);
    println!("burst-like envelope events: {burst_count}");

    // Surface partial-CRC sync hits too — same setting the live
    // PocsagDemod block defaults to. "Sync seen but BCH couldn't
    // repair" is a useful diagnostic between dead carrier and weak.
    pocsag::set_show_partial_decodes(true);

    println!("\n--- decoder output (POCSAG512/1200/2400 + FLEX + FLEX_NEXT) ---");
    let mut total_lines = 0_usize;
    for kind in [
        Decoder::Pocsag512,
        Decoder::Pocsag1200,
        Decoder::Pocsag2400,
        Decoder::Flex,
        Decoder::FlexNext,
    ] {
        let mut d = MultimonDemod::new(kind);
        // multimon expects floats already scaled to ~i16-amplitude
        // range. Our WAV samples are normalised to [-1, 1] so scale
        // back up before pushing.
        let scaled: Vec<f32> = samples.iter().map(|&v| v * (i16::MAX as f32)).collect();
        // Pump in 4096-sample chunks so drain_lines() can interleave
        // with decoding (large single push works too, but smaller
        // chunks give more responsive output ordering).
        for chunk in scaled.chunks(4096) {
            d.push(chunk);
            for line in d.drain_lines() {
                println!("[{}] {line}", kind.name());
                total_lines += 1;
            }
        }
        // Final drain in case any line completes after the last
        // chunk push.
        for line in d.drain_lines() {
            println!("[{}] {line}", kind.name());
            total_lines += 1;
        }
    }
    println!("\n--- end ({total_lines} decoder lines total) ---");
}

fn read_wav_mono_s16(path: &PathBuf) -> std::io::Result<Vec<f32>> {
    let mut r = BufReader::new(File::open(path)?);
    // RIFF header: skip 12 bytes (RIFF + size + WAVE)
    let mut riff = [0_u8; 12];
    r.read_exact(&mut riff)?;
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a RIFF/WAVE file",
        ));
    }

    let mut sample_rate = 0_u32;
    let mut bits = 0_u16;
    let mut channels = 0_u16;
    // Walk chunks until we find `data`.
    loop {
        let mut hdr = [0_u8; 8];
        if r.read_exact(&mut hdr).is_err() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "no data chunk",
            ));
        }
        let id = &hdr[0..4];
        let size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]);
        if id == b"fmt " {
            let mut fmt = vec![0_u8; size as usize];
            r.read_exact(&mut fmt)?;
            // PCM-style fmt: 2 bytes audio_fmt | 2 chans | 4 sr | 4 byterate | 2 blockalign | 2 bits
            channels = u16::from_le_bytes([fmt[2], fmt[3]]);
            sample_rate = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
            bits = u16::from_le_bytes([fmt[14], fmt[15]]);
        } else if id == b"data" {
            if channels != 1 || bits != 16 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("expected mono 16-bit, got {channels}ch {bits}-bit"),
                ));
            }
            if sample_rate != 22_050 {
                eprintln!(
                    "WARNING: sample rate {sample_rate} Hz != 22050 — POCSAG decode \
                     timing will be off"
                );
            }
            // FileAudioSink writes a stub header with size=0 and
            // backfills on Drop. Reading an in-flight capture means
            // we can't trust the field — read to EOF instead.
            let mut buf = Vec::new();
            if size == 0 {
                r.read_to_end(&mut buf)?;
                eprintln!(
                    "(stub header — read {} bytes to EOF, capture probably still running)",
                    buf.len()
                );
            } else {
                buf.resize(size as usize, 0);
                r.read_exact(&mut buf)?;
            }
            // Even-byte truncate (mid-sample EOF on a live file).
            let usable = buf.len() & !1;
            let n = usable / 2;
            return Ok((0..n)
                .map(|i| {
                    let s = i16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]);
                    f32::from(s) / f32::from(i16::MAX)
                })
                .collect());
        } else {
            // Unknown chunk — skip it.
            r.seek(SeekFrom::Current(i64::from(size)))?;
        }
    }
}

/// Count windows where the running envelope is meaningfully above
/// the long-term noise floor. Cheap heuristic — not a real burst
/// detector, just enough to say "the carrier woke up N times".
fn count_bursts(samples: &[f32], rate: u32) -> u32 {
    let win = (rate / 10) as usize; // 100 ms windows
    if samples.len() < win * 2 {
        return 0;
    }
    let mut envs: Vec<f32> = Vec::with_capacity(samples.len() / win);
    for chunk in samples.chunks(win) {
        let avg_abs: f32 = chunk.iter().map(|v| v.abs()).sum::<f32>() / chunk.len() as f32;
        envs.push(avg_abs);
    }
    let mut sorted = envs.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor = sorted[sorted.len() / 4]; // 25th percentile = noise estimate
    let threshold = floor * 3.0;
    let mut count = 0_u32;
    let mut in_burst = false;
    for &e in &envs {
        if e > threshold {
            if !in_burst {
                count += 1;
                in_burst = true;
            }
        } else {
            in_burst = false;
        }
    }
    count
}
