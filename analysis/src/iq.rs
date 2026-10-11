//! Streaming `cf32` reading.

use std::io::Read;

use anyhow::{Context, Result};
use num_complex::Complex;
use serde::{Deserialize, Serialize};

/// What a snapshot is: its sample rate and centre (RF) frequency.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct IqMeta {
    pub rate_hz: f64,
    pub center_hz: f64,
}

/// Feed every sample of an interleaved little-endian `cf32` stream to
/// `f`, in blocks. Partial samples across `read` boundaries are carried
/// (a reader may return any byte count).
pub fn for_each_block<R: Read>(mut reader: R, mut f: impl FnMut(&[Complex<f32>])) -> Result<()> {
    let mut buf = vec![0u8; 1 << 16];
    let mut block: Vec<Complex<f32>> = Vec::with_capacity(buf.len() / 8);
    let mut have = 0usize;
    loop {
        let got = reader.read(&mut buf[have..]).context("read IQ")?;
        if got == 0 {
            break;
        }
        let total = have + got;
        let whole = total - total % 8;
        block.clear();
        let (samples, _) = buf[..whole].as_chunks::<8>();
        for c in samples {
            let re = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            let im = f32::from_le_bytes([c[4], c[5], c[6], c[7]]);
            block.push(Complex::new(re, im));
        }
        f(&block);
        buf.copy_within(whole..total, 0);
        have = total - whole;
    }
    Ok(())
}

/// Encode complex samples as `cf32` bytes (tests / fixtures).
#[must_use]
pub fn to_cf32(samples: &[Complex<f32>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 8);
    for z in samples {
        out.extend_from_slice(&z.re.to_le_bytes());
        out.extend_from_slice(&z.im.to_le_bytes());
    }
    out
}
