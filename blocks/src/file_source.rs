//! IQ file source — replay mode.
//!
//! Reads complex samples from a file and emits `iq_f32`. Used by
//! `ferrited --source file://...` to feed the pipeline from a recorded
//! capture instead of live hardware.
//!
//! Two formats are auto-detected on open:
//!
//! * **`cf32` raw** — interleaved little-endian f32 pairs, 8 bytes per
//!   sample. No header. The sample rate must come from elsewhere (CLI
//!   flag or sidecar) since the format does not carry it.
//! * **WAV-PCM s16 stereo** — the SDR# / HDSDR convention: a standard
//!   RIFF/WAVE file where the left channel is I and the right channel is
//!   Q, 16-bit signed, little-endian. Sample rate lives in the `fmt `
//!   chunk.
//!
//! Other WAV variants (24-bit, f32, mono) and other raw formats (`cs16`,
//! `cu8`) are not supported yet — they error out at open time with a
//! clear message rather than mis-decoding.

use std::{
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, OutBuf, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};
use crate::wav::parse_wav_header;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IqFileFormat {
    /// Interleaved f32 LE (I, Q, I, Q, …). 8 B/sample.
    Cf32,
    /// 16-bit signed stereo PCM (L=I, R=Q). 4 B/sample.
    Cs16Stereo,
}

impl IqFileFormat {
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Cf32 => 8,
            Self::Cs16Stereo => 4,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FileIqSourceParams {
    pub path: PathBuf,
    /// Hint used when the file format does not embed a sample rate
    /// (`cf32` raw). Ignored for WAV files — the header wins.
    pub rate_hz_hint: f64,
    /// Metadata — file formats do not carry this.
    pub center_freq_hz: f64,
    /// Seek to the start of the data and continue when EOF is reached.
    pub loop_playback: bool,
}

/// `Read` + `Seek` + `Send`. Enables swapping in a `Cursor<Vec<u8>>` in tests.
pub trait ReadSeek: Read + Seek + Send {}
impl<T: Read + Seek + Send> ReadSeek for T {}

pub struct FileIqSource {
    format: IqFileFormat,
    rate_hz: f64,
    center_freq_hz: f64,
    loop_playback: bool,
    reader: Box<dyn ReadSeek>,
    /// Offset of the first IQ byte. For `cf32` this is 0; for WAV it is
    /// the start of the `data` chunk.
    data_start: u64,
    /// One past the last IQ byte, or `None` if the data extends to EOF.
    data_end: Option<u64>,
    scratch: Vec<u8>,
    eof: bool,
}

impl FileIqSource {
    /// Open a file, sniff the format, and construct the block.
    pub fn new(params: &FileIqSourceParams) -> Result<Self> {
        let f =
            File::open(&params.path).with_context(|| format!("open {}", params.path.display()))?;
        Self::from_reader(params, Box::new(BufReader::new(f)))
    }

    /// Constructor parameterised over the reader — intended for tests
    /// that feed in-memory bytes via `Cursor`.
    pub fn from_reader(params: &FileIqSourceParams, mut reader: Box<dyn ReadSeek>) -> Result<Self> {
        let mut magic = [0u8; 12];
        let n = reader.read(&mut magic)?;
        reader.seek(SeekFrom::Start(0))?;
        let (format, rate_hz, data_start, data_end) =
            if n >= 12 && &magic[0..4] == b"RIFF" && &magic[8..12] == b"WAVE" {
                let wav = parse_wav_header(&mut reader)?;
                if wav.format_tag != 1 {
                    bail!(
                        "WAV: only PCM (format tag 1) supported, got {}",
                        wav.format_tag
                    );
                }
                if wav.channels != 2 {
                    bail!(
                        "WAV: only stereo I/Q supported, got {} channels",
                        wav.channels
                    );
                }
                if wav.bits_per_sample != 16 {
                    bail!(
                        "WAV: only 16-bit PCM supported, got {}-bit",
                        wav.bits_per_sample
                    );
                }
                reader.seek(SeekFrom::Start(wav.data_start))?;
                (
                    IqFileFormat::Cs16Stereo,
                    f64::from(wav.rate_hz),
                    wav.data_start,
                    Some(wav.data_start + u64::from(wav.data_len)),
                )
            } else {
                (IqFileFormat::Cf32, params.rate_hz_hint, 0, None)
            };
        Ok(Self {
            format,
            rate_hz,
            center_freq_hz: params.center_freq_hz,
            loop_playback: params.loop_playback,
            reader,
            data_start,
            data_end,
            scratch: Vec::new(),
            eof: false,
        })
    }

    #[must_use]
    pub fn format(&self) -> IqFileFormat {
        self.format
    }

    /// Sample rate as decoded from the file (WAV) or the hint (cf32).
    #[must_use]
    pub fn rate_hz(&self) -> f64 {
        self.rate_hz
    }

    #[must_use]
    pub fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    #[must_use]
    pub fn is_eof(&self) -> bool {
        self.eof
    }

    /// Read up to `dst.len()` bytes, but not past the end of the IQ data
    /// region. Returns `Ok(0)` on EOF. Takes `reader` and `data_end`
    /// directly (not `&mut self`) so callers can simultaneously hold a
    /// disjoint mutable borrow on `self.scratch`.
    fn read_bounded(
        reader: &mut dyn ReadSeek,
        data_end: Option<u64>,
        dst: &mut [u8],
    ) -> io::Result<usize> {
        let cap = match data_end {
            Some(end) => {
                let pos = reader.stream_position()?;
                usize::try_from(end.saturating_sub(pos)).unwrap_or(usize::MAX)
            }
            None => dst.len(),
        };
        let n = dst.len().min(cap);
        if n == 0 {
            return Ok(0);
        }
        reader.read(&mut dst[..n])
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for FileIqSource {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "FileIqSource",
            placement: Placement::NativeOnly,
            inputs: &[],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::IqF32,
            }],
            params: &[
                ParamSpec {
                    key: "path",
                    label: "Path",
                    kind: ParamKind::Text { default: "" },
                    // New file = new stream; re-open the source.
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Filesystem path to the IQ file. Auto-detects cf32 (raw interleaved f32) or WAV-PCM-s16 stereo (I=L, Q=R).",
                },
                ParamSpec {
                    key: "loop",
                    label: "Loop",
                    kind: ParamKind::Toggle { default: false },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "True = rewind to start on EOF (cyclic replay). False = stop the stream when the file ends.",
                },
            ],
            ai_notes: "Replays a recorded IQ file as if it were live RF. Use for offline analysis of a known-good capture, regression-testing demods against fixed inputs, or experimenting without an SDR.",
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        Some(self.rate_hz)
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        // Peek the output length without holding a borrow that conflicts
        // with `self.reader` and `self.scratch` below.
        let n = io
            .outputs
            .iter()
            .find(|p| p.name == "out")
            .and_then(|p| match &p.buf {
                OutBuf::IqF32(s) => Some(s.len()),
                _ => None,
            })
            .unwrap_or(0);
        if n == 0 {
            return Ok(Work::new());
        }

        let bps = self.format.bytes_per_sample();
        let need = n * bps;
        if self.scratch.len() < need {
            self.scratch.resize(need, 0);
        }

        let mut filled = 0usize;
        while filled < need {
            let m = Self::read_bounded(
                &mut *self.reader,
                self.data_end,
                &mut self.scratch[filled..need],
            )?;
            if m == 0 {
                if self.loop_playback {
                    self.reader.seek(SeekFrom::Start(self.data_start))?;
                    let retry = Self::read_bounded(
                        &mut *self.reader,
                        self.data_end,
                        &mut self.scratch[filled..need],
                    )?;
                    if retry == 0 {
                        self.eof = true;
                        break;
                    }
                    filled += retry;
                    continue;
                }
                self.eof = true;
                break;
            }
            filled += m;
        }
        let full_samples = filled / bps;

        let out = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_iq_f32_mut)
            .expect("out port length already checked above");

        match self.format {
            IqFileFormat::Cf32 => decode_cf32(&self.scratch, full_samples, out),
            IqFileFormat::Cs16Stereo => decode_cs16(&self.scratch, full_samples, out),
        }
        for item in out.iter_mut().take(n).skip(full_samples) {
            *item = Complex::new(0.0, 0.0);
        }

        let mut w = Work::new();
        w.produced[0] = full_samples;
        Ok(w)
    }
}

impl BlockFactory for FileIqSource {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: FileIqSourceParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(FileIqSource::new(&p)?))
    }
}

fn decode_cf32(src: &[u8], samples: usize, dst: &mut [Complex<f32>]) {
    for (i, slot) in dst.iter_mut().take(samples).enumerate() {
        let off = i * 8;
        let re = f32::from_le_bytes([src[off], src[off + 1], src[off + 2], src[off + 3]]);
        let im = f32::from_le_bytes([src[off + 4], src[off + 5], src[off + 6], src[off + 7]]);
        *slot = Complex::new(re, im);
    }
}

const S16_SCALE: f32 = 1.0 / 32_768.0;

fn decode_cs16(src: &[u8], samples: usize, dst: &mut [Complex<f32>]) {
    for (i, slot) in dst.iter_mut().take(samples).enumerate() {
        let off = i * 4;
        let re = i16::from_le_bytes([src[off], src[off + 1]]);
        let im = i16::from_le_bytes([src[off + 2], src[off + 3]]);
        *slot = Complex::new(f32::from(re) * S16_SCALE, f32::from(im) * S16_SCALE);
    }
}

#[cfg(test)]
mod tests {
    use super::{FileIqSource, FileIqSourceParams, IqFileFormat};
    use crate::block::{Block, BlockIo, OutBuf, OutputPort, PortMeta};
    use num_complex::Complex;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn cf32_bytes(samples: &[(f32, f32)]) -> Vec<u8> {
        let mut v = Vec::with_capacity(samples.len() * 8);
        for &(re, im) in samples {
            v.extend_from_slice(&re.to_le_bytes());
            v.extend_from_slice(&im.to_le_bytes());
        }
        v
    }

    fn cs16_stereo_wav(rate_hz: u32, pairs: &[(i16, i16)]) -> Vec<u8> {
        let data_len = u32::try_from(pairs.len() * 4).expect("fits in u32");
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36u32 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // PCM
        v.extend_from_slice(&2u16.to_le_bytes()); // stereo
        v.extend_from_slice(&rate_hz.to_le_bytes());
        let byte_rate = rate_hz * 4;
        v.extend_from_slice(&byte_rate.to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes()); // block align
        v.extend_from_slice(&16u16.to_le_bytes()); // bits
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        for &(l, r) in pairs {
            v.extend_from_slice(&l.to_le_bytes());
            v.extend_from_slice(&r.to_le_bytes());
        }
        v
    }

    fn make_source(bytes: Vec<u8>, loop_playback: bool) -> FileIqSource {
        FileIqSource::from_reader(
            &FileIqSourceParams {
                path: PathBuf::new(),
                rate_hz_hint: 1.0e6,
                center_freq_hz: 0.0,
                loop_playback,
            },
            Box::new(Cursor::new(bytes)),
        )
        .unwrap()
    }

    fn process_once(source: &mut FileIqSource, n: usize) -> Vec<Complex<f32>> {
        let mut out_buf = vec![Complex::new(0.0_f32, 0.0); n];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::IqF32(&mut out_buf),
        }];
        let mut io = BlockIo {
            inputs: &mut [],
            outputs: &mut outputs,
        };
        source.process(&mut io).unwrap();
        out_buf
    }

    fn close(a: Complex<f32>, re: f32, im: f32) -> bool {
        (a.re - re).abs() < 1e-6 && (a.im - im).abs() < 1e-6
    }

    #[test]
    fn reads_cf32_le() {
        let bytes = cf32_bytes(&[(1.0, 2.0), (3.0, 4.0), (5.0, 6.0), (7.0, 8.0)]);
        let mut src = make_source(bytes, false);
        assert_eq!(src.format(), IqFileFormat::Cf32);
        assert!((src.rate_hz() - 1.0e6).abs() < 1.0);
        let out = process_once(&mut src, 4);
        assert!(close(out[0], 1.0, 2.0));
        assert!(close(out[3], 7.0, 8.0));
        assert!(!src.is_eof());
    }

    #[test]
    fn cf32_loops_when_enabled() {
        let bytes = cf32_bytes(&[(1.0, 0.0), (2.0, 0.0)]);
        let mut src = make_source(bytes, true);
        let out = process_once(&mut src, 4);
        assert!(close(out[0], 1.0, 0.0));
        assert!(close(out[1], 2.0, 0.0));
        assert!(close(out[2], 1.0, 0.0));
        assert!(close(out[3], 2.0, 0.0));
        assert!(!src.is_eof());
    }

    #[test]
    fn cf32_zero_fills_and_sets_eof_without_loop() {
        let bytes = cf32_bytes(&[(0.5, 0.5)]);
        let mut src = make_source(bytes, false);
        let out = process_once(&mut src, 4);
        assert!(close(out[0], 0.5, 0.5));
        assert!(close(out[1], 0.0, 0.0));
        assert!(src.is_eof());
    }

    #[test]
    fn empty_file_with_loop_does_not_spin() {
        let mut src = make_source(Vec::new(), true);
        let out = process_once(&mut src, 2);
        assert!(close(out[0], 0.0, 0.0));
        assert!(src.is_eof());
    }

    #[test]
    fn reads_wav_pcm_s16_stereo() {
        let wav = cs16_stereo_wav(
            39_062,
            &[(16_384, -16_384), (-32_768, 32_767), (0, 0), (8192, 8192)],
        );
        let mut src = make_source(wav, false);
        assert_eq!(src.format(), IqFileFormat::Cs16Stereo);
        assert!((src.rate_hz() - 39_062.0).abs() < 0.5);
        let out = process_once(&mut src, 4);
        assert!((out[0].re - 0.5).abs() < 1e-4);
        assert!((out[0].im + 0.5).abs() < 1e-4);
        assert!((out[1].re + 1.0).abs() < 1e-4);
        assert!(close(out[2], 0.0, 0.0));
    }

    #[test]
    fn wav_loops_on_eof() {
        let wav = cs16_stereo_wav(8000, &[(16_384, 0), (-16_384, 0)]);
        let mut src = make_source(wav, true);
        let out = process_once(&mut src, 4);
        assert!((out[0].re - 0.5).abs() < 1e-4);
        assert!((out[1].re + 0.5).abs() < 1e-4);
        assert!((out[2].re - 0.5).abs() < 1e-4);
        assert!((out[3].re + 0.5).abs() < 1e-4);
        assert!(!src.is_eof());
    }

    #[test]
    fn rejects_unsupported_wav_variants() {
        // 8-bit mono — valid WAV, but not an IQ file.
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&36u32.to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&8000u32.to_le_bytes());
        v.extend_from_slice(&8000u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&8u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&0u32.to_le_bytes());
        let res = FileIqSource::from_reader(
            &FileIqSourceParams {
                path: PathBuf::new(),
                rate_hz_hint: 1.0,
                center_freq_hz: 0.0,
                loop_playback: false,
            },
            Box::new(Cursor::new(v)),
        );
        assert!(res.is_err());
    }
}
