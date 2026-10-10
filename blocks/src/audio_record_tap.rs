//! "Record what the operator hears" — a live WAV tee for real-audio
//! blocks that grow `record_path` / `record_max_seconds` params.
//!
//! Sits inside [`crate::ws_bridge::WsBridgeTxF32`], the node→browser
//! audio bridge `env_split` inserts at every audio crossing. That block
//! sees exactly the samples the browser is about to play (post-demod,
//! post-shaper; post-NR under `audio_split=server`), so a capture is a
//! faithful copy of the listening session and never has to swap presets
//! or retune. The bridge's input rate varies by preset and profile
//! (240 kS/s raw demod on wbfm/balanced, 48 kS/s under server), so the
//! tap resamples to a fixed 48 kHz before writing — one WAV shape for
//! every capture.
//!
//! Lifecycle mirrors the IQ/FFT tees (`Channelizer` / `LogMagU8`):
//! `record_path` non-empty opens a file (sidecar written immediately),
//! samples stream through `push`, and either the caller clears the path
//! or `record_max_seconds` fires — then the WAV header is patched, the
//! sidecar rewritten with the final count, and the path cleared so a
//! stale param never silently grows a file.

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result};
use ferrite_liquid_dsp::MsResamp;
use serde_json::{json, Value};

use crate::file_audio_sink::{FileAudioSink, FileAudioSinkParams};

/// Every capture WAV is written at this rate regardless of the tapped
/// block's input rate — the sidecar carries the source rate for
/// reference.
pub const RECORD_RATE_HZ: f64 = 48_000.0;
const STOPBAND_DB: f32 = 60.0;

pub struct AudioRecordTap {
    record_path: PathBuf,
    record_max_seconds: f64,
    input_rate_hz: f64,
    active: Option<Active>,
}

struct Active {
    sink: FileAudioSink,
    path: PathBuf,
    /// `None` when the input is already at [`RECORD_RATE_HZ`].
    resamp: Option<MsResamp>,
    scratch: Vec<f32>,
    started: Instant,
    source_rate_hz: f64,
}

impl Default for AudioRecordTap {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioRecordTap {
    #[must_use]
    pub fn new() -> Self {
        Self {
            record_path: PathBuf::new(),
            record_max_seconds: 0.0,
            input_rate_hz: 0.0,
            active: None,
        }
    }

    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.active.is_some()
    }

    #[must_use]
    pub fn record_path(&self) -> &Path {
        &self.record_path
    }

    /// Note the tapped block's input rate (from `init` / `update_rates`).
    /// A rate change mid-recording rebuilds the resampler so the file
    /// stays at 48 kHz.
    pub fn set_input_rate(&mut self, rate_hz: f64) -> Result<()> {
        if !(rate_hz.is_finite() && rate_hz > 0.0) || rate_hz == self.input_rate_hz {
            return Ok(());
        }
        self.input_rate_hz = rate_hz;
        if let Some(a) = self.active.as_mut() {
            a.resamp = make_resamp(rate_hz)?;
            a.source_rate_hz = rate_hz;
        }
        Ok(())
    }

    /// Apply the `record_path` / `record_max_seconds` keys of a live
    /// delta. Returns `Ok(false)` untouched when the delta holds any
    /// other key, so the owning block can fall through to its own
    /// handling (or reject).
    pub fn apply_live(&mut self, obj: &serde_json::Map<String, Value>) -> Result<bool> {
        const KEYS: &[&str] = &["record_path", "record_max_seconds"];
        if !obj.keys().all(|k| KEYS.contains(&k.as_str())) {
            return Ok(false);
        }
        if let Some(v) = obj.get("record_max_seconds").and_then(Value::as_f64) {
            self.record_max_seconds = v.max(0.0);
        }
        if let Some(v) = obj.get("record_path") {
            let new_path = match v {
                Value::String(s) => PathBuf::from(s),
                _ => PathBuf::new(),
            };
            self.record_path.clone_from(&new_path);
            if new_path.as_os_str().is_empty() {
                self.finish()?;
            } else {
                self.start(new_path)?;
            }
        }
        Ok(true)
    }

    /// Open a fresh recording at `path`, closing any prior one first.
    pub fn start(&mut self, path: PathBuf) -> Result<()> {
        self.finish()?;
        let sink = FileAudioSink::new(&FileAudioSinkParams {
            path: path.clone(),
            rate_hz: RECORD_RATE_HZ,
            max_seconds: self.record_max_seconds,
        })?;
        let resamp = if self.input_rate_hz > 0.0 {
            make_resamp(self.input_rate_hz)?
        } else {
            None
        };
        write_sidecar(&path, &self.sidecar_value(0, self.input_rate_hz, 0.0))?;
        self.active = Some(Active {
            sink,
            path,
            resamp,
            scratch: Vec::new(),
            started: Instant::now(),
            source_rate_hz: self.input_rate_hz,
        });
        Ok(())
    }

    /// Patch the WAV header + rewrite the sidecar with the final count.
    /// Idempotent.
    pub fn finish(&mut self) -> Result<()> {
        let Some(mut a) = self.active.take() else {
            return Ok(());
        };
        a.sink.finalise()?;
        let n = a.sink.samples_written();
        write_sidecar(
            &a.path,
            &self.sidecar_value(n, a.source_rate_hz, a.started.elapsed().as_secs_f64()),
        )
        .with_context(|| format!("update sidecar for {}", a.path.display()))?;
        Ok(())
    }

    /// Tee `samples` (at the tapped input rate) into the open recording.
    /// No-op when idle. When the cap fires the recording is finalised and
    /// `record_path` cleared.
    pub fn push(&mut self, samples: &[f32]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let capped = {
            let Some(a) = self.active.as_mut() else {
                return Ok(());
            };
            if a.resamp.is_none()
                && a.source_rate_hz != self.input_rate_hz
                && self.input_rate_hz > 0.0
            {
                // Rate learned after `start` (init order) — build now.
                a.resamp = make_resamp(self.input_rate_hz)?;
                a.source_rate_hz = self.input_rate_hz;
            }
            match a.resamp.as_mut() {
                None => {
                    a.sink.push_samples(samples)?;
                }
                Some(r) => {
                    let max_out = r.max_output_for(samples.len());
                    if a.scratch.len() < max_out {
                        a.scratch.resize(max_out, 0.0);
                    }
                    let n = r.execute(samples, &mut a.scratch[..max_out]);
                    a.sink.push_samples(&a.scratch[..n])?;
                }
            }
            a.sink.is_capped()
        };
        if capped {
            self.finish()?;
            self.record_path = PathBuf::new();
        }
        Ok(())
    }

    fn sidecar_value(&self, samples_written: u64, source_rate_hz: f64, elapsed_s: f64) -> Value {
        #[allow(clippy::cast_precision_loss)]
        let duration_s = samples_written as f64 / RECORD_RATE_HZ;
        json!({
            "kind": "audio",
            "format": "wav-pcm-s16-mono-audio",
            "sample_rate_hz": RECORD_RATE_HZ,
            "source_rate_hz": source_rate_hz,
            "samples_written": samples_written,
            "duration_s": duration_s,
            "wall_elapsed_s": elapsed_s,
            "record_max_seconds": self.record_max_seconds,
        })
    }
}

fn make_resamp(input_rate_hz: f64) -> Result<Option<MsResamp>> {
    if (input_rate_hz - RECORD_RATE_HZ).abs() < 0.5 {
        return Ok(None);
    }
    #[allow(clippy::cast_possible_truncation)]
    let rate = (RECORD_RATE_HZ / input_rate_hz) as f32;
    MsResamp::new(rate, STOPBAND_DB)
        .map(Some)
        .map_err(|e| anyhow::anyhow!("audio record tap resampler: {e}"))
}

/// Same convention as the IQ/FFT tees and the daemon's capture
/// orchestrator: `foo.wav` → `foo.json`.
fn sidecar_path_for(bin_path: &Path) -> PathBuf {
    let mut p = bin_path.to_path_buf();
    if p.extension().is_some() {
        p.set_extension("json");
    } else {
        let mut s = p.into_os_string();
        s.push(".json");
        p = PathBuf::from(s);
    }
    p
}

fn write_sidecar(bin_path: &Path, value: &Value) -> Result<()> {
    let side = sidecar_path_for(bin_path);
    std::fs::write(&side, serde_json::to_vec_pretty(value)?)
        .with_context(|| format!("write sidecar {}", side.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique scratch dir without a `tempfile` dev-dep (the blocks
    /// crate doesn't carry one).
    fn scratch_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ferrite-audio-tap-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn tone(rate: f64, secs: f64, f: f64) -> Vec<f32> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let n = (rate * secs) as usize;
        #[allow(clippy::cast_precision_loss)]
        (0..n)
            .map(|i| (core::f32::consts::TAU * f as f32 * i as f32 / rate as f32).sin() * 0.5)
            .collect()
    }

    fn wav_header(path: &Path) -> (u32, u32) {
        let b = std::fs::read(path).unwrap();
        assert_eq!(&b[0..4], b"RIFF");
        let rate = u32::from_le_bytes([b[24], b[25], b[26], b[27]]);
        let data_bytes = u32::from_le_bytes([b[40], b[41], b[42], b[43]]);
        (rate, data_bytes)
    }

    #[test]
    fn resamples_240k_to_48k_and_caps() {
        let dir = scratch_dir("cap");
        let path = dir.join("cap.wav");
        let mut tap = AudioRecordTap::new();
        tap.set_input_rate(240_000.0).unwrap();
        let delta: serde_json::Map<String, Value> = serde_json::from_value(json!({
            "record_path": path.to_str().unwrap(),
            "record_max_seconds": 0.25,
        }))
        .unwrap();
        assert!(tap.apply_live(&delta).unwrap());
        assert!(tap.is_recording());
        // Half a second of input in scheduler-sized chunks; the cap
        // fires at 0.25 s of *output*.
        let input = tone(240_000.0, 0.5, 1_000.0);
        for chunk in input.chunks(2048) {
            tap.push(chunk).unwrap();
        }
        assert!(!tap.is_recording(), "cap must finalise the recording");
        assert!(
            tap.record_path().as_os_str().is_empty(),
            "cap must clear the path"
        );
        let (rate, data_bytes) = wav_header(&path);
        assert_eq!(rate, 48_000);
        assert_eq!(data_bytes, 12_000 * 2, "0.25 s @ 48 k s16");
        let side: Value =
            serde_json::from_slice(&std::fs::read(sidecar_path_for(&path)).unwrap()).unwrap();
        assert_eq!(side["samples_written"], 12_000);
        assert_eq!(side["source_rate_hz"], 240_000.0);
        assert_eq!(side["sample_rate_hz"], 48_000.0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn passthrough_at_48k_and_explicit_stop() {
        let dir = scratch_dir("cap48");
        let path = dir.join("cap48.wav");
        let mut tap = AudioRecordTap::new();
        tap.set_input_rate(48_000.0).unwrap();
        tap.start(path.clone()).unwrap();
        tap.push(&tone(48_000.0, 0.1, 1_000.0)).unwrap();
        assert!(tap.is_recording());
        let stop: serde_json::Map<String, Value> =
            serde_json::from_value(json!({ "record_path": "" })).unwrap();
        assert!(tap.apply_live(&stop).unwrap());
        assert!(!tap.is_recording());
        let (rate, data_bytes) = wav_header(&path);
        assert_eq!(rate, 48_000);
        assert_eq!(data_bytes, 4_800 * 2);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn foreign_keys_are_not_consumed() {
        let mut tap = AudioRecordTap::new();
        let delta: serde_json::Map<String, Value> =
            serde_json::from_value(json!({ "stream_id": 5 })).unwrap();
        assert!(!tap.apply_live(&delta).unwrap());
    }
}
