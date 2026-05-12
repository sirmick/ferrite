//! `LogMagU8` — complex FFT bins → dBFS quantised to u8.
//!
//! Each input bin's power is normalised by the FFT size to compute dBFS,
//! exponentially smoothed across frames, and linearly mapped from a
//! **fixed** `[SERVER_FLOOR_DBFS, SERVER_CEIL_DBFS]` window to `[0, 255]`.
//! That's a deliberate architectural choice: the server's quantisation
//! window is wide and constant (−160..0 dBFS, 0.625 dB/byte), and every
//! display-range decision is made client-side on top of the byte stream.
//! No more "weak signal invisible because the preset shipped floor=−100"
//! foot-gun; no more server round-trip to zoom the spectrum.
//!
//! Live-tunable params:
//! - `alpha` — per-bin EMA smoothing factor.
//! - `record_path` — set non-empty to side-tee the byte stream to disk
//!   while the live pipeline keeps driving the UI; empty to stop. A
//!   sidecar JSON describing `frame_size`, the captured input rate, and
//!   the centre frequency is written alongside (and rewritten with the
//!   final byte / frame count on close).
//! - `record_max_seconds` — wall-clock cap; closes the file
//!   automatically when exceeded. `0` disables the cap (record until
//!   `record_path` is cleared or the pipeline stops). Wall-clock is
//!   used rather than byte count because the FFT block's
//!   `max_frames_per_second` throttle would otherwise make a
//!   byte-derived cap mis-translate.
//!
//! Native-only on the recording path. The block keeps `Placement::Either`
//! so the browser can still run the smoothing+quantise math, but the
//! file-I/O fields are no-ops on `wasm32`.

use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, Instant};

#[cfg(not(target_arch = "wasm32"))]
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutputPort, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work, MAX_PORTS,
};
#[cfg(not(target_arch = "wasm32"))]
use crate::record::Recorder;

/// Low end of the server quantisation window. Byte 0 maps to this.
pub const SERVER_FLOOR_DBFS: f32 = -160.0;
/// High end of the server quantisation window. Byte 255 maps to this.
pub const SERVER_CEIL_DBFS: f32 = 0.0;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LogMagU8Params {
    pub size: usize,
    /// Exponential smoothing factor in `(0, 1]`. 1.0 means "no smoothing"
    /// (each frame overwrites the previous); lower values hold noise
    /// floors steadier.
    pub alpha: f32,
    /// Live-tunable. Non-empty starts a side-tee write of the u8 byte
    /// stream to the path. Empty stops + finalises (sidecar updated with
    /// the final byte / frame count). Native-only — no-op on `wasm32`.
    pub record_path: PathBuf,
    /// Live-tunable. `0.0` = unbounded; positive caps wall-clock recording
    /// duration. Wall clock dodges the FFT throttle: if the FFT block is
    /// emitting at, say, 30 fps instead of nominal `rate / size`, a
    /// byte-derived cap would record for the wrong wall duration.
    pub record_max_seconds: f64,
}

impl Default for LogMagU8Params {
    fn default() -> Self {
        Self {
            size: 4096,
            alpha: 0.3,
            record_path: PathBuf::new(),
            record_max_seconds: 0.0,
        }
    }
}

pub struct LogMagU8 {
    params: LogMagU8Params,
    /// Per-bin smoothed dBFS value. Initialised to the server floor so
    /// the first frame emits the colormap's black level instead of a
    /// spike.
    smoothed: Vec<f32>,
    /// Sidecar metadata captured at init. Stamped into the recording
    /// sidecar so an offline tool can label time + frequency axes.
    /// `0.0` until init runs; recording works without these but the
    /// sidecar is less useful.
    input_sample_rate_hz: f64,
    input_center_freq_hz: f64,
    /// Active recording. `None` when `record_path` is empty.
    #[cfg(not(target_arch = "wasm32"))]
    recording: Option<Recording>,
}

#[cfg(not(target_arch = "wasm32"))]
struct Recording {
    recorder: Recorder,
    bin_path: PathBuf,
    /// Wall-clock instant of the first byte written. `None` until the
    /// first non-empty `process()` call lands. Used for the
    /// `record_max_seconds` cap.
    started_at: Option<Instant>,
}

impl LogMagU8 {
    #[must_use]
    pub fn new(params: LogMagU8Params) -> Self {
        let smoothed = vec![SERVER_FLOOR_DBFS; params.size];
        Self {
            params,
            smoothed,
            input_sample_rate_hz: 0.0,
            input_center_freq_hz: 0.0,
            #[cfg(not(target_arch = "wasm32"))]
            recording: None,
        }
    }

    pub fn set_alpha(&mut self, v: f32) {
        self.params.alpha = v.clamp(0.0, 1.0);
    }

    #[must_use]
    pub fn smoothed(&self) -> &[f32] {
        &self.smoothed
    }

    /// True when a side-tee write is currently open. Native-only — the
    /// browser placement keeps the byte-emit path but the recording
    /// fields are absent.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Open a fresh recording at `path`. Closes any existing recording
    /// first so a mid-record path swap doesn't leak the open file.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_recording(&mut self, path: PathBuf) -> Result<()> {
        self.finish_recording()?;
        let recorder = Recorder::open(&path, None)?;
        // Initial sidecar with `bytes_written: 0` — gives an offline
        // tool the format/frame-size/freq context immediately, even if
        // the runtime is killed before finish_recording rewrites the
        // final count.
        write_sidecar(&path, &self.sidecar_value(0))?;
        self.recording = Some(Recording {
            recorder,
            bin_path: path,
            started_at: None,
        });
        Ok(())
    }

    /// Flush + rewrite the sidecar with the final frame count. Idempotent
    /// — safe from `stop()`, `Drop`, and apply_live_params(record_path="").
    #[cfg(not(target_arch = "wasm32"))]
    fn finish_recording(&mut self) -> Result<()> {
        let Some(mut rec) = self.recording.take() else {
            return Ok(());
        };
        let bytes_written = rec.recorder.bytes_written();
        rec.recorder.finalise()?;
        write_sidecar(&rec.bin_path, &self.sidecar_value(bytes_written))
            .with_context(|| format!("update sidecar for {}", rec.bin_path.display()))?;
        Ok(())
    }

    /// Tap helper called from `process` after the byte stream is
    /// computed. Writes to the file, stamps the start instant on first
    /// call, and closes the recording when `record_max_seconds` fires.
    #[cfg(not(target_arch = "wasm32"))]
    fn tap_recording(&mut self, bytes: &[u8]) -> Result<()> {
        let cap_secs = self.params.record_max_seconds;
        // Borrow scope: take a &mut on `recording`, write/stamp, then
        // drop the borrow before potentially calling finish_recording
        // (which itself wants a &mut self).
        let cap_fired = {
            let Some(rec) = self.recording.as_mut() else {
                return Ok(());
            };
            if rec.started_at.is_none() {
                rec.started_at = Some(Instant::now());
            }
            // A short write past the cap returns Ok(0) and silently
            // closes the inner writer — block still owns the Recording
            // wrapper until cap_fired triggers finish_recording below.
            rec.recorder.write(bytes)?;
            let started = rec.started_at.expect("set above");
            cap_secs > 0.0 && started.elapsed() >= Duration::from_secs_f64(cap_secs)
        };
        if cap_fired {
            self.finish_recording()?;
            // Clear the live param so a follow-up status query reflects
            // that the cap closed the file. Sets to empty string in the
            // params struct only; the daemon still sees the original
            // value in /api/flowgraph until the next refresh.
            self.params.record_path = PathBuf::new();
        }
        Ok(())
    }

    /// Sidecar JSON shape — used both at open (with `bytes_written: 0`)
    /// and at finalise (with the real count). Frames-written is derived
    /// for offline tools that prefer that unit.
    #[cfg(not(target_arch = "wasm32"))]
    fn sidecar_value(&self, bytes_written: u64) -> serde_json::Value {
        let frame_size = self.params.size as u64;
        let frames_written = if frame_size > 0 {
            bytes_written / frame_size
        } else {
            0
        };
        serde_json::json!({
            "format": "fft-u8-raw",
            "frame_size": self.params.size,
            "alpha": self.params.alpha,
            "sample_rate_hz": self.input_sample_rate_hz,
            "center_freq_hz": self.input_center_freq_hz,
            "fft_floor_dbfs": SERVER_FLOOR_DBFS,
            "fft_ceil_dbfs": SERVER_CEIL_DBFS,
            "bytes_written": bytes_written,
            "frames_written": frames_written,
            "source": { "origin": "ferrite-record" },
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for LogMagU8 {
    fn drop(&mut self) {
        if let Err(e) = self.finish_recording() {
            eprintln!("LogMagU8: finish_recording on drop: {e:#}");
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn write_sidecar(bin_path: &std::path::Path, value: &serde_json::Value) -> Result<()> {
    let sidecar = sidecar_path_for(bin_path);
    let json = serde_json::to_string_pretty(value)?;
    std::fs::write(&sidecar, json)
        .with_context(|| format!("write sidecar {}", sidecar.display()))?;
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn sidecar_path_for(bin_path: &std::path::Path) -> PathBuf {
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

#[ferrite_blocks_macros::ferrite_block]
impl Block for LogMagU8 {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "LogMagU8",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::IqF32,
            }],
            outputs: &[PortSpec {
                name: "out",
                port_type: PortType::FftU8,
            }],
            params: &[
                ParamSpec {
                    key: "size",
                    label: "FFT size",
                    kind: ParamKind::EnumNumeric {
                        // Mirrors the FFT block's ladder so the two stay
                        // in lockstep when the UI flips one of them.
                        values: &[
                            1024.0, 2048.0, 4096.0, 8192.0, 16384.0, 32768.0, 65536.0, 131072.0,
                        ],
                        default: 4096.0,
                        unit: "bins",
                    },
                    // Output bin count — downstream consumers re-init.
                    reconfig_scope: ReconfigureScope::Downstream,
                },
                ParamSpec {
                    key: "alpha",
                    label: "Smoothing",
                    kind: ParamKind::Range {
                        min: 0.01,
                        max: 1.0,
                        step: 0.01,
                        default: 0.3,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "record_path",
                    label: "Record path",
                    kind: ParamKind::Text { default: "" },
                    // Live: opening / closing a file is a self-block
                    // side effect — no port shapes change.
                    reconfig_scope: ReconfigureScope::SelfBlock,
                },
                ParamSpec {
                    key: "record_max_seconds",
                    label: "Record cap",
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

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        // Capture the input rate + RF centre into the sidecar so an
        // offline tool reading the byte stream knows the time + freq
        // axes without round-tripping through the daemon.
        self.input_sample_rate_hz = ctx.input_rate("in").unwrap_or(0.0);
        // TODO: today the runtime hardcodes PortMeta.center_freq_hz to
        // 0.0 (see runtime/src/runtime.rs ~L671/764) — this read is
        // future-correct for when center-freq propagation lands. Until
        // then, the sidecar's `center_freq_hz` is 0; offline tools have
        // to combine with `/api/source` to get the actual RF centre.
        self.input_center_freq_hz = ctx
            .input_meta
            .iter()
            .find(|(n, _)| *n == "in")
            .map(|(_, m)| m.center_freq_hz)
            .unwrap_or(0.0);
        // Honour a construction-time `record_path` — apply_live_params
        // doesn't fire for params set in the preset itself, so an
        // authored "record on load" preset would otherwise silently
        // drop the path.
        #[cfg(not(target_arch = "wasm32"))]
        if !self.params.record_path.as_os_str().is_empty() && self.recording.is_none() {
            let path = self.params.record_path.clone();
            self.start_recording(path)?;
        }
        Ok(())
    }

    /// Live-tunable: `alpha` (EMA factor), `record_path` (start/stop
    /// the side-tee write), `record_max_seconds` (wall-clock cap, 0 =
    /// unbounded). `size` is structural and falls through to a block
    /// rebuild.
    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let Some(obj) = delta.as_object() else {
            return Ok(false);
        };
        const LIVE_KEYS: &[&str] = &["alpha", "record_path", "record_max_seconds"];
        if !obj.keys().all(|k| LIVE_KEYS.contains(&k.as_str())) {
            return Ok(false);
        }
        if let Some(v) = obj.get("alpha").and_then(|v| v.as_f64()) {
            self.set_alpha(v as f32);
        }
        if let Some(v) = obj.get("record_max_seconds").and_then(|v| v.as_f64()) {
            // Negative -> 0 (unbounded). The cap is checked against the
            // wall-clock instant of the first written byte; a mid-record
            // change applies on the next process() tick.
            self.params.record_max_seconds = v.max(0.0);
        }
        if let Some(v) = obj.get("record_path") {
            let new_path = match v {
                serde_json::Value::String(s) => PathBuf::from(s),
                _ => PathBuf::new(),
            };
            self.params.record_path = new_path.clone();
            #[cfg(not(target_arch = "wasm32"))]
            {
                if new_path.as_os_str().is_empty() {
                    self.finish_recording()?;
                } else {
                    self.start_recording(new_path)?;
                }
            }
        }
        Ok(true)
    }

    fn output_capacity_hints(&self) -> [usize; MAX_PORTS] {
        let mut h = [0; MAX_PORTS];
        h[0] = self.params.size;
        h
    }

    fn forecast(&self, _noutput_items: usize) -> Option<[usize; MAX_PORTS]> {
        // LogMagU8 operates on whole FFT bin blocks: consume `size`
        // cf32 bins and emit `size` u8 magnitudes, atomically. Tell the
        // scheduler not to call us before a full window is available —
        // upstream is `FftBlock` which emits exactly `size` samples at
        // a time, so the wire fills in one shot.
        let mut f = [0; MAX_PORTS];
        f[0] = self.params.size;
        Some(f)
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let n = self.params.size;
        let src = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_iq_f32);
        let Some(src) = src else {
            return Ok(Work::new());
        };
        if src.len() < n {
            return Ok(Work::new());
        }
        let dst = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(OutputPort::as_fft_u8_mut);
        let Some(dst) = dst else {
            return Ok(Work::new());
        };
        if dst.len() < n {
            return Ok(Work::new());
        }

        // Fixed wide quantisation window — client handles any visual
        // zoom via a display-range remap on the byte stream.
        let floor = SERVER_FLOOR_DBFS;
        let ceil = SERVER_CEIL_DBFS;
        let alpha = self.params.alpha;
        let scale = 255.0 / (ceil - floor).max(1e-6);
        #[allow(clippy::cast_precision_loss)]
        let norm = 1.0 / (n as f32 * n as f32);

        for (i, s) in src[..n].iter().enumerate() {
            let power = (s.re * s.re + s.im * s.im) * norm;
            let db = 10.0 * power.max(1e-20).log10();
            let smoothed = alpha * db + (1.0 - alpha) * self.smoothed[i];
            self.smoothed[i] = smoothed;
            let clipped = (smoothed - floor).clamp(0.0, ceil - floor);
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            let byte = (clipped * scale).round().clamp(0.0, 255.0) as u8;
            dst[i] = byte;
        }

        #[cfg(not(target_arch = "wasm32"))]
        // Tap the just-emitted bytes to disk (no-op when no recording is
        // active). Done after the smoothing/quantise loop so a tap
        // failure can't poison the live waterfall — we'd rather drop
        // recording than freeze the UI.
        self.tap_recording(&dst[..n])?;

        let mut work = Work::new();
        work.consumed[0] = n;
        work.produced[0] = n;
        Ok(work)
    }

    fn stop(&mut self) -> Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        self.finish_recording()?;
        Ok(())
    }
}

impl BlockFactory for LogMagU8 {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: LogMagU8Params = crate::block::deserialize_params(params)?;
        Ok(Box::new(LogMagU8::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{LogMagU8, LogMagU8Params};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use num_complex::Complex;

    fn run(block: &mut LogMagU8, input: &[Complex<f32>]) -> Vec<u8> {
        let n = input.len();
        let mut out = vec![0_u8; n];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::IqF32(input),
        }];
        let mut outputs = [OutputPort {
            name: "out",
            meta: PortMeta::default(),
            buf: OutBuf::FftU8(&mut out),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let _ = block.process(&mut io).unwrap();
        out
    }

    #[test]
    fn zero_input_goes_to_floor() {
        // alpha=1.0 skips smoothing: zero magnitude → dBFS below floor → 0.
        let n = 64;
        let params = LogMagU8Params {
            size: n,
            alpha: 1.0,
            ..LogMagU8Params::default()
        };
        let mut block = LogMagU8::new(params);
        let input = vec![Complex::new(0.0_f32, 0.0); n];
        let out = run(&mut block, &input);
        for b in out {
            assert_eq!(b, 0);
        }
    }

    #[test]
    fn peak_bin_hits_ceiling() {
        // Magnitude n in one bin → power = n² / n² = 1.0 → dBFS = 0 →
        // SERVER_CEIL_DBFS = 0, maps to 255.
        let n = 64;
        let params = LogMagU8Params {
            size: n,
            alpha: 1.0,
            ..LogMagU8Params::default()
        };
        let mut block = LogMagU8::new(params);
        let mut input = vec![Complex::new(0.0_f32, 0.0); n];
        #[allow(clippy::cast_precision_loss)]
        {
            input[n / 2] = Complex::new(n as f32, 0.0);
        }
        let out = run(&mut block, &input);
        assert_eq!(out[n / 2], 255);
        assert_eq!(out[0], 0);
    }

    #[test]
    fn midpoint_of_server_window_maps_to_half() {
        // Power = 1e-8 → dBFS = -80. SERVER window is [-160, 0] so -80
        // is the exact halfway point → maps to ~128.
        let n = 64;
        let params = LogMagU8Params {
            size: n,
            alpha: 1.0,
            ..LogMagU8Params::default()
        };
        let mut block = LogMagU8::new(params);
        // power = |s|² / n² = 1e-8 → |s|² = n² · 1e-8 → |s| = n · sqrt(1e-8).
        #[allow(clippy::cast_precision_loss)]
        let mag = (n as f32) * (1e-8_f32).sqrt();
        let input = vec![Complex::new(mag, 0.0); n];
        let out = run(&mut block, &input);
        for b in out {
            assert!(b.abs_diff(128) <= 2, "expected ~128, got {b}");
        }
    }

    #[test]
    fn smoothing_converges() {
        // With alpha = 0.5, feeding ceil-level input repeatedly should
        // climb towards 255 over a handful of frames.
        let n = 16;
        let params = LogMagU8Params {
            size: n,
            alpha: 0.5,
            ..LogMagU8Params::default()
        };
        let mut block = LogMagU8::new(params);
        #[allow(clippy::cast_precision_loss)]
        let peak = Complex::new(n as f32, 0.0);
        let input = vec![peak; n];
        let mut last = 0_u8;
        for _ in 0..100 {
            let out = run(&mut block, &input);
            last = out[0];
        }
        assert!(last >= 250, "expected convergence to ceil, got {last}");
    }

    #[test]
    fn live_record_path_writes_bytes_and_sidecar_then_stops_on_clear() {
        // Push frames through the block while record_path is set; verify
        // the bin file matches the byte stream and the sidecar reports
        // the right `bytes_written` after a path-clear stops it.
        let dir = tempdir();
        let path = dir.join("fft.bin");
        let n = 16;
        let params = LogMagU8Params {
            size: n,
            alpha: 1.0,
            ..LogMagU8Params::default()
        };
        let mut block = LogMagU8::new(params);

        // Start recording via apply_live_params.
        block
            .apply_live_params(&serde_json::json!({
                "record_path": path.to_str().unwrap(),
            }))
            .unwrap();
        assert!(block.is_recording());

        // Push 3 frames; each frame emits n=16 bytes so 48 B total on disk.
        #[allow(clippy::cast_precision_loss)]
        let input = vec![Complex::new(n as f32, 0.0); n];
        let mut bytes_emitted: Vec<u8> = Vec::new();
        for _ in 0..3 {
            let out = run(&mut block, &input);
            bytes_emitted.extend_from_slice(&out);
        }

        // Stop recording — clearing record_path triggers finish_recording.
        block
            .apply_live_params(&serde_json::json!({ "record_path": "" }))
            .unwrap();
        assert!(!block.is_recording());

        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk.len(), 48);
        assert_eq!(on_disk, bytes_emitted, "on-disk bytes match emit stream");

        let sidecar = dir.join("fft.json");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&sidecar).unwrap()).unwrap();
        assert_eq!(v["format"], "fft-u8-raw");
        assert_eq!(v["frame_size"], n);
        assert_eq!(v["bytes_written"], 48);
        assert_eq!(v["frames_written"], 3);

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&sidecar).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn record_max_seconds_zero_is_unbounded() {
        // Cap=0 should not abort recording — a small smoke-test that
        // the wall-clock check is bypassed when max_seconds is 0.
        let dir = tempdir();
        let path = dir.join("fft.bin");
        let n = 16;
        let mut block = LogMagU8::new(LogMagU8Params {
            size: n,
            alpha: 1.0,
            ..LogMagU8Params::default()
        });
        block
            .apply_live_params(&serde_json::json!({
                "record_path": path.to_str().unwrap(),
                "record_max_seconds": 0.0,
            }))
            .unwrap();
        let input = vec![Complex::new(0.0_f32, 0.0); n];
        for _ in 0..5 {
            let _ = run(&mut block, &input);
        }
        assert!(block.is_recording(), "cap=0 must not auto-stop");
        block
            .apply_live_params(&serde_json::json!({ "record_path": "" }))
            .unwrap();

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(dir.join("fft.json")).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    fn tempdir() -> std::path::PathBuf {
        let mut base = std::env::temp_dir();
        let pid = std::process::id();
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        base.push(format!("ferrite-logmag-record-{pid}-{ts}"));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
