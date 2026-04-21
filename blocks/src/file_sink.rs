//! IQ file sink — capture mode.
//!
//! Writes complex samples from an `iq_f32` input to a file. Mirrors
//! [`crate::file_source::FileIqSource`] on the capture side: the same two
//! formats that `FileIqSource` reads back are supported here, so capture →
//! replay is symmetric and an authored preset can drop straight into
//! `samples/<band>/`.
//!
//! Two formats:
//!
//!   * **`cf32` raw** — interleaved little-endian f32 pairs. Lossless, no
//!     header; the bytes flowing through the pipeline hit the file as-is.
//!     The sample rate is not embedded — it lives in the sidecar.
//!   * **`wav-s16`** — RIFF/WAVE PCM, stereo 16-bit, L=I R=Q, as emitted
//!     by SDR#/HDSDR. The sample rate is stamped into the `fmt ` chunk.
//!     [`Drop`] seeks back to the RIFF/data size fields at finalisation
//!     so the file parses correctly in other tools.
//!
//! A JSON sidecar (`<path>.json`) is written on open when
//! `write_sidecar` is set (default true), so every capture carries its
//! rate + centre frequency per the `samples/README.md` convention.
//!
//! [`Placement::NativeOnly`] — writing to a filesystem path is a native
//! concern. The browser cannot instantiate this block, and `env_split`
//! will refuse to pin it to the `browser` side.

use std::{
    fs::OpenOptions,
    io::{BufWriter, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context, Result};
use num_complex::Complex;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InBuf, InitCtx, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IqSinkFormat {
    /// Interleaved f32 LE (I, Q, I, Q, …). 8 B/sample, no header.
    Cf32,
    /// WAV RIFF/PCM, stereo 16-bit, L=I R=Q. 4 B/sample.
    WavS16,
}

impl IqSinkFormat {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "cf32" => Ok(Self::Cf32),
            "wav-s16" => Ok(Self::WavS16),
            other => bail!(
                "FileIqSink: unknown format {:?} (expected \"cf32\" or \"wav-s16\")",
                other
            ),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FileIqSinkParams {
    pub path: PathBuf,
    /// `"cf32"` or `"wav-s16"`. WAV gets header + trailing size patch;
    /// cf32 is raw bytes.
    pub format: String,
    /// Stamped into the WAV `fmt ` chunk and into the sidecar. For
    /// `cf32` it is sidecar-only (the format does not embed it).
    pub rate_hz: f64,
    /// Sidecar metadata. Neither format embeds centre frequency.
    pub center_freq_hz: f64,
    /// Stop writing after this many seconds of wall-clock audio. `0.0`
    /// disables the cap — the block writes until the pipeline stops.
    /// Past the cap, input is still consumed but not written, so the
    /// scheduler keeps ticking.
    pub max_seconds: f64,
    /// Emit `<path>.json` sidecar on open per `samples/README.md`.
    pub write_sidecar: bool,
}

impl Default for FileIqSinkParams {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            format: "cf32".into(),
            rate_hz: 0.0,
            center_freq_hz: 0.0,
            max_seconds: 0.0,
            write_sidecar: true,
        }
    }
}

/// `Write + Seek + Send` so tests can swap in an in-memory cursor.
pub trait WriteSeek: Write + Seek + Send {}
impl<T: Write + Seek + Send> WriteSeek for T {}

pub struct FileIqSink {
    format: IqSinkFormat,
    rate_hz: f64,
    center_freq_hz: f64,
    max_samples: Option<u64>,
    samples_written: u64,
    writer: Option<BufWriter<Box<dyn WriteSeek>>>,
    /// Offset of the WAV `data` chunk size u32 (4 B LE). `None` for cf32.
    wav_data_size_pos: Option<u64>,
    /// Scratch for `wav-s16` PCM conversion — avoids re-allocating on the
    /// hot path.
    scratch: Vec<u8>,
    /// Finalised on first `Drop` so replayed drops (unlikely but cheap to
    /// guard) don't seek a closed writer.
    finalised: bool,
}

impl FileIqSink {
    pub fn new(params: &FileIqSinkParams) -> Result<Self> {
        if params.path.as_os_str().is_empty() {
            bail!("FileIqSink: `path` is required");
        }
        let format = IqSinkFormat::parse(&params.format)?;
        if matches!(format, IqSinkFormat::WavS16) && !(params.rate_hz > 0.0) {
            bail!("FileIqSink: wav-s16 requires a positive rate_hz for the header");
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&params.path)
            .with_context(|| format!("open {}", params.path.display()))?;

        let writer: Box<dyn WriteSeek> = Box::new(file);
        let mut sink = Self::from_writer_with_rate(format, params, writer)?;

        if params.write_sidecar {
            if let Err(e) =
                write_sidecar(&params.path, format, params.rate_hz, params.center_freq_hz)
            {
                // Sidecar is a convenience, not load-bearing. Surface the
                // error as a warning by prefixing but continue.
                eprintln!(
                    "FileIqSink: sidecar write for {} failed: {e:#}",
                    params.path.display()
                );
            }
        }

        sink.max_samples = if params.max_seconds > 0.0 && params.rate_hz > 0.0 {
            Some((params.max_seconds * params.rate_hz).round() as u64)
        } else {
            None
        };

        Ok(sink)
    }

    /// Constructor parameterised over the writer for tests.
    pub fn from_writer(
        format: IqSinkFormat,
        rate_hz: f64,
        writer: Box<dyn WriteSeek>,
    ) -> Result<Self> {
        let params = FileIqSinkParams {
            format: String::new(),
            rate_hz,
            ..FileIqSinkParams::default()
        };
        Self::from_writer_with_rate(format, &params, writer)
    }

    fn from_writer_with_rate(
        format: IqSinkFormat,
        params: &FileIqSinkParams,
        writer: Box<dyn WriteSeek>,
    ) -> Result<Self> {
        let mut writer = BufWriter::new(writer);
        let wav_data_size_pos = match format {
            IqSinkFormat::Cf32 => None,
            IqSinkFormat::WavS16 => Some(write_wav_stub_header(&mut writer, params.rate_hz)?),
        };
        Ok(Self {
            format,
            rate_hz: params.rate_hz,
            center_freq_hz: params.center_freq_hz,
            max_samples: None,
            samples_written: 0,
            writer: Some(writer),
            wav_data_size_pos,
            scratch: Vec::new(),
            finalised: false,
        })
    }

    #[must_use]
    pub fn format(&self) -> IqSinkFormat {
        self.format
    }

    #[must_use]
    pub fn rate_hz(&self) -> f64 {
        self.rate_hz
    }

    #[must_use]
    pub fn center_freq_hz(&self) -> f64 {
        self.center_freq_hz
    }

    #[must_use]
    pub fn samples_written(&self) -> u64 {
        self.samples_written
    }

    fn write_samples(&mut self, samples: &[Complex<f32>]) -> Result<usize> {
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
            .ok_or_else(|| anyhow!("FileIqSink: writer already finalised"))?;
        match self.format {
            IqSinkFormat::Cf32 => {
                let need = allowed * 8;
                if self.scratch.len() < need {
                    self.scratch.resize(need, 0);
                }
                let buf = &mut self.scratch[..need];
                for (i, c) in samples[..allowed].iter().enumerate() {
                    let off = i * 8;
                    buf[off..off + 4].copy_from_slice(&c.re.to_le_bytes());
                    buf[off + 4..off + 8].copy_from_slice(&c.im.to_le_bytes());
                }
                writer.write_all(buf).context("cf32 write")?;
            }
            IqSinkFormat::WavS16 => {
                let need = allowed * 4;
                if self.scratch.len() < need {
                    self.scratch.resize(need, 0);
                }
                let buf = &mut self.scratch[..need];
                for (i, c) in samples[..allowed].iter().enumerate() {
                    let off = i * 4;
                    let l = clip_to_i16(c.re);
                    let r = clip_to_i16(c.im);
                    buf[off..off + 2].copy_from_slice(&l.to_le_bytes());
                    buf[off + 2..off + 4].copy_from_slice(&r.to_le_bytes());
                }
                writer.write_all(buf).context("wav-s16 write")?;
            }
        }
        self.samples_written = self.samples_written.saturating_add(allowed as u64);
        Ok(allowed)
    }

    /// Flush + patch WAV header sizes. Idempotent. Callers typically rely
    /// on `Drop` to trigger this, but tests call it explicitly to observe
    /// the final file without moving the sink.
    pub fn finalise(&mut self) -> Result<()> {
        if self.finalised {
            return Ok(());
        }
        self.finalised = true;
        let Some(mut writer) = self.writer.take() else {
            return Ok(());
        };
        writer.flush().context("flush")?;
        if let Some(data_size_pos) = self.wav_data_size_pos {
            let data_bytes: u32 =
                u32::try_from(self.samples_written.saturating_mul(4)).unwrap_or(u32::MAX);
            let inner = writer
                .into_inner()
                .map_err(|e| anyhow!("FileIqSink: unwrap writer: {}", e.into_error()))?;
            patch_wav_sizes(inner, data_size_pos, data_bytes)?;
        }
        Ok(())
    }
}

impl Drop for FileIqSink {
    fn drop(&mut self) {
        if let Err(e) = self.finalise() {
            eprintln!("FileIqSink: finalise on drop failed: {e:#}");
        }
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for FileIqSink {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "FileIqSink",
            placement: Placement::NativeOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[],
            params: &[
                ParamSpec {
                    key: "path",
                    label: "Path",
                    kind: ParamKind::Text { default: "" },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                },
                ParamSpec {
                    key: "format",
                    label: "Format",
                    kind: ParamKind::EnumString {
                        values: &["cf32", "wav-s16"],
                        default: "cf32",
                    },
                    reconfig_scope: ReconfigureScope::SourceRestart,
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
                },
            ],
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
        let InBuf::IqF32(samples) = &port.buf else {
            return Ok(w);
        };
        let n = samples.len();
        if n == 0 {
            return Ok(w);
        }
        // Ignore how many bytes landed on disk — the port contract is
        // "consumed == presented" for sinks. A short write past the
        // max_seconds cap or a filesystem hiccup still reports full
        // consume so the scheduler doesn't back-pressure upstream.
        let _ = self.write_samples(samples)?;
        // Once the cap fires, finalise immediately so the file is valid
        // even if the runtime is later SIGKILL'd before Drop runs.
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

impl BlockFactory for FileIqSink {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: FileIqSinkParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(FileIqSink::new(&p)?))
    }
}

fn clip_to_i16(x: f32) -> i16 {
    // Match SDR# convention: full-scale float 1.0 → i16::MAX, −1.0 →
    // i16::MIN. `as i16` saturates on overflow since Rust 1.45, but
    // clamping first makes the intent explicit.
    let scaled = (x.clamp(-1.0, 1.0) * 32_767.0).round();
    scaled as i16
}

/// Writes the WAV header with zero placeholders for the RIFF chunk size
/// and data chunk size. Returns the byte offset of the data chunk size
/// u32 so [`patch_wav_sizes`] can seek to it on finalise.
fn write_wav_stub_header<W: Write + Seek>(w: &mut W, rate_hz: f64) -> Result<u64> {
    if !(rate_hz > 0.0) || rate_hz > f64::from(u32::MAX) {
        bail!("FileIqSink: rate_hz out of WAV range: {rate_hz}");
    }
    let rate = rate_hz.round() as u32;
    let byte_rate = rate.saturating_mul(4);

    w.write_all(b"RIFF").context("RIFF")?;
    w.write_all(&0u32.to_le_bytes()).context("RIFF size stub")?; // patched on finalise
    w.write_all(b"WAVE").context("WAVE")?;

    w.write_all(b"fmt ").context("fmt id")?;
    w.write_all(&16u32.to_le_bytes()).context("fmt size")?;
    w.write_all(&1u16.to_le_bytes()).context("PCM tag")?; // PCM
    w.write_all(&2u16.to_le_bytes()).context("channels")?; // stereo
    w.write_all(&rate.to_le_bytes()).context("sample rate")?;
    w.write_all(&byte_rate.to_le_bytes()).context("byte rate")?;
    w.write_all(&4u16.to_le_bytes()).context("block align")?; // 2 ch * 2 B
    w.write_all(&16u16.to_le_bytes()).context("bits")?;

    w.write_all(b"data").context("data id")?;
    let data_size_pos = w.stream_position().context("tell data size pos")?;
    w.write_all(&0u32.to_le_bytes()).context("data size stub")?; // patched on finalise
    Ok(data_size_pos)
}

fn patch_wav_sizes<W: Write + Seek>(mut w: W, data_size_pos: u64, data_bytes: u32) -> Result<()> {
    w.seek(SeekFrom::Start(data_size_pos))
        .context("seek data size")?;
    w.write_all(&data_bytes.to_le_bytes())
        .context("patch data size")?;
    // RIFF chunk size = 36 + data_bytes (header is 44 B; RIFF size
    // excludes the first 8 B).
    let riff_size = data_bytes.saturating_add(36);
    w.seek(SeekFrom::Start(4)).context("seek riff size")?;
    w.write_all(&riff_size.to_le_bytes())
        .context("patch riff size")?;
    w.flush().context("flush after patch")?;
    Ok(())
}

fn write_sidecar(
    path: &Path,
    format: IqSinkFormat,
    rate_hz: f64,
    center_freq_hz: f64,
) -> Result<()> {
    let sidecar = sidecar_path(path);
    let fmt_tag = match format {
        IqSinkFormat::Cf32 => "cf32",
        IqSinkFormat::WavS16 => "wav-pcm-s16-stereo-iq",
    };
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let value = serde_json::json!({
        "file": file_name,
        "format": fmt_tag,
        "sample_rate_hz": rate_hz,
        "center_freq_hz": center_freq_hz,
        "source": { "origin": "ferrite-capture" },
    });
    let text = serde_json::to_string_pretty(&value)?;
    std::fs::write(&sidecar, text)
        .with_context(|| format!("write sidecar {}", sidecar.display()))?;
    Ok(())
}

fn sidecar_path(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    // Replace final extension with .json; if none, append .json.
    if p.extension().is_some() {
        p.set_extension("json");
    } else {
        let mut s = p.into_os_string();
        s.push(".json");
        p = PathBuf::from(s);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::{clip_to_i16, sidecar_path, FileIqSink, FileIqSinkParams, IqSinkFormat};
    use crate::block::{Block, BlockIo, InBuf, InputPort, PortMeta};
    use crate::file_source::{FileIqSource, FileIqSourceParams};
    use num_complex::Complex;
    use std::io::Cursor;
    use std::path::PathBuf;

    fn run_process(sink: &mut FileIqSink, samples: &[Complex<f32>]) -> usize {
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(samples),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut [],
        };
        let work = sink.process(&mut io).unwrap();
        work.consumed[0]
    }

    fn cf32_sink() -> (FileIqSink, std::rc::Rc<std::cell::RefCell<Vec<u8>>>) {
        let shared = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u8>::new()));
        let w = SharedCursor {
            inner: Cursor::new(Vec::new()),
            shared: shared.clone(),
        };
        let sink = FileIqSink::from_writer(IqSinkFormat::Cf32, 48_000.0, Box::new(w)).unwrap();
        (sink, shared)
    }

    struct SharedCursor {
        inner: Cursor<Vec<u8>>,
        shared: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    }
    impl std::io::Write for SharedCursor {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let n = self.inner.write(buf)?;
            *self.shared.borrow_mut() = self.inner.get_ref().clone();
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.inner.flush()
        }
    }
    impl std::io::Seek for SharedCursor {
        fn seek(&mut self, p: std::io::SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(p)
        }
    }
    // SharedCursor is constructed on one thread in tests; `Rc` is !Send
    // so we mark it as logically Send for the trait object. Not shared
    // across threads, so this is sound for the test use. We provide
    // `unsafe impl Send` here and keep the raw test-only type local.
    // `Rc` is !Send, so the blanket `WriteSeek` impl (which requires
    // `Send`) does not cover `SharedCursor` by default. The test runs on
    // one thread — no samples cross a thread boundary — so marking it
    // Send by fiat is sound for this fixture.
    unsafe impl Send for SharedCursor {}

    #[test]
    fn spec_is_native_only_iq_in() {
        let s = FileIqSink::spec();
        assert_eq!(s.type_name, "FileIqSink");
        assert!(matches!(s.placement, crate::block::Placement::NativeOnly));
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.outputs.len(), 0);
        assert_eq!(s.inputs[0].port_type, crate::block::PortType::IqF32);
    }

    #[test]
    fn clip_to_i16_bounds() {
        assert_eq!(clip_to_i16(0.0), 0);
        assert_eq!(clip_to_i16(1.0), 32_767);
        assert_eq!(clip_to_i16(-1.0), -32_767);
        assert_eq!(clip_to_i16(2.0), 32_767);
        assert_eq!(clip_to_i16(-2.0), -32_767);
    }

    #[test]
    fn sidecar_path_replaces_extension_or_appends() {
        assert_eq!(
            sidecar_path(std::path::Path::new("/tmp/cap.wav")),
            PathBuf::from("/tmp/cap.json")
        );
        assert_eq!(
            sidecar_path(std::path::Path::new("/tmp/cap.cf32")),
            PathBuf::from("/tmp/cap.json")
        );
        assert_eq!(
            sidecar_path(std::path::Path::new("/tmp/capnoext")),
            PathBuf::from("/tmp/capnoext.json")
        );
    }

    #[test]
    fn cf32_round_trip_through_file_iq_source() {
        let (mut sink, shared) = cf32_sink();
        let samples: Vec<Complex<f32>> = (0..16)
            .map(|i| Complex::new(i as f32 * 0.1, i as f32 * -0.05))
            .collect();
        let consumed = run_process(&mut sink, &samples);
        assert_eq!(consumed, 16);
        sink.finalise().unwrap();
        drop(sink);

        let bytes = shared.borrow().clone();
        assert_eq!(bytes.len(), 16 * 8, "8 B/sample for cf32");

        let mut src = FileIqSource::from_reader(
            &FileIqSourceParams {
                path: PathBuf::new(),
                rate_hz_hint: 48_000.0,
                center_freq_hz: 0.0,
                loop_playback: false,
            },
            Box::new(Cursor::new(bytes)),
        )
        .unwrap();
        let mut out = vec![Complex::new(0.0_f32, 0.0); 16];
        let mut outputs = [crate::block::OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: crate::block::OutBuf::IqF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut [],
            outputs: &mut outputs,
        };
        src.process(&mut io).unwrap();
        for (a, b) in out.iter().zip(samples.iter()) {
            assert!(
                (a.re - b.re).abs() < 1e-6 && (a.im - b.im).abs() < 1e-6,
                "{a:?} vs {b:?}"
            );
        }
    }

    #[test]
    fn wav_round_trip_through_file_iq_source() {
        let shared = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u8>::new()));
        let w = SharedCursor {
            inner: Cursor::new(Vec::new()),
            shared: shared.clone(),
        };
        let mut sink = FileIqSink::from_writer(IqSinkFormat::WavS16, 8_000.0, Box::new(w)).unwrap();

        let samples: Vec<Complex<f32>> =
            vec![(0.5, -0.5), (-1.0, 0.999_969_5), (0.0, 0.0), (0.25, 0.25)]
                .into_iter()
                .map(|(re, im)| Complex::new(re, im))
                .collect();
        run_process(&mut sink, &samples);
        sink.finalise().unwrap();
        drop(sink);

        let bytes = shared.borrow().clone();
        // 44 B header + 4 samples × 4 B
        assert_eq!(bytes.len(), 44 + 16);
        // Basic header sanity.
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        // Data chunk size patched correctly.
        assert_eq!(&bytes[40..44], &16u32.to_le_bytes());
        // RIFF size = data + 36.
        assert_eq!(&bytes[4..8], &(16u32 + 36).to_le_bytes());

        let mut src = FileIqSource::from_reader(
            &FileIqSourceParams {
                path: PathBuf::new(),
                rate_hz_hint: 0.0,
                center_freq_hz: 0.0,
                loop_playback: false,
            },
            Box::new(Cursor::new(bytes)),
        )
        .unwrap();
        assert!((src.rate_hz() - 8_000.0).abs() < 0.5);

        let mut out = vec![Complex::new(0.0_f32, 0.0); 4];
        let mut outputs = [crate::block::OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: crate::block::OutBuf::IqF32(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut [],
            outputs: &mut outputs,
        };
        src.process(&mut io).unwrap();
        // s16 round-trip is lossy at the LSB; 1.2e-4 covers the quantisation floor.
        let tol = 1.5e-4;
        for (a, b) in out.iter().zip(samples.iter()) {
            assert!(
                (a.re - b.re).abs() < tol && (a.im - b.im).abs() < tol,
                "{a:?} vs {b:?}"
            );
        }
    }

    #[test]
    fn max_seconds_caps_writes_but_still_consumes() {
        // Simulate real disk so rate-based cap math kicks in via new().
        let dir = tempdir();
        let path = dir.join("cap.cf32");
        let params = FileIqSinkParams {
            path: path.clone(),
            format: "cf32".into(),
            rate_hz: 1_000.0,
            center_freq_hz: 0.0,
            max_seconds: 0.01, // 10 samples at 1 kHz
            write_sidecar: false,
        };
        let mut sink = FileIqSink::new(&params).unwrap();

        // Push 25 samples — only the first 10 should land on disk.
        let samples: Vec<Complex<f32>> = (0..25).map(|i| Complex::new(i as f32, 0.0)).collect();
        let consumed = run_process(&mut sink, &samples);
        assert_eq!(consumed, 25, "sink must consume every presented sample");
        assert_eq!(sink.samples_written(), 10);

        sink.finalise().unwrap();
        let disk = std::fs::read(&path).unwrap();
        assert_eq!(disk.len(), 10 * 8);

        // Further writes stay at the cap.
        let more: Vec<Complex<f32>> = vec![Complex::new(99.0, 99.0); 8];
        let _ = run_process(&mut sink, &more);
        assert_eq!(sink.samples_written(), 10);

        std::fs::remove_file(&path).ok();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn sidecar_emitted_on_open_and_parses() {
        let dir = tempdir();
        let path = dir.join("cap.cf32");
        let params = FileIqSinkParams {
            path: path.clone(),
            format: "cf32".into(),
            rate_hz: 2_400_000.0,
            center_freq_hz: 100_100_000.0,
            max_seconds: 0.0,
            write_sidecar: true,
        };
        let sink = FileIqSink::new(&params).unwrap();
        drop(sink);
        let sidecar = dir.join("cap.json");
        let text = std::fs::read_to_string(&sidecar).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["file"], "cap.cf32");
        assert_eq!(v["format"], "cf32");
        assert!((v["sample_rate_hz"].as_f64().unwrap() - 2_400_000.0).abs() < 1e-3);
        assert!((v["center_freq_hz"].as_f64().unwrap() - 100_100_000.0).abs() < 1e-3);
        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&sidecar).ok();
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_wav_without_rate() {
        let params = FileIqSinkParams {
            path: PathBuf::from("/tmp/should-not-be-created.wav"),
            format: "wav-s16".into(),
            rate_hz: 0.0,
            ..FileIqSinkParams::default()
        };
        let Err(err) = FileIqSink::new(&params) else {
            panic!("wav-s16 with rate_hz=0 must fail");
        };
        assert!(
            format!("{err}").contains("rate_hz"),
            "error should mention rate_hz: {err}"
        );
    }

    #[test]
    fn rejects_unknown_format() {
        let params = FileIqSinkParams {
            path: PathBuf::from("/tmp/should-not-be-created.bin"),
            format: "cs16".into(),
            rate_hz: 1.0,
            ..FileIqSinkParams::default()
        };
        let Err(err) = FileIqSink::new(&params) else {
            panic!("unknown format must fail");
        };
        assert!(
            format!("{err}").contains("cs16"),
            "error should name the bad format: {err}"
        );
    }

    fn tempdir() -> PathBuf {
        let mut base = std::env::temp_dir();
        let pid = std::process::id();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        base.push(format!("ferrite-file-sink-{pid}-{ts}"));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
