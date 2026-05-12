//! End-to-end FFT sanity harness.
//!
//! Pumps a captured IQ WAV through `FileIqSource → FftBlock → LogMagU8`
//! and writes the resulting byte rows to a PGM image (+ an averaged
//! single-row trace). Useful for eyeballing the spectrum the browser
//! *would* paint, without the browser and without the network.
//!
//! Run:
//! ```text
//! cargo run --release --example fft_through_pipeline -- \
//!     /tmp/ferrite-fm-100.100mhz_iq-s16.wav 16384
//! ```

use std::{
    env,
    fs::File,
    io::{BufWriter, Write},
    path::PathBuf,
    process::ExitCode,
};

use anyhow::{anyhow, Context, Result};
use ferrite_blocks::{
    block::{BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta},
    collapse_row_to_columns, Block, FftBlock, FftBlockParams, FftWindow, FileIqSource,
    FileIqSourceParams, LogMagU8, LogMagU8Params,
};
use num_complex::Complex;

const DEFAULT_FFT_SIZE: usize = 16_384;
const OUTPUT_PIXEL_WIDTH: usize = 1200;
const ALPHA: f32 = 0.3;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let path = PathBuf::from(
        args.next()
            .ok_or_else(|| anyhow!("usage: fft_through_pipeline <wav> [fft_size]"))?,
    );
    let fft_size: usize = match args.next() {
        Some(s) => s.parse().context("fft_size")?,
        None => DEFAULT_FFT_SIZE,
    };

    let mut source = FileIqSource::new(&FileIqSourceParams {
        path: path.clone(),
        rate_hz_hint: 0.0,
        center_freq_hz: 0.0,
        loop_playback: false,
    })?;

    let mut fft = FftBlock::new(FftBlockParams {
        size: fft_size,
        window: FftWindow::Hann,
        ..Default::default()
    })?;

    let mut logmag = LogMagU8::new(LogMagU8Params {
        size: fft_size,
        alpha: ALPHA,
        ..LogMagU8Params::default()
    });

    let mut rows: Vec<Vec<u8>> = Vec::new();
    let mut iq_buf = vec![Complex::new(0.0, 0.0); fft_size]; // source → fft
    let mut bin_buf = vec![Complex::new(0.0, 0.0); fft_size]; // fft → logmag
    let mut u8_buf = vec![0u8; fft_size]; // logmag → us

    loop {
        // --- source → iq_buf ---
        iq_buf.iter_mut().for_each(|s| *s = Complex::new(0.0, 0.0));
        let mut outs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut iq_buf),
        }];
        let mut inputs: [InputPort; 0] = [];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outs,
        };
        let w = source.process(&mut io)?;
        let produced = w.produced[0];
        if produced == 0 {
            break;
        }

        // --- iq_buf → fft → bin_buf ---
        let mut ins = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&iq_buf[..produced]),
        }];
        let mut outs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut bin_buf),
        }];
        let mut io = BlockIo {
            inputs: &mut ins,
            outputs: &mut outs,
        };
        let w = fft.process(&mut io)?;
        if w.produced[0] == 0 {
            continue;
        }

        // --- bin_buf → logmag → u8_buf ---
        let mut ins = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(&bin_buf[..fft_size]),
        }];
        let mut outs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::FftU8(&mut u8_buf),
        }];
        let mut io = BlockIo {
            inputs: &mut ins,
            outputs: &mut outs,
        };
        logmag.process(&mut io)?;

        rows.push(u8_buf.clone());
    }

    if rows.is_empty() {
        return Err(anyhow!("produced zero FFT rows"));
    }
    println!("rows={}  fft_size={}", rows.len(), fft_size);

    // Average row (post-warm-up) — a stable spectrum we can eyeball.
    let warm = rows.len() / 10;
    let mut avg = vec![0u64; fft_size];
    for r in &rows[warm..] {
        for (i, &v) in r.iter().enumerate() {
            avg[i] += u64::from(v);
        }
    }
    #[allow(clippy::cast_possible_truncation)]
    let avg_u8: Vec<u8> = avg
        .iter()
        .map(|&x| ((x / (rows.len() - warm) as u64).min(255)) as u8)
        .collect();

    // Collapse to the plot width using the Rust render helper — exactly
    // what the browser would do with this same row.
    let mut cols = vec![0u8; OUTPUT_PIXEL_WIDTH];
    collapse_row_to_columns(&avg_u8, &mut cols);

    // Percentile summary.
    let (p10, p50, p99) = pct(&avg_u8);
    println!("avg row  p10={p10}  p50={p50}  p99={p99}");

    // Top-10 peaks in the collapsed-to-plot-width view.
    let mut idx: Vec<usize> = (0..cols.len()).collect();
    idx.sort_by(|&a, &b| cols[b].cmp(&cols[a]));
    println!("top 10 peaks (after column collapse):");
    for &i in idx.iter().take(10) {
        let frac = i as f64 / cols.len() as f64;
        println!("  col {i:4} ({frac:5.3}) → {}", cols[i]);
    }

    // Waterfall PGM — rows stacked top (oldest) to bottom.
    let wf_path = PathBuf::from("/tmp/ferrite-fft-waterfall.pgm");
    write_pgm_rows(&wf_path, &rows, fft_size)?;
    println!("wrote {}", wf_path.display());

    // Collapsed average — one row wide as PGM.
    let avg_path = PathBuf::from("/tmp/ferrite-fft-average.pgm");
    write_pgm_rows(&avg_path, std::slice::from_ref(&cols), cols.len())?;
    println!("wrote {}", avg_path.display());

    // ASCII trace so the terminal tells us at a glance.
    println!("\nASCII spectrum (avg, collapsed to 120 cols):");
    ascii_trace(&avg_u8, 120, 24);
    Ok(())
}

fn pct(row: &[u8]) -> (u8, u8, u8) {
    let mut v = row.to_vec();
    v.sort_unstable();
    (v[v.len() / 10], v[v.len() / 2], v[v.len() * 99 / 100])
}

fn write_pgm_rows(path: &PathBuf, rows: &[impl AsRef<[u8]>], width: usize) -> Result<()> {
    let mut f = BufWriter::new(File::create(path)?);
    writeln!(f, "P5")?;
    writeln!(f, "{} {}", width, rows.len())?;
    writeln!(f, "255")?;
    for r in rows {
        f.write_all(r.as_ref())?;
    }
    Ok(())
}

fn ascii_trace(row: &[u8], w: usize, h: usize) {
    let mut cols = vec![0u8; w];
    collapse_row_to_columns(row, &mut cols);
    for y in 0..h {
        let thresh = 255 - (y * 255 / (h - 1)) as u8;
        let line: String = cols
            .iter()
            .map(|&v| if v >= thresh { '#' } else { ' ' })
            .collect();
        println!("{}", line);
    }
}
