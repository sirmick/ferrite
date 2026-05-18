//! Audio file sink — records demodulated audio to a WAV file.
//!
//! The symmetric counterpart to [`crate::file_sink::FileIqSink`], but one
//! port type down the pipeline: it consumes `RealF32` audio (as emitted
//! by `FmDemod`, `AmDemod`, etc.) and writes a mono 16-bit PCM WAV that
//! any audio tool can open. This is what gives `ferrited` a headless
//! "record an FM station to disk" mode — no browser, no AudioWorklet.
//!
//! Format: WAV RIFF/PCM, mono 16-bit LE, `rate_hz` stamped into the
//! `fmt ` chunk. [`Drop`] seeks back to the RIFF/data size fields so the
//! file parses correctly if the runtime exits cleanly. As with
//! `FileIqSink`, `max_seconds` caps the on-disk duration while still
//! reporting full consume so the scheduler doesn't back-pressure
//! upstream.
//!
//! [`Placement::NativeOnly`]: filesystem paths are a native concern; the
//! browser cannot instantiate this block, and `env_split` refuses to
//! pin it to the `browser` side.

use std::{
    fs::OpenOptions,
    io::{BufWriter, Write},
    path::PathBuf,
};

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InBuf, InitCtx, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};
use crate::file_sink::WriteSeek;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FileAudioSinkParams {
    pub path: PathBuf,
    /// Stamped into the WAV `fmt ` chunk. Must be positive.
    pub rate_hz: f64,
    /// Stop writing after this many seconds. `0.0` disables the cap;
    /// past the cap input is still consumed, just not written.
    pub max_seconds: f64,
}

impl Default for FileAudioSinkParams {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            rate_hz: 48_000.0,
            max_seconds: 0.0,
        }
    }
}

pub struct FileAudioSink {
    rate_hz: f64,
    max_samples: Option<u64>,
    samples_written: u64,
    writer: Option<BufWriter<Box<dyn WriteSeek>>>,
    data_size_pos: u64,
    scratch: Vec<u8>,
    finalised: bool,
}

impl FileAudioSink {
    pub fn new(params: &FileAudioSinkParams) -> Result<Self> {
        if params.path.as_os_str().is_empty() {
            bail!("FileAudioSink: `path` is required");
        }
        if !(params.rate_hz > 0.0) {
            bail!("FileAudioSink: rate_hz must be positive for the WAV header");
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&params.path)
            .with_context(|| format!("open {}", params.path.display()))?;
        let writer: Box<dyn WriteSeek> = Box::new(file);
        let mut sink = Self::from_writer(params.rate_hz, writer)?;
        sink.max_samples = if params.max_seconds > 0.0 {
            Some((params.max_seconds * params.rate_hz).round() as u64)
        } else {
            None
        };
        Ok(sink)
    }

    /// Constructor parameterised over the writer so tests can swap in an
    /// in-memory cursor.
    pub fn from_writer(rate_hz: f64, writer: Box<dyn WriteSeek>) -> Result<Self> {
        if !(rate_hz > 0.0) {
            bail!("FileAudioSink: rate_hz must be positive");
        }
        let mut writer = BufWriter::new(writer);
        let data_size_pos = crate::wav::write_pcm_s16_stub_header(&mut writer, rate_hz, 1)?;
        Ok(Self {
            rate_hz,
            max_samples: None,
            samples_written: 0,
            writer: Some(writer),
            data_size_pos,
            scratch: Vec::new(),
            finalised: false,
        })
    }

    #[must_use]
    pub fn rate_hz(&self) -> f64 {
        self.rate_hz
    }

    #[must_use]
    pub fn samples_written(&self) -> u64 {
        self.samples_written
    }

    fn write_samples(&mut self, samples: &[f32]) -> Result<usize> {
        if samples.is_empty() {
            return Ok(0);
        }
        let allowed = match self.max_samples {
            None => samples.len(),
            Some(cap) => {
                let remaining = cap.saturating_sub(self.samples_written) as usize;
                remaining.min(samples.len())
            }
        };
        if allowed == 0 {
            return Ok(0);
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow!("FileAudioSink: writer already finalised"))?;
        let need = allowed * 2;
        if self.scratch.len() < need {
            self.scratch.resize(need, 0);
        }
        let buf = &mut self.scratch[..need];
        for (i, s) in samples[..allowed].iter().enumerate() {
            let off = i * 2;
            let v = clip_to_i16(*s);
            buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
        }
        writer.write_all(buf).context("audio wav write")?;
        self.samples_written = self.samples_written.saturating_add(allowed as u64);
        Ok(allowed)
    }

    /// Flush + patch WAV header sizes. Idempotent.
    pub fn finalise(&mut self) -> Result<()> {
        if self.finalised {
            return Ok(());
        }
        self.finalised = true;
        let Some(mut writer) = self.writer.take() else {
            return Ok(());
        };
        writer.flush().context("flush")?;
        let data_bytes: u32 =
            u32::try_from(self.samples_written.saturating_mul(2)).unwrap_or(u32::MAX);
        let inner = writer
            .into_inner()
            .map_err(|e| anyhow!("FileAudioSink: unwrap writer: {}", e.into_error()))?;
        crate::wav::patch_wav_sizes(inner, self.data_size_pos, data_bytes)?;
        Ok(())
    }
}

impl Drop for FileAudioSink {
    fn drop(&mut self) {
        if let Err(e) = self.finalise() {
            tracing::warn!(error = ?e, "FileAudioSink: finalise on drop failed");
        }
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for FileAudioSink {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "FileAudioSink",
            placement: Placement::NativeOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[],
            params: &[
                ParamSpec {
                    key: "path",
                    label: "Path",
                    kind: ParamKind::Text { default: "" },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Output WAV path.",
                },
                ParamSpec {
                    key: "rate_hz",
                    label: "Sample rate",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 192_000.0,
                        step: 1.0,
                        default: 48_000.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Audio sample rate to write into the WAV header. Must match the upstream chain.",
                },
                ParamSpec {
                    key: "max_seconds",
                    label: "Max duration",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 3600.0,
                        step: 0.1,
                        default: 0.0,
                        unit: "s",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Stop after N seconds (0 = no cap).",
                },
            ],
            ai_notes: "Node-side audio recording sink — writes WAV-s16 mono. Terminal block in `*-audio-record` flowgraphs. Use when you want to capture demodulated audio to disk on the server without going through the browser AudioSink.",
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let mut w = Work::new();
        let Some(port) = io.inputs.iter().find(|p| p.name == "in") else {
            return Ok(w);
        };
        let InBuf::RealF32(samples) = &port.buf else {
            return Ok(w);
        };
        let n = samples.len();
        if n == 0 {
            return Ok(w);
        }
        let _ = self.write_samples(samples)?;
        if let Some(cap) = self.max_samples {
            if self.samples_written >= cap && !self.finalised {
                self.finalise()?;
            }
        }
        w.consumed[0] = n;
        Ok(w)
    }

    fn stop(&mut self) -> Result<()> {
        self.finalise()
    }
}

impl BlockFactory for FileAudioSink {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: FileAudioSinkParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(FileAudioSink::new(&p)?))
    }
}

fn clip_to_i16(x: f32) -> i16 {
    let scaled = (x.clamp(-1.0, 1.0) * 32_767.0).round();
    scaled as i16
}

#[cfg(test)]
mod tests {
    use super::{clip_to_i16, FileAudioSink, FileAudioSinkParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, PortMeta};
    use crate::test_support::{SharedBuf, SharedCursor};
    use std::path::PathBuf;

    fn run_process(sink: &mut FileAudioSink, samples: &[f32]) -> usize {
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(samples),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut [],
        };
        let work = sink.process(&mut io).unwrap();
        work.consumed[0]
    }

    fn in_memory_sink(rate: f64) -> (FileAudioSink, SharedBuf) {
        let (w, shared) = SharedCursor::new();
        let sink = FileAudioSink::from_writer(rate, Box::new(w)).unwrap();
        (sink, shared)
    }

    #[test]
    fn spec_is_native_only_real_in() {
        let s = FileAudioSink::spec();
        assert_eq!(s.type_name, "FileAudioSink");
        assert!(matches!(s.placement, crate::block::Placement::NativeOnly));
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.outputs.len(), 0);
        assert_eq!(s.inputs[0].port_type, crate::block::PortType::RealF32);
    }

    #[test]
    fn clip_to_i16_saturates() {
        assert_eq!(clip_to_i16(0.0), 0);
        assert_eq!(clip_to_i16(1.0), 32_767);
        assert_eq!(clip_to_i16(-1.0), -32_767);
        assert_eq!(clip_to_i16(2.0), 32_767);
        assert_eq!(clip_to_i16(-2.0), -32_767);
    }

    #[test]
    fn wav_header_and_data_are_patched_on_finalise() {
        let (mut sink, shared) = in_memory_sink(48_000.0);
        let samples: Vec<f32> = (0..10).map(|i| (i as f32) / 10.0).collect();
        let consumed = run_process(&mut sink, &samples);
        assert_eq!(consumed, 10);
        sink.finalise().unwrap();
        drop(sink);

        let bytes = shared.borrow().clone();
        // 44 B mono header + 10 samples × 2 B
        assert_eq!(bytes.len(), 44 + 20);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[12..16], b"fmt ");
        // channels = 1, sample rate = 48_000, bits = 16
        assert_eq!(&bytes[22..24], &1u16.to_le_bytes());
        assert_eq!(&bytes[24..28], &48_000u32.to_le_bytes());
        assert_eq!(&bytes[34..36], &16u16.to_le_bytes());
        // data chunk size = 20, RIFF size = 20 + 36
        assert_eq!(&bytes[40..44], &20u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &(20u32 + 36).to_le_bytes());
        // First sample is 0.0 → 0i16; second is 0.1 → 3277 (rounded).
        assert_eq!(&bytes[44..46], &0i16.to_le_bytes());
        let s2 = i16::from_le_bytes([bytes[46], bytes[47]]);
        assert!((s2 - 3277).abs() <= 1, "got {s2}");
    }

    #[test]
    fn max_seconds_caps_writes_but_still_consumes() {
        let dir = tempdir();
        let path = dir.join("audio.wav");
        let params = FileAudioSinkParams {
            path: path.clone(),
            rate_hz: 1_000.0,
            max_seconds: 0.01, // 10 samples
        };
        let mut sink = FileAudioSink::new(&params).unwrap();
        let samples: Vec<f32> = (0..25).map(|i| (i as f32) * 0.01).collect();
        let consumed = run_process(&mut sink, &samples);
        assert_eq!(consumed, 25, "sink must consume every presented sample");
        assert_eq!(sink.samples_written(), 10);
        sink.finalise().unwrap();

        let disk = std::fs::read(&path).unwrap();
        assert_eq!(disk.len(), 44 + 10 * 2);
        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_missing_path() {
        let p = FileAudioSinkParams {
            path: PathBuf::new(),
            rate_hz: 48_000.0,
            max_seconds: 0.0,
        };
        let Err(err) = FileAudioSink::new(&p) else {
            panic!("empty path must fail");
        };
        assert!(format!("{err}").contains("path"));
    }

    #[test]
    fn rejects_non_positive_rate() {
        let p = FileAudioSinkParams {
            path: PathBuf::from("/tmp/should-not-be-created.wav"),
            rate_hz: 0.0,
            max_seconds: 0.0,
        };
        let Err(err) = FileAudioSink::new(&p) else {
            panic!("zero rate must fail");
        };
        assert!(format!("{err}").contains("rate_hz"));
    }

    fn tempdir() -> PathBuf {
        let mut base = std::env::temp_dir();
        let pid = std::process::id();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        base.push(format!("ferrite-file-audio-sink-{pid}-{ts}"));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
