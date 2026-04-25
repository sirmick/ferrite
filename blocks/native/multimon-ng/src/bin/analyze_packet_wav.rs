//! Read a 22050 Hz mono 16-bit WAV and run it through every packet-
//! family decoder (AFSK1200, AFSK2400 ×3 timing variants, FSK9600).
//! Use to A/B a captured `/tmp/ferrite-aprs.wav` from the live runner
//! against the same multimon code path the `PacketDemod` block runs.
//!
//! Usage: `cargo run --release -p ferrite-multimon-ng --bin analyze-packet-wav -- /tmp/ferrite-aprs.wav`

use std::env;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use ferrite_multimon_ng::{Decoder, MultimonDemod};

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
    let clipped = samples.iter().filter(|v| v.abs() >= 0.999).count();
    println!(
        "peak amp = {max:.4}  RMS = {rms:.4}  clipped samples = {clipped} ({pct:.1}%)",
        pct = 100.0 * clipped as f32 / samples.len() as f32
    );

    println!("\n--- decoder output (AFSK1200, AFSK2400 ×3, FSK9600) ---");
    let mut total_lines = 0_usize;
    for kind in [
        Decoder::Afsk1200,
        Decoder::Afsk2400,
        Decoder::Afsk2400_2,
        Decoder::Afsk2400_3,
        Decoder::Fsk9600,
    ] {
        let mut d = MultimonDemod::new(kind);
        // Chunk size matches the live PacketDemod feed (~10 samples per
        // tick at 22050 Hz with the runtime's default 1 ms tick) so the
        // offline path stresses multimon the same way.
        let chunk_size: usize = std::env::var("CHUNK")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4096);
        for chunk in samples.chunks(chunk_size) {
            d.push(chunk);
            for line in d.drain_lines() {
                println!("[{}] {line}", kind.name());
                total_lines += 1;
            }
        }
        for line in d.drain_lines() {
            println!("[{}] {line}", kind.name());
            total_lines += 1;
        }
    }
    println!("\n--- end ({total_lines} decoder lines total) ---");
}

fn read_wav_mono_s16(path: &PathBuf) -> std::io::Result<Vec<f32>> {
    let mut r = BufReader::new(File::open(path)?);
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
                    "WARNING: sample rate {sample_rate} Hz != 22050 — packet decode \
                     timing will be off"
                );
            }
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
            let usable = buf.len() & !1;
            let n = usable / 2;
            return Ok((0..n)
                .map(|i| {
                    let s = i16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]);
                    f32::from(s) / f32::from(i16::MAX)
                })
                .collect());
        } else {
            r.seek(SeekFrom::Current(i64::from(size)))?;
        }
    }
}
