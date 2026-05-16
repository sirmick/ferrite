//! Mono-audio file source — replay a recorded audio clip as a
//! `real_f32` stream.
//!
//! The companion of [`FileIqSource`](crate::file_source): that one
//! replays *IQ* captures; this one replays *demodulated audio* (the
//! `samples/audio/*.wav` clips) so a test can push a known waveform
//! through a modulator + the real demod chain and score the recovered
//! audio against this original. It is the deterministic, CI-safe driver
//! behind the analog-mode end-to-end tests — there is no listen preset
//! that uses it.
//!
//! Two layouts are accepted:
//!
//! * **WAV mono PCM** — `s16` or 32-bit IEEE-`f32`, single channel.
//!   Sample rate comes from the `fmt ` chunk.
//! * **`f32` raw** — headerless interleaved little-endian f32 mono.
//!   The rate must come from `rate_hz_hint`.
//!
//! Stereo/multi-channel WAVs are rejected with a clear message rather
//! than silently demoting to the first channel — the audio fixtures are
//! mono by construction and an unexpected channel count is a bug worth
//! surfacing.

use std::{
    fs::File,
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, OutBuf, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work,
};
use crate::file_source::ReadSeek;
use crate::wav::{parse_wav_header, WAVE_FORMAT_IEEE_FLOAT, WAVE_FORMAT_PCM};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFileFormat {
    /// Headerless interleaved f32 LE, mono. 4 B/sample.
    RawF32,
    /// WAV mono 16-bit signed PCM. 2 B/sample.
    WavS16,
    /// WAV mono 32-bit IEEE float. 4 B/sample.
    WavF32,
}

impl AudioFileFormat {
    #[must_use]
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::WavS16 => 2,
            Self::RawF32 | Self::WavF32 => 4,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FileAudioSourceParams {
    pub path: PathBuf,
    /// Hint used only for the headerless `f32` raw layout. Ignored for
    /// WAV — the `fmt ` chunk wins.
    pub rate_hz_hint: f64,
    /// Linear gain applied to every sample as it is emitted. The mono
    /// audio fixtures sit well below full scale; 1.0 replays them as-is.
    pub gain: f32,
    /// Rewind to the start of `data` on EOF (cyclic replay).
    pub loop_playback: bool,
}

pub struct FileAudioSource {
    format: AudioFileFormat,
    rate_hz: f64,
    gain: f32,
    loop_playback: bool,
    reader: Box<dyn ReadSeek>,
    /// Offset of the first audio byte. 0 for raw, the `data` chunk start
    /// for WAV.
    data_start: u64,
    /// One past the last audio byte, or `None` if data runs to EOF.
    data_end: Option<u64>,
    scratch: Vec<u8>,
    eof: bool,
}

impl FileAudioSource {
    pub fn new(params: &FileAudioSourceParams) -> Result<Self> {
        let f =
            File::open(&params.path).with_context(|| format!("open {}", params.path.display()))?;
        Self::from_reader(params, Box::new(BufReader::new(f)))
    }

    /// Reader-parameterised constructor — tests feed an in-memory
    /// `Cursor`.
    pub fn from_reader(
        params: &FileAudioSourceParams,
        mut reader: Box<dyn ReadSeek>,
    ) -> Result<Self> {
        let mut magic = [0u8; 12];
        let n = reader.read(&mut magic)?;
        reader.seek(SeekFrom::Start(0))?;
        let gain = if params.gain == 0.0 { 1.0 } else { params.gain };
        if !gain.is_finite() {
            bail!("file_audio_source gain must be finite");
        }

        let (format, rate_hz, data_start, data_end) =
            if n >= 12 && &magic[0..4] == b"RIFF" && &magic[8..12] == b"WAVE" {
                let w = parse_wav_header(&mut reader)?;
                if w.channels != 1 {
                    bail!(
                        "WAV: only mono audio supported, got {} channels",
                        w.channels
                    );
                }
                let format = match (w.format_tag, w.bits_per_sample) {
                    (WAVE_FORMAT_PCM, 16) => AudioFileFormat::WavS16,
                    (WAVE_FORMAT_IEEE_FLOAT, 32) => AudioFileFormat::WavF32,
                    (tag, bits) => bail!(
                        "WAV: only mono s16 (tag 1) or f32 (tag 3) audio supported, \
                         got tag {tag} / {bits}-bit"
                    ),
                };
                reader.seek(SeekFrom::Start(w.data_start))?;
                (
                    format,
                    f64::from(w.rate_hz),
                    w.data_start,
                    Some(w.data_start + u64::from(w.data_len)),
                )
            } else {
                if !(params.rate_hz_hint.is_finite() && params.rate_hz_hint > 0.0) {
                    bail!(
                        "file_audio_source: headerless raw f32 needs a positive \
                         rate_hz_hint, got {}",
                        params.rate_hz_hint
                    );
                }
                (AudioFileFormat::RawF32, params.rate_hz_hint, 0, None)
            };

        Ok(Self {
            format,
            rate_hz,
            gain,
            loop_playback: params.loop_playback,
            reader,
            data_start,
            data_end,
            scratch: Vec::new(),
            eof: false,
        })
    }

    #[must_use]
    pub fn format(&self) -> AudioFileFormat {
        self.format
    }

    #[must_use]
    pub fn rate_hz(&self) -> f64 {
        self.rate_hz
    }

    #[must_use]
    pub fn is_eof(&self) -> bool {
        self.eof
    }

    /// Read up to `dst.len()` bytes, capped at the end of the audio
    /// region. `Ok(0)` on EOF. Standalone (not `&mut self`) so the
    /// caller can hold a disjoint borrow on `self.scratch`.
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
impl Block for FileAudioSource {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "FileAudioSource",
            placement: Placement::NativeOnly,
            inputs: &[],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::RealF32,
            }],
            params: &[
                ParamSpec {
                    key: "path",
                    label: "Path",
                    kind: ParamKind::Text { default: "" },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Filesystem path to a mono audio file. Auto-detects WAV mono s16/f32 (rate from header) or headerless raw f32 (needs rate_hz_hint).",
                },
                ParamSpec {
                    key: "gain",
                    label: "Gain",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1_000.0,
                        step: 0.01,
                        default: 1.0,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Linear gain on the replayed audio. 1.0 = as recorded.",
                },
                ParamSpec {
                    key: "loop",
                    label: "Loop",
                    kind: ParamKind::Toggle { default: false },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "True = rewind on EOF (cyclic). False = stop when the clip ends.",
                },
            ],
            ai_notes: "Replays a recorded mono audio clip as a real_f32 stream. Loopback/test only (drives a modulator → demod chain for end-to-end fidelity scoring); not used by any listen preset.",
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn output_rate_hz(&self, _port: usize) -> Option<f64> {
        Some(self.rate_hz)
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let n = io
            .outputs
            .iter()
            .find(|p| p.name == "out")
            .and_then(|p| match &p.buf {
                OutBuf::RealF32(s) => Some(s.len()),
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
            .and_then(OutputPort::as_real_f32_mut)
            .expect("out port length already checked above");

        let g = self.gain;
        match self.format {
            AudioFileFormat::WavS16 => {
                for (i, slot) in out.iter_mut().take(full_samples).enumerate() {
                    let off = i * 2;
                    let s = i16::from_le_bytes([self.scratch[off], self.scratch[off + 1]]);
                    *slot = f32::from(s) * (1.0 / 32_768.0) * g;
                }
            }
            AudioFileFormat::RawF32 | AudioFileFormat::WavF32 => {
                for (i, slot) in out.iter_mut().take(full_samples).enumerate() {
                    let off = i * 4;
                    let s = f32::from_le_bytes([
                        self.scratch[off],
                        self.scratch[off + 1],
                        self.scratch[off + 2],
                        self.scratch[off + 3],
                    ]);
                    *slot = s * g;
                }
            }
        }
        for item in out.iter_mut().take(n).skip(full_samples) {
            *item = 0.0;
        }

        let mut w = Work::new();
        w.produced[0] = full_samples;
        Ok(w)
    }
}

impl BlockFactory for FileAudioSource {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: FileAudioSourceParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(FileAudioSource::new(&p)?))
    }
}

#[cfg(test)]
mod tests {
    use super::{AudioFileFormat, FileAudioSource, FileAudioSourceParams};
    use crate::block::{Block, BlockIo, OutBuf, OutputPort, PortMeta};
    use std::io::Cursor;
    use std::path::PathBuf;

    fn mono_wav(format_tag: u16, bits: u16, rate: u32, body: &[u8]) -> Vec<u8> {
        let data_len = u32::try_from(body.len()).unwrap();
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36u32 + data_len).to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&format_tag.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // mono
        v.extend_from_slice(&rate.to_le_bytes());
        let block_align = u32::from(bits) / 8;
        v.extend_from_slice(&(rate * block_align).to_le_bytes());
        v.extend_from_slice(&u16::try_from(block_align).unwrap().to_le_bytes());
        v.extend_from_slice(&bits.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data_len.to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    fn make(bytes: Vec<u8>, rate_hint: f64, gain: f32, looping: bool) -> FileAudioSource {
        FileAudioSource::from_reader(
            &FileAudioSourceParams {
                path: PathBuf::new(),
                rate_hz_hint: rate_hint,
                gain,
                loop_playback: looping,
            },
            Box::new(Cursor::new(bytes)),
        )
        .unwrap()
    }

    fn pull(src: &mut FileAudioSource, n: usize) -> Vec<f32> {
        let mut buf = vec![0.0_f32; n];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::RealF32(&mut buf),
        }];
        let mut io = BlockIo {
            inputs: &mut [],
            outputs: &mut outputs,
        };
        src.process(&mut io).unwrap();
        buf
    }

    #[test]
    fn reads_mono_s16_with_gain() {
        let body: Vec<u8> = [16_384i16, -16_384, 0, 32_767]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut src = make(mono_wav(1, 16, 48_000, &body), 0.0, 2.0, false);
        assert_eq!(src.format(), AudioFileFormat::WavS16);
        assert!((src.rate_hz() - 48_000.0).abs() < 0.5);
        // Pull past the 4 samples to exercise zero-fill + EOF (matches
        // FileIqSource semantics: EOF is set only when a read yields 0).
        let out = pull(&mut src, 6);
        assert!((out[0] - 1.0).abs() < 1e-3); // 0.5 × gain 2
        assert!((out[1] + 1.0).abs() < 1e-3);
        assert!(out[2].abs() < 1e-6);
        assert!(out[4].abs() < 1e-6); // zero-filled past EOF
        assert!(out[5].abs() < 1e-6);
        assert!(src.is_eof());
    }

    #[test]
    fn reads_mono_f32_wav() {
        let body: Vec<u8> = [0.25_f32, -0.5, 0.75]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut src = make(mono_wav(3, 32, 8_000, &body), 0.0, 1.0, false);
        assert_eq!(src.format(), AudioFileFormat::WavF32);
        let out = pull(&mut src, 3);
        assert!((out[0] - 0.25).abs() < 1e-6);
        assert!((out[1] + 0.5).abs() < 1e-6);
        assert!((out[2] - 0.75).abs() < 1e-6);
    }

    #[test]
    fn raw_f32_uses_rate_hint_and_loops() {
        let body: Vec<u8> = [1.0_f32, 2.0]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let mut src = make(body, 16_000.0, 1.0, true);
        assert_eq!(src.format(), AudioFileFormat::RawF32);
        assert!((src.rate_hz() - 16_000.0).abs() < 0.5);
        let out = pull(&mut src, 4);
        assert!((out[0] - 1.0).abs() < 1e-6);
        assert!((out[1] - 2.0).abs() < 1e-6);
        assert!((out[2] - 1.0).abs() < 1e-6); // looped
        assert!((out[3] - 2.0).abs() < 1e-6);
        assert!(!src.is_eof());
    }

    #[test]
    fn rejects_stereo_wav() {
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&36u32.to_le_bytes());
        v.extend_from_slice(b"WAVE");
        v.extend_from_slice(b"fmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes()); // stereo → reject
        v.extend_from_slice(&48_000u32.to_le_bytes());
        v.extend_from_slice(&192_000u32.to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&0u32.to_le_bytes());
        let res = FileAudioSource::from_reader(
            &FileAudioSourceParams {
                path: PathBuf::new(),
                rate_hz_hint: 1.0,
                gain: 1.0,
                loop_playback: false,
            },
            Box::new(Cursor::new(v)),
        );
        assert!(res.is_err());
    }
}
