//! Minimal RIFF/WAVE header parsing, shared by the file-replay sources.
//!
//! [`FileIqSource`](crate::file_source) reads stereo s16 (L=I, R=Q);
//! [`FileAudioSource`](crate::file_audio_source) reads mono s16/f32
//! audio. Both need the same `fmt `/`data` chunk walk, so it lives here
//! once. This parser is intentionally permissive — it returns whatever
//! the header says (any channel count, any bit depth, PCM tag 1 or
//! IEEE-float tag 3) and lets each caller enforce the layout it
//! actually supports with a clear error.

use std::io::{Read, Seek, SeekFrom, Write};

use anyhow::{anyhow, bail, Context, Result};

/// WAVE `wFormatTag` for integer PCM.
pub const WAVE_FORMAT_PCM: u16 = 1;
/// WAVE `wFormatTag` for 32-bit IEEE float PCM.
pub const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// The few `fmt `/`data` fields the replay sources care about.
#[derive(Debug, Clone, Copy)]
pub struct WavInfo {
    /// `wFormatTag`: [`WAVE_FORMAT_PCM`] or [`WAVE_FORMAT_IEEE_FLOAT`].
    pub format_tag: u16,
    pub channels: u16,
    pub rate_hz: u32,
    pub bits_per_sample: u16,
    /// Byte offset of the first sample (start of the `data` chunk body).
    pub data_start: u64,
    /// Length of the `data` chunk body in bytes.
    pub data_len: u32,
}

/// Walk a RIFF/WAVE header, returning [`WavInfo`] positioned so the
/// reader's cursor is left at `data_start`. Unknown chunks are skipped
/// (with even-length padding) until `data` is reached.
pub fn parse_wav_header<R: Read + Seek>(r: &mut R) -> Result<WavInfo> {
    let mut hdr = [0u8; 12];
    r.read_exact(&mut hdr).context("read RIFF header")?;
    if &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
        bail!("not a RIFF/WAVE file");
    }
    let mut fmt_info: Option<(u16, u16, u32, u16)> = None;
    loop {
        let mut chunk_id = [0u8; 4];
        let mut chunk_sz = [0u8; 4];
        if r.read_exact(&mut chunk_id).is_err() {
            bail!("WAV: reached EOF before finding `data` chunk");
        }
        r.read_exact(&mut chunk_sz).context("read chunk size")?;
        let sz = u32::from_le_bytes(chunk_sz);
        match &chunk_id {
            b"fmt " => {
                if sz < 16 {
                    bail!("WAV: short fmt chunk ({sz} bytes)");
                }
                let mut fmt = vec![0u8; sz as usize];
                r.read_exact(&mut fmt).context("read fmt chunk")?;
                let format_tag = u16::from_le_bytes([fmt[0], fmt[1]]);
                let channels = u16::from_le_bytes([fmt[2], fmt[3]]);
                let rate_hz = u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]);
                let bits_per_sample = u16::from_le_bytes([fmt[14], fmt[15]]);
                fmt_info = Some((format_tag, channels, rate_hz, bits_per_sample));
            }
            b"data" => {
                let (format_tag, channels, rate_hz, bits_per_sample) =
                    fmt_info.ok_or_else(|| anyhow!("WAV: `data` chunk before `fmt `"))?;
                let data_start = r.stream_position().context("tell data_start")?;
                return Ok(WavInfo {
                    format_tag,
                    channels,
                    rate_hz,
                    bits_per_sample,
                    data_start,
                    data_len: sz,
                });
            }
            _ => {
                r.seek(SeekFrom::Current(i64::from(sz)))
                    .context("skip unknown chunk")?;
                // Chunks are padded to an even length.
                if sz % 2 == 1 {
                    r.seek(SeekFrom::Current(1)).ok();
                }
            }
        }
    }
}
/// Write a 44-byte 16-bit-PCM RIFF/WAVE header with placeholder RIFF
/// and `data` chunk sizes (both zero — patched by [`patch_wav_sizes`]
/// at finalise once the total sample count is known). Returns the byte
/// offset of the `data` size `u32` so the caller can seek back to it.
///
/// `channels` is 1 for mono audio ([`FileAudioSink`](crate::file_audio_sink))
/// or 2 for stereo IQ ([`FileIqSink`](crate::file_sink)); both write
/// signed 16-bit samples, so `block_align = channels * 2`.
pub fn write_pcm_s16_stub_header<W: Write + Seek>(
    w: &mut W,
    rate_hz: f64,
    channels: u16,
) -> Result<u64> {
    if !(rate_hz > 0.0) || rate_hz > f64::from(u32::MAX) {
        bail!("WAV: rate_hz out of range: {rate_hz}");
    }
    // Bounded by the check above: 0 < rate_hz <= u32::MAX, so the
    // round-and-cast is exact and non-negative.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let rate = rate_hz.round() as u32;
    let block_align = channels.saturating_mul(2); // s16 = 2 B/sample
    let byte_rate = rate.saturating_mul(u32::from(block_align));

    w.write_all(b"RIFF").context("RIFF")?;
    w.write_all(&0u32.to_le_bytes()).context("RIFF size stub")?; // patched on finalise
    w.write_all(b"WAVE").context("WAVE")?;

    w.write_all(b"fmt ").context("fmt id")?;
    w.write_all(&16u32.to_le_bytes()).context("fmt size")?;
    w.write_all(&WAVE_FORMAT_PCM.to_le_bytes())
        .context("PCM tag")?;
    w.write_all(&channels.to_le_bytes()).context("channels")?;
    w.write_all(&rate.to_le_bytes()).context("sample rate")?;
    w.write_all(&byte_rate.to_le_bytes()).context("byte rate")?;
    w.write_all(&block_align.to_le_bytes())
        .context("block align")?;
    w.write_all(&16u16.to_le_bytes()).context("bits")?;

    w.write_all(b"data").context("data id")?;
    let data_size_pos = w.stream_position().context("tell data size pos")?;
    w.write_all(&0u32.to_le_bytes()).context("data size stub")?; // patched on finalise
    Ok(data_size_pos)
}

/// Back-patch the RIFF and `data` chunk sizes once writing is done.
/// `data_size_pos` is what [`write_pcm_s16_stub_header`] returned;
/// `data_bytes` is the total sample-payload size in bytes.
pub fn patch_wav_sizes<W: Write + Seek>(
    mut w: W,
    data_size_pos: u64,
    data_bytes: u32,
) -> Result<()> {
    w.seek(SeekFrom::Start(data_size_pos))
        .context("seek data size")?;
    w.write_all(&data_bytes.to_le_bytes())
        .context("patch data size")?;
    // RIFF chunk size = 36 + data_bytes (header is 44 B; the RIFF size
    // field excludes the leading `RIFF` + size = 8 B).
    let riff_size = data_bytes.saturating_add(36);
    w.seek(SeekFrom::Start(4)).context("seek riff size")?;
    w.write_all(&riff_size.to_le_bytes())
        .context("patch riff size")?;
    w.flush().context("flush after patch")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_wav_header, WAVE_FORMAT_IEEE_FLOAT, WAVE_FORMAT_PCM};
    use std::io::Cursor;

    fn wav(format_tag: u16, channels: u16, rate: u32, bits: u16, data: &[u8]) -> Vec<u8> {
        let data_len = u32::try_from(data.len()).unwrap();
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36u32 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&format_tag.to_le_bytes());
        v.extend_from_slice(&channels.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        let block_align = u32::from(channels) * u32::from(bits) / 8;
        v.extend_from_slice(&(rate * block_align).to_le_bytes());
        v.extend_from_slice(&u16::try_from(block_align).unwrap().to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    #[test]
    fn parses_mono_s16() {
        let bytes = wav(WAVE_FORMAT_PCM, 1, 48_000, 16, &[1, 2, 3, 4]);
        let info = parse_wav_header(&mut Cursor::new(bytes)).unwrap();
        assert_eq!(info.format_tag, WAVE_FORMAT_PCM);
        assert_eq!(info.channels, 1);
        assert_eq!(info.rate_hz, 48_000);
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.data_len, 4);
    }

    #[test]
    fn parses_mono_f32_and_skips_unknown_chunk() {
        // A LIST chunk before data must be skipped (with odd padding).
        let data = 1.0f32.to_le_bytes();
        let data_len = u32::try_from(data.len()).unwrap();
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(100u32).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&WAVE_FORMAT_IEEE_FLOAT.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&48_000u32.to_le_bytes());
        v.extend_from_slice(&192_000u32.to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&32u16.to_le_bytes());
        v.extend_from_slice(b"LIST");
        v.extend_from_slice(&3u32.to_le_bytes()); // odd → 1 pad byte
        v.extend_from_slice(b"abc");
        v.push(0);
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        v.extend_from_slice(&data);
        let info = parse_wav_header(&mut Cursor::new(v)).unwrap();
        assert_eq!(info.format_tag, WAVE_FORMAT_IEEE_FLOAT);
        assert_eq!(info.bits_per_sample, 32);
        assert_eq!(info.data_len, 4);
    }

    #[test]
    fn rejects_non_riff() {
        let res = parse_wav_header(&mut Cursor::new(b"NOPE........".to_vec()));
        assert!(res.is_err());
    }
}
