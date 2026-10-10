//! Live cf32 tee for IQ blocks that grow `record_path` /
//! `record_max_seconds` params — the wideband sibling of
//! [`crate::audio_record_tap`].
//!
//! Lives inside [`crate::soapy_source::SoapySource`]: the raw full-span
//! stream straight off the radio, before the DC blocker and the
//! channelizer, is what a "wideband IQ capture" means. Teeing it here
//! means the capture never swaps presets or retunes — the operator keeps
//! listening while the file fills. Format is always raw interleaved
//! little-endian f32 pairs (`cf32`, the shape `FileIqSource` replays and
//! the channelizer tee already writes) with a sidecar carrying the rate
//! and RF centre the format can't embed.
//!
//! Disk rate is `8 × sample_rate` bytes/s — 16 MB/s at 2 MS/s. The cap
//! is in *samples* (`record_max_seconds × rate`) so a capture's length
//! is exact regardless of tick jitter.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use num_complex::Complex;
use serde_json::{json, Value};

use crate::record::Recorder;

pub struct IqRecordTap {
    record_path: PathBuf,
    record_max_seconds: f64,
    sample_rate_hz: f64,
    center_freq_hz: f64,
    active: Option<Active>,
}

struct Active {
    recorder: Recorder,
    path: PathBuf,
    scratch: Vec<u8>,
    samples_written: u64,
    /// `None` = uncapped.
    max_samples: Option<u64>,
}

impl Default for IqRecordTap {
    fn default() -> Self {
        Self::new()
    }
}

impl IqRecordTap {
    #[must_use]
    pub fn new() -> Self {
        Self {
            record_path: PathBuf::new(),
            record_max_seconds: 0.0,
            sample_rate_hz: 0.0,
            center_freq_hz: 0.0,
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

    /// Note the tapped stream's rate + RF centre (sidecar metadata; the
    /// rate also sizes the sample cap). Call before `start` and whenever
    /// the owner retunes — the sidecar is rewritten at finish with the
    /// values current then.
    pub fn set_meta(&mut self, sample_rate_hz: f64, center_freq_hz: f64) {
        if sample_rate_hz.is_finite() && sample_rate_hz > 0.0 {
            self.sample_rate_hz = sample_rate_hz;
        }
        if center_freq_hz.is_finite() {
            self.center_freq_hz = center_freq_hz;
        }
    }

    /// Apply the `record_path` / `record_max_seconds` keys of a live
    /// delta. `Ok(false)` untouched when the delta holds any other key.
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
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let max_samples = if self.record_max_seconds > 0.0 && self.sample_rate_hz > 0.0 {
            Some((self.record_max_seconds * self.sample_rate_hz).round() as u64)
        } else {
            None
        };
        let recorder = Recorder::open(&path, max_samples.map(|n| n * 8))?;
        write_sidecar(&path, &self.sidecar_value(0))?;
        self.active = Some(Active {
            recorder,
            path,
            scratch: Vec::new(),
            samples_written: 0,
            max_samples,
        });
        Ok(())
    }

    /// Flush + rewrite the sidecar with the final count. Idempotent.
    pub fn finish(&mut self) -> Result<()> {
        let Some(mut a) = self.active.take() else {
            return Ok(());
        };
        a.recorder.finalise()?;
        write_sidecar(&a.path, &self.sidecar_value(a.samples_written))
            .with_context(|| format!("update sidecar for {}", a.path.display()))?;
        Ok(())
    }

    /// Tee `samples` into the open recording. No-op when idle. When the
    /// sample cap fires the recording is finalised and `record_path`
    /// cleared so a stale param never silently grows a file.
    pub fn push(&mut self, samples: &[Complex<f32>]) -> Result<()> {
        if samples.is_empty() {
            return Ok(());
        }
        let capped = {
            let Some(a) = self.active.as_mut() else {
                return Ok(());
            };
            let allowed = match a.max_samples {
                None => samples.len(),
                Some(cap) => (cap.saturating_sub(a.samples_written) as usize).min(samples.len()),
            };
            if allowed > 0 {
                let need = allowed * 8;
                if a.scratch.len() < need {
                    a.scratch.resize(need, 0);
                }
                for (i, c) in samples[..allowed].iter().enumerate() {
                    let off = i * 8;
                    a.scratch[off..off + 4].copy_from_slice(&c.re.to_le_bytes());
                    a.scratch[off + 4..off + 8].copy_from_slice(&c.im.to_le_bytes());
                }
                let wrote = a.recorder.write(&a.scratch[..need])?;
                a.samples_written += (wrote / 8) as u64;
            }
            a.max_samples.is_some_and(|cap| a.samples_written >= cap)
        };
        if capped {
            self.finish()?;
            self.record_path = PathBuf::new();
        }
        Ok(())
    }

    fn sidecar_value(&self, samples_written: u64) -> Value {
        #[allow(clippy::cast_precision_loss)]
        let duration_s = if self.sample_rate_hz > 0.0 {
            samples_written as f64 / self.sample_rate_hz
        } else {
            0.0
        };
        json!({
            "kind": "iq",
            "wideband": true,
            "format": "cf32",
            "sample_rate_hz": self.sample_rate_hz,
            "center_freq_hz": self.center_freq_hz,
            "bytes_written": samples_written * 8,
            "samples_written": samples_written,
            "duration_s": duration_s,
            "record_max_seconds": self.record_max_seconds,
            "source": { "origin": "ferrite-record" },
        })
    }
}

/// `foo.cf32` → `foo.json` — same convention as every other tee and the
/// daemon's capture orchestrator.
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

    fn scratch_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ferrite-iq-tap-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn caps_by_sample_count_and_writes_cf32_with_sidecar() {
        let dir = scratch_dir("cap");
        let path = dir.join("wide.cf32");
        let mut tap = IqRecordTap::new();
        tap.set_meta(2_000_000.0, 98_500_000.0);
        let delta: serde_json::Map<String, Value> = serde_json::from_value(json!({
            "record_path": path.to_str().unwrap(),
            "record_max_seconds": 0.01, // 20 000 samples
        }))
        .unwrap();
        assert!(tap.apply_live(&delta).unwrap());
        // 25 000 samples in 4096-chunks: ramp so the file is checkable.
        #[allow(clippy::cast_precision_loss)]
        let input: Vec<Complex<f32>> = (0..25_000)
            .map(|i| Complex::new(i as f32, -(i as f32)))
            .collect();
        for chunk in input.chunks(4096) {
            tap.push(chunk).unwrap();
        }
        assert!(!tap.is_recording(), "cap must finalise");
        assert!(
            tap.record_path().as_os_str().is_empty(),
            "cap must clear the path"
        );
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes.len(),
            20_000 * 8,
            "exactly max_seconds × rate samples"
        );
        // Last sample is index 19 999, re = 19999.0, im = -19999.0.
        let last = &bytes[bytes.len() - 8..];
        assert_eq!(
            f32::from_le_bytes([last[0], last[1], last[2], last[3]]),
            19_999.0
        );
        assert_eq!(
            f32::from_le_bytes([last[4], last[5], last[6], last[7]]),
            -19_999.0
        );
        let side: Value =
            serde_json::from_slice(&std::fs::read(sidecar_path_for(&path)).unwrap()).unwrap();
        assert_eq!(side["samples_written"], 20_000);
        assert_eq!(side["sample_rate_hz"], 2_000_000.0);
        assert_eq!(side["center_freq_hz"], 98_500_000.0);
        assert_eq!(side["format"], "cf32");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn uncapped_until_explicit_stop() {
        let dir = scratch_dir("stop");
        let path = dir.join("wide.cf32");
        let mut tap = IqRecordTap::new();
        tap.set_meta(1_000_000.0, 0.0);
        tap.start(path.clone()).unwrap();
        tap.push(&vec![Complex::new(0.5, 0.25); 1000]).unwrap();
        tap.push(&vec![Complex::new(0.5, 0.25); 1000]).unwrap();
        assert!(tap.is_recording());
        let stop: serde_json::Map<String, Value> =
            serde_json::from_value(json!({ "record_path": "" })).unwrap();
        assert!(tap.apply_live(&stop).unwrap());
        assert!(!tap.is_recording());
        assert_eq!(std::fs::read(&path).unwrap().len(), 2000 * 8);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn foreign_keys_are_not_consumed() {
        let mut tap = IqRecordTap::new();
        let delta: serde_json::Map<String, Value> =
            serde_json::from_value(json!({ "gain_db": 20 })).unwrap();
        assert!(!tap.apply_live(&delta).unwrap());
    }
}
