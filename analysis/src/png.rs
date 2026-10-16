//! A spectrogram as a PNG — what an agent (or a file) sees. Palette PNG
//! with stored (uncompressed) deflate blocks: one byte a pixel, no
//! dependencies. Colours match the UI's `heat` ramp
//! (web/src/lib/analysis/types.ts). Time runs down, frequency right.

use crate::spectrogram::Spectrogram;

/// The UI's dark-blue → cyan → yellow → white heat ramp.
fn heat(v: u8) -> [u8; 3] {
    let t = f32::from(v) / 255.0;
    let c = |x: f32| x.round().clamp(0.0, 255.0) as u8;
    if t < 0.33 {
        let k = t / 0.33;
        [0, c(20.0 + 80.0 * k), c(40.0 + 140.0 * k)]
    } else if t < 0.66 {
        let k = (t - 0.33) / 0.33;
        [c(200.0 * k), c(100.0 + 120.0 * k), c(180.0 - 140.0 * k)]
    } else {
        let k = (t - 0.66) / 0.34;
        [
            c(200.0 + 55.0 * k),
            c(220.0 + 35.0 * k),
            c(40.0 + 215.0 * k),
        ]
    }
}

fn crc32(chunks: &[&[u8]]) -> u32 {
    let mut table = [0u32; 256];
    for (n, e) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 == 1 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *e = c;
    }
    let mut c = 0xFFFF_FFFFu32;
    for b in chunks.iter().flat_map(|s| s.iter()) {
        c = table[((c ^ u32::from(*b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

/// zlib stream of stored blocks.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, b) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = b.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(b);
    }
    if raw.is_empty() {
        z.extend_from_slice(&[1, 0, 0, 0xFF, 0xFF]);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &x in raw {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    z
}

/// Encode 8-bit palette indices (`w × h`, row-major) with `palette`.
fn png_indexed(w: usize, h: usize, px: &[u8], palette: &[[u8; 3]]) -> Vec<u8> {
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 3, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    let plte: Vec<u8> = palette.iter().flatten().copied().collect();
    chunk(&mut out, b"PLTE", &plte);
    let mut raw = Vec::with_capacity((w + 1) * h);
    for row in px.chunks(w) {
        raw.push(0); // filter: none
        raw.extend_from_slice(row);
    }
    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// The spectrogram as a PNG, upscaled (nearest) so the short side is at
/// least ~240 px.
#[must_use]
pub fn spectrogram_png(sg: &Spectrogram) -> Vec<u8> {
    let (rows, cols) = (sg.rows.max(1), sg.cols.max(1));
    let sx = (240 / cols).clamp(1, 8);
    let sy = (240 / rows).clamp(1, 8);
    let (w, h) = (cols * sx, rows * sy);
    let mut px = Vec::with_capacity(w * h);
    for y in 0..h {
        let r = y / sy;
        for x in 0..w {
            px.push(sg.cells.get(r * cols + x / sx).copied().unwrap_or(0));
        }
    }
    let palette: Vec<[u8; 3]> = (0..=255u8).map(heat).collect();
    png_indexed(w, h, &px, &palette)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_and_header_are_standard() {
        // CRC of "IEND" with no data is the well-known AE426082.
        assert_eq!(crc32(&[b"IEND"]), 0xAE42_6082);
        let png = png_indexed(2, 2, &[0, 1, 2, 3], &[[0, 0, 0]; 256]);
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        assert_eq!(&png[png.len() - 8..png.len() - 4], b"IEND");
    }

    #[test]
    fn zlib_adler_of_known_input() {
        // adler32("Wikipedia") = 0x11E60398.
        let z = zlib_stored(b"Wikipedia");
        assert_eq!(&z[z.len() - 4..], &0x11E6_0398u32.to_be_bytes());
    }
}
