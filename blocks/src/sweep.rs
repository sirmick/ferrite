//! Stepped sweep — the engine behind the spectrum analyzer's panorama.
//!
//! A stepped sweep hops the radio's LO across a range wider than one
//! span: tune → drop the samples still in flight from the old LO → FFT
//! and average a few frames → keep the flat middle of the passband →
//! emit one self-describing **row** → next step. Rows tile the range
//! exactly (each step owns `[lo, hi)` with no overlap), so a consumer
//! stitches a pass by placing rows at their `f_lo_hz`.
//!
//! The loop ([`run_sweep`]) is generic over [`SweepRadio`], so the whole
//! thing runs against [`FakeBand`] — a synthetic band with carriers,
//! bursts and noise — with no hardware: the [`SweepSim`] block (tests,
//! UI development, CI) and `SweepSource` (the real radio, `soapysdr`
//! feature) share [`SweepRunner`].
//!
//! Timing numbers come from `docs/plans/specan.md` (RSPdx over
//! SoapyRemote): no hardware timestamps, so stale samples are dropped
//! **by count** after `tune` returns (`discard_ms`); no settling ramp.
//!
//! Rows go out as NDJSON on the `events` port → `ui:panorama` →
//! `EventStore` → store kind `panorama`, keyed by `step`. `power` is
//! base64 of little-endian `i16` centi-dB (dBFS × 100).

#![cfg(not(target_arch = "wasm32"))]

use std::collections::VecDeque;
use std::f32::consts::TAU;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use base64::Engine;
use num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use serde::Deserialize;

use crate::block::{
    drain_events_port, Block, BlockFactory, BlockIo, BlockSpec, InitCtx, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work, MAX_PORTS,
};

/// Masked bin marker in the encoded power array.
pub const MASKED: i16 = i16::MIN;

/// Rows queued between the hop thread and `process()`; the oldest drop
/// if nothing drains (a stalled graph must not grow memory).
const ROW_QUEUE_CAP: usize = 64;

/// Bytes the `events` port is provisioned for — a few rows.
const EVENTS_HINT: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

/// The sweep knobs (shared by `SweepSim` and `SweepSource`). All live:
/// a change restarts the pass at step 0.
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(default)]
pub struct SweepParams {
    /// Range to sweep, Hz.
    pub sweep_start_hz: f64,
    pub sweep_stop_hz: f64,
    /// Requested resolution (bin spacing), Hz. The FFT size is the power
    /// of two nearest `rate / rbw_hz` (256..=65536); rows report the
    /// actual `bin_hz`.
    pub rbw_hz: f64,
    /// FFT frames averaged per step (power average). Dwell per step is
    /// `averages × fft_size / rate`.
    pub averages: u32,
    /// Samples to drop after each retune, ms. `0` = auto (50 ms at
    /// ≥ 6 MS/s, 90 ms below — the measured SoapyRemote in-flight).
    pub discard_ms: f64,
    /// Fraction of the sample rate that's flat enough to keep (the IF
    /// filter's passband): 0.8 at 10 MS/s on the RSPdx.
    pub usable_fraction: f64,
    /// Extra margin trimmed from the usable window, as a fraction of it.
    pub overlap: f64,
    /// Half-width around each step's LO that is interpolated over (the
    /// DC / LO-leakage spur), Hz.
    pub dc_mask_hz: f64,
}

impl Default for SweepParams {
    fn default() -> Self {
        Self {
            sweep_start_hz: 88e6,
            sweep_stop_hz: 108e6,
            rbw_hz: 2_500.0,
            averages: 16,
            discard_ms: 0.0,
            usable_fraction: 0.8,
            overlap: 0.05,
            dc_mask_hz: 5_000.0,
        }
    }
}

/// Keys of [`SweepParams`] — all applied live.
pub const SWEEP_LIVE_KEYS: &[&str] = &[
    "sweep_start_hz",
    "sweep_stop_hz",
    "rbw_hz",
    "averages",
    "discard_ms",
    "usable_fraction",
    "overlap",
    "dc_mask_hz",
];

/// One step: LO position and the slice of the range it owns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub center_hz: f64,
    pub own_lo_hz: f64,
    pub own_hi_hz: f64,
}

/// Everything the loop needs, resolved against the radio's rate.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepPlan {
    pub params: SweepParams,
    pub rate_hz: f64,
    pub fft_size: usize,
    pub bin_hz: f64,
    pub discard_samples: usize,
    pub steps: Vec<Step>,
}

impl SweepPlan {
    pub fn new(params: &SweepParams, rate_hz: f64) -> Result<Self> {
        let (lo, hi) = (
            params.sweep_start_hz.min(params.sweep_stop_hz),
            params.sweep_start_hz.max(params.sweep_stop_hz),
        );
        if !(rate_hz > 0.0 && lo.is_finite() && hi.is_finite() && hi > lo) {
            return Err(anyhow!(
                "sweep: need sweep_start_hz < sweep_stop_hz and a positive rate"
            ));
        }
        let want = (rate_hz / params.rbw_hz.max(1.0)).max(1.0);
        let fft_size = (want.log2().round().exp2() as usize).clamp(256, 65_536);
        let bin_hz = rate_hz / fft_size as f64;
        let usable = rate_hz * params.usable_fraction.clamp(0.05, 1.0);
        let step_w = usable * (1.0 - params.overlap.clamp(0.0, 0.9));
        let n = ((hi - lo) / step_w).ceil().max(1.0) as usize;
        if n > 100_000 {
            return Err(anyhow!(
                "sweep: {n} steps — narrow the range or raise the rate"
            ));
        }
        // Centre the tiling on the range so a single-step sweep sits its
        // LO in the middle.
        let tiled = n as f64 * step_w;
        let first_lo = lo - (tiled - (hi - lo)) / 2.0;
        let steps = (0..n)
            .map(|i| {
                let own_lo = first_lo + i as f64 * step_w;
                Step {
                    center_hz: own_lo + step_w / 2.0,
                    own_lo_hz: own_lo.max(lo),
                    own_hi_hz: (own_lo + step_w).min(hi),
                }
            })
            .collect();
        let discard_ms = if params.discard_ms > 0.0 {
            params.discard_ms
        } else if rate_hz >= 6e6 {
            50.0
        } else {
            90.0
        };
        Ok(Self {
            params: params.clone(),
            rate_hz,
            fft_size,
            bin_hz,
            discard_samples: (discard_ms * 1e-3 * rate_hz) as usize,
            steps,
        })
    }

    /// Wall time one pass should take with `per_step_overhead` of retune
    /// latency per step — the UI's pass-time readout.
    #[must_use]
    pub fn pass_seconds(&self, per_step_overhead: Duration) -> f64 {
        let dwell = (self.discard_samples + self.params.averages.max(1) as usize * self.fft_size)
            as f64
            / self.rate_hz;
        self.steps.len() as f64 * (dwell + per_step_overhead.as_secs_f64())
    }
}

// ---------------------------------------------------------------------------
// Spectrum of one step
// ---------------------------------------------------------------------------

/// Averaged, windowed power spectrum → cropped dBFS row.
pub struct StepSpectrum {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// `Σw²`-free normalisation: a full-scale tone reads 0 dBFS.
    norm: f32,
    scratch: Vec<Complex<f32>>,
    acc: Vec<f64>,
    frames: u32,
}

impl StepSpectrum {
    #[must_use]
    pub fn new(n: usize) -> Self {
        let fft = FftPlanner::<f32>::new().plan_fft_forward(n);
        let window: Vec<f32> = (0..n)
            .map(|i| 0.5 - 0.5 * (TAU * i as f32 / n as f32).cos())
            .collect();
        let sum: f32 = window.iter().sum();
        Self {
            fft,
            window,
            norm: sum * sum,
            scratch: vec![Complex::new(0.0, 0.0); n],
            acc: vec![0.0; n],
            frames: 0,
        }
    }

    pub fn reset(&mut self) {
        self.acc.iter_mut().for_each(|a| *a = 0.0);
        self.frames = 0;
    }

    /// Add one frame (`frame.len()` must equal the FFT size).
    pub fn push(&mut self, frame: &[Complex<f32>]) {
        for ((s, x), w) in self.scratch.iter_mut().zip(frame).zip(&self.window) {
            *s = x * *w;
        }
        self.fft.process(&mut self.scratch);
        for (a, s) in self.acc.iter_mut().zip(&self.scratch) {
            *a += f64::from(s.norm_sqr());
        }
        self.frames += 1;
    }

    /// The averaged spectrum, DC-centred (fftshift), dBFS.
    #[must_use]
    pub fn db_shifted(&self) -> Vec<f32> {
        let n = self.acc.len();
        let k = f64::from(self.frames.max(1)) * f64::from(self.norm);
        (0..n)
            .map(|i| {
                let p = self.acc[(i + n / 2) % n] / k;
                (10.0 * p.max(1e-20).log10()) as f32
            })
            .collect()
    }
}

/// Crop a DC-centred spectrum to the step's owned range and interpolate
/// over the DC spur. Returns `(f_lo_hz, bins)`.
#[must_use]
pub fn crop_row(db: &[f32], plan: &SweepPlan, step: &Step) -> (f64, Vec<f32>) {
    let n = db.len() as f64;
    let bin = plan.bin_hz;
    // bin i (shifted) centre = centre + (i - n/2) · bin
    let idx = |f: f64| ((f - step.center_hz) / bin + n / 2.0).round();
    let first = idx(step.own_lo_hz).clamp(0.0, n - 1.0) as usize;
    let last = (idx(step.own_hi_hz) as isize - 1).clamp(first as isize, n as isize - 1) as usize;
    let mut row: Vec<f32> = db[first..=last].to_vec();
    // DC spur: linear interpolation across ±dc_mask_hz of the LO.
    let half = (plan.params.dc_mask_hz / bin).ceil() as isize;
    if half > 0 {
        let c = (n / 2.0) as isize - first as isize;
        let (a, b) = (c - half - 1, c + half + 1);
        if a >= 0 && (b as usize) < row.len() {
            let (va, vb) = (row[a as usize], row[b as usize]);
            for k in (a + 1)..b {
                let t = (k - a) as f32 / (b - a) as f32;
                row[k as usize] = va + (vb - va) * t;
            }
        }
    }
    let f_lo = step.center_hz + (first as f64 - n / 2.0) * bin;
    (f_lo, row)
}

/// Base64 of little-endian `i16` centi-dB; non-finite → [`MASKED`].
#[must_use]
pub fn encode_power(row: &[f32]) -> String {
    let mut bytes = Vec::with_capacity(row.len() * 2);
    for &v in row {
        let q = if v.is_finite() {
            (v * 100.0).round().clamp(-32_767.0, 32_767.0) as i16
        } else {
            MASKED
        };
        bytes.extend_from_slice(&q.to_le_bytes());
    }
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Inverse of [`encode_power`] (tests, MCP summaries).
pub fn decode_power(b64: &str) -> Result<Vec<f32>> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64)?;
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            let q = i16::from_le_bytes(*c);
            if q == MASKED {
                f32::NAN
            } else {
                f32::from(q) / 100.0
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Radio seam + the hop loop
// ---------------------------------------------------------------------------

/// One read off the radio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SweepRead {
    Samples(usize),
    Timeout,
    Overflow,
    Fatal(String),
}

/// What the hop loop needs from a radio.
pub trait SweepRadio: Send {
    /// Sample rate the stream runs at (fixed for the sweep's lifetime —
    /// see the rate-change finding in the plan).
    fn rate_hz(&self) -> f64;
    /// Retune the LO; returns when the driver accepted it.
    fn tune(&mut self, hz: f64) -> Result<()>;
    fn read(&mut self, buf: &mut [Complex<f32>]) -> SweepRead;
}

/// State shared between the hop thread and the block.
#[derive(Debug, Default)]
pub struct SweepShared {
    /// Config generation + params; a bump restarts the pass.
    pub cfg: Mutex<(u64, SweepParams)>,
    pub rows: Mutex<VecDeque<String>>,
    /// Liveness, same meaning as the continuous reader's counters.
    pub samples_pushed: AtomicU64,
    pub last_sample_at_ns: AtomicU64,
    pub hung: AtomicBool,
    pub reader_ended: AtomicBool,
    pub reader_error: Mutex<Option<String>>,
    pub rows_emitted: AtomicU64,
    pub passes: AtomicU64,
    /// Last full pass duration, ms (0 until one completes).
    pub last_pass_ms: AtomicU64,
}

/// Config generations are unique across block instances, not just within
/// one: a rebuilt sweep (a driver-setting change restarts the block) must
/// out-rank rows an earlier instance left in the store, since consumers
/// keep the rows of the *highest* `cfg`. Seeded from the clock, ×1000 so
/// each instance has room for its own live re-aims.
fn next_cfg_base() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let seed = now_ms().saturating_mul(1000);
    let mut cur = NEXT.load(Ordering::Relaxed);
    loop {
        let base = cur.max(seed);
        match NEXT.compare_exchange(cur, base + 1000, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return base,
            Err(actual) => cur = actual,
        }
    }
}

impl SweepShared {
    #[must_use]
    pub fn new(params: SweepParams) -> Self {
        Self {
            cfg: Mutex::new((next_cfg_base(), params)),
            ..Self::default()
        }
    }

    fn push_row(&self, line: String) {
        let mut q = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        q.push_back(line);
        while q.len() > ROW_QUEUE_CAP {
            q.pop_front();
        }
        self.rows_emitted.fetch_add(1, Ordering::Relaxed);
    }
}

/// Outcome of filling a buffer from the radio.
enum Fill {
    Ok,
    Stop,
    Restart,
    Dead,
}

/// Read exactly `buf.len()` samples, tracking liveness. Bails out on
/// stop, a config change, or a fatal read.
fn fill<R: SweepRadio>(
    radio: &mut R,
    buf: &mut [Complex<f32>],
    shared: &SweepShared,
    stop: &AtomicBool,
    gen: u64,
    anchor: Instant,
    hung_stall: Duration,
) -> Fill {
    let mut at = 0;
    let mut last_progress = Instant::now();
    while at < buf.len() {
        if stop.load(Ordering::Relaxed) {
            return Fill::Stop;
        }
        if shared.cfg.lock().map(|c| c.0).unwrap_or(gen) != gen {
            return Fill::Restart;
        }
        match radio.read(&mut buf[at..]) {
            SweepRead::Samples(0) | SweepRead::Timeout => {
                if last_progress.elapsed() > hung_stall {
                    shared.hung.store(true, Ordering::Relaxed);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            SweepRead::Samples(n) => {
                at += n;
                last_progress = Instant::now();
                let now = (anchor.elapsed().as_nanos() as u64).max(1);
                shared.last_sample_at_ns.store(now, Ordering::Relaxed);
                shared.samples_pushed.fetch_add(n as u64, Ordering::Relaxed);
                shared.hung.store(false, Ordering::Relaxed);
            }
            SweepRead::Overflow => {}
            SweepRead::Fatal(msg) => {
                tracing::warn!(%msg, "sweep read error; ending the sweep");
                shared.hung.store(true, Ordering::Relaxed);
                shared.reader_ended.store(true, Ordering::Relaxed);
                if let Ok(mut e) = shared.reader_error.lock() {
                    *e = Some(msg);
                }
                return Fill::Dead;
            }
        }
    }
    Fill::Ok
}

/// The hop loop. Runs until `stop` (or a fatal read); pushes one NDJSON
/// row per step into `shared.rows`.
pub fn run_sweep<R: SweepRadio>(
    radio: &mut R,
    shared: &SweepShared,
    stop: &AtomicBool,
    anchor: Instant,
    hung_stall: Duration,
) {
    let rate = radio.rate_hz();
    let mut pass: u64 = 0;
    'config: while !stop.load(Ordering::Relaxed) {
        let (gen, params) = shared
            .cfg
            .lock()
            .map(|c| c.clone())
            .unwrap_or_else(|e| e.into_inner().clone());
        let plan = match SweepPlan::new(&params, rate) {
            Ok(p) => p,
            Err(e) => {
                // Invalid range: idle until the config changes.
                tracing::warn!(error = %e, "sweep plan rejected; waiting for a valid range");
                while !stop.load(Ordering::Relaxed)
                    && shared.cfg.lock().map(|c| c.0).unwrap_or(gen) == gen
                {
                    std::thread::sleep(Duration::from_millis(50));
                }
                continue;
            }
        };
        let n = plan.fft_size;
        let mut spectrum = StepSpectrum::new(n);
        let mut frame = vec![Complex::new(0.0f32, 0.0); n];
        let mut junk = vec![Complex::new(0.0f32, 0.0); plan.discard_samples.clamp(1, 1 << 20)];
        let steps = plan.steps.len();
        let range = [
            params.sweep_start_hz.min(params.sweep_stop_hz),
            params.sweep_start_hz.max(params.sweep_stop_hz),
        ];
        loop {
            let pass_t0 = Instant::now();
            for (i, step) in plan.steps.iter().enumerate() {
                if let Err(e) = radio.tune(step.center_hz) {
                    tracing::warn!(error = %format!("{e:#}"), hz = step.center_hz, "sweep tune failed");
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                // Drop what's still in flight from the previous LO.
                let mut left = plan.discard_samples;
                while left > 0 {
                    let take = left.min(junk.len());
                    match fill(
                        radio,
                        &mut junk[..take],
                        shared,
                        stop,
                        gen,
                        anchor,
                        hung_stall,
                    ) {
                        Fill::Ok => left -= take,
                        Fill::Stop | Fill::Dead => return,
                        Fill::Restart => continue 'config,
                    }
                }
                spectrum.reset();
                for _ in 0..params.averages.max(1) {
                    match fill(radio, &mut frame, shared, stop, gen, anchor, hung_stall) {
                        Fill::Ok => spectrum.push(&frame),
                        Fill::Stop | Fill::Dead => return,
                        Fill::Restart => continue 'config,
                    }
                }
                let (f_lo, row) = crop_row(&spectrum.db_shifted(), &plan, step);
                let line = serde_json::json!({
                    "cfg": gen,
                    "pass": pass,
                    "step": i,
                    "steps": steps,
                    "f_lo_hz": f_lo,
                    "bin_hz": plan.bin_hz,
                    "n": row.len(),
                    "power": encode_power(&row),
                    "center_hz": step.center_hz,
                    "range": range,
                    "rate_hz": rate,
                    "rbw_hz": plan.bin_hz,
                    "fft_size": n,
                    "averages": params.averages.max(1),
                    "t_ms": crate::sweep::now_ms(),
                })
                .to_string();
                shared.push_row(line);
            }
            pass += 1;
            shared.passes.fetch_add(1, Ordering::Relaxed);
            shared.last_pass_ms.store(
                u64::try_from(pass_t0.elapsed().as_millis()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if shared.cfg.lock().map(|c| c.0).unwrap_or(gen) != gen {
                continue 'config;
            }
        }
    }
}

#[must_use]
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

// ---------------------------------------------------------------------------
// Runner: thread + queue + live params, shared by both blocks
// ---------------------------------------------------------------------------

/// Owns the hop thread and the row queue; the block side drains rows
/// into its `events` port.
pub struct SweepRunner {
    pub shared: Arc<SweepShared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pending: Vec<u8>,
    pub anchor: Instant,
}

impl SweepRunner {
    #[must_use]
    pub fn new(params: SweepParams) -> Self {
        Self {
            shared: Arc::new(SweepShared::new(params)),
            stop: Arc::new(AtomicBool::new(false)),
            thread: None,
            pending: Vec::new(),
            anchor: Instant::now(),
        }
    }

    /// Spawn the hop thread over `radio`.
    pub fn start<R: SweepRadio + 'static>(
        &mut self,
        mut radio: R,
        hung_stall: Duration,
    ) -> Result<()> {
        if self.thread.is_some() {
            return Err(anyhow!("sweep already running"));
        }
        let shared = Arc::clone(&self.shared);
        let stop = Arc::clone(&self.stop);
        let anchor = self.anchor;
        self.thread = Some(
            std::thread::Builder::new()
                .name("sweep".into())
                .spawn(move || run_sweep(&mut radio, &shared, &stop, anchor, hung_stall))?,
        );
        Ok(())
    }

    /// Stop and join. Idempotent.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    /// Apply sweep keys from a live delta; restarts the pass when
    /// anything changed. Returns false if `delta` holds a non-sweep key.
    pub fn apply_live(&self, obj: &serde_json::Map<String, serde_json::Value>) -> Result<bool> {
        if !obj.keys().all(|k| SWEEP_LIVE_KEYS.contains(&k.as_str())) {
            return Ok(false);
        }
        let mut cfg = self
            .shared
            .cfg
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut v = serde_json::to_value(&cfg.1)?;
        if let Some(m) = v.as_object_mut() {
            for (k, val) in obj {
                m.insert(k.clone(), val.clone());
            }
        }
        let next: SweepParams = serde_json::from_value(v)?;
        if next != cfg.1 {
            cfg.0 += 1;
            cfg.1 = next;
        }
        Ok(true)
    }

    /// Move queued rows into the block's `events` port.
    pub fn drain(&mut self, io: &mut BlockIo<'_>) -> Work {
        {
            let mut q = self
                .shared
                .rows
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Keep the pending buffer bounded: only top it up when it has
            // drained (rows are ~10 KB; the port takes a few per tick).
            while self.pending.len() < EVENTS_HINT / 2 {
                let Some(line) = q.pop_front() else { break };
                self.pending.extend_from_slice(line.as_bytes());
                self.pending.push(b'\n');
            }
        }
        let mut w = Work::new();
        drain_events_port(io, &mut self.pending, &mut w);
        w
    }
}

impl Drop for SweepRunner {
    fn drop(&mut self) {
        self.stop();
    }
}

fn sweep_param_specs() -> [ParamSpec; 8] {
    [
        ParamSpec {
            key: "sweep_start_hz",
            label: "Start",
            kind: ParamKind::Range { min: 0.0, max: 6e9, step: 1.0, default: 88e6, unit: "Hz" },
            reconfig_scope: ReconfigureScope::SelfBlock,
            ai_notes: "Lower edge of the swept range, Hz. Live: changing it restarts the pass.",
        },
        ParamSpec {
            key: "sweep_stop_hz",
            label: "Stop",
            kind: ParamKind::Range { min: 0.0, max: 6e9, step: 1.0, default: 108e6, unit: "Hz" },
            reconfig_scope: ReconfigureScope::SelfBlock,
            ai_notes: "Upper edge of the swept range, Hz. Steps are derived (usable bandwidth − overlap), ~7.6 MHz each at 10 MS/s.",
        },
        ParamSpec {
            key: "rbw_hz",
            label: "RBW",
            kind: ParamKind::Range { min: 100.0, max: 100_000.0, step: 1.0, default: 2_500.0, unit: "Hz" },
            reconfig_scope: ReconfigureScope::SelfBlock,
            ai_notes: "Resolution (bin spacing). The FFT size is the nearest power of two to rate/rbw; finer RBW = lower noise floor per bin and longer dwell.",
        },
        ParamSpec {
            key: "averages",
            label: "Averages",
            kind: ParamKind::Range { min: 1.0, max: 512.0, step: 1.0, default: 16.0, unit: "" },
            reconfig_scope: ReconfigureScope::SelfBlock,
            ai_notes: "FFT frames power-averaged per step. More = smoother floor, longer dwell (averages × fft_size / rate per step).",
        },
        ParamSpec {
            key: "discard_ms",
            label: "Retune discard",
            kind: ParamKind::Range { min: 0.0, max: 1_000.0, step: 1.0, default: 0.0, unit: "ms" },
            reconfig_scope: ReconfigureScope::SelfBlock,
            ai_notes: "Samples dropped after each retune (still from the old LO). 0 = auto: 50 ms at ≥6 MS/s, 90 ms below — measured over SoapyRemote, which has no hardware timestamps.",
        },
        ParamSpec {
            key: "usable_fraction",
            label: "Usable fraction",
            kind: ParamKind::Range { min: 0.05, max: 1.0, step: 0.01, default: 0.8, unit: "" },
            reconfig_scope: ReconfigureScope::SelfBlock,
            ai_notes: "Share of the sample rate flat enough to keep (IF filter passband). 0.8 at 10 MS/s on the RSPdx (8 MHz filter).",
        },
        ParamSpec {
            key: "overlap",
            label: "Edge margin",
            kind: ParamKind::Range { min: 0.0, max: 0.9, step: 0.01, default: 0.05, unit: "" },
            reconfig_scope: ReconfigureScope::SelfBlock,
            ai_notes: "Extra trim off each step's usable window, as a fraction of it — keeps filter roll-off out of the stitched trace.",
        },
        ParamSpec {
            key: "dc_mask_hz",
            label: "DC mask",
            kind: ParamKind::Range { min: 0.0, max: 100_000.0, step: 100.0, default: 5_000.0, unit: "Hz" },
            reconfig_scope: ReconfigureScope::SelfBlock,
            ai_notes: "Half-width around each step's LO that is interpolated over, hiding the DC / LO-leakage spur.",
        },
    ]
}

/// Leak a param list once per block type (BlockSpec wants `'static`).
pub(crate) fn leak_params(v: Vec<ParamSpec>) -> &'static [ParamSpec] {
    Box::leak(v.into_boxed_slice())
}

pub(crate) fn sweep_params_with(extra: &[ParamSpec]) -> Vec<ParamSpec> {
    let mut v = sweep_param_specs().to_vec();
    v.extend_from_slice(extra);
    v
}

pub(crate) const PANORAMA_OUTPUTS: &[PortSpec] = &[PortSpec {
    name: "events",
    port_type: PortType::Events,
}];

pub(crate) fn events_hints() -> [usize; MAX_PORTS] {
    let mut h = [0; MAX_PORTS];
    h[0] = EVENTS_HINT;
    h
}

// ---------------------------------------------------------------------------
// FakeBand + SweepSim
// ---------------------------------------------------------------------------

/// One synthetic emitter.
#[derive(Debug, Clone)]
pub struct FakeEmitter {
    pub freq_hz: f64,
    /// Amplitude (linear, full scale = 1).
    pub amp: f32,
    /// FM deviation for broadcast-like blobs (0 = clean carrier).
    pub fm_dev_hz: f64,
    /// Keyed on for `on_s` of every `period_s` (period 0 = always on).
    pub period_s: f64,
    pub on_s: f64,
}

/// A synthetic band: emitters + noise, delivered at the configured rate
/// in (roughly) real time, with `stale_s` of old-LO samples after each
/// tune — the shape the real radio has, minus the radio.
pub struct FakeBand {
    pub rate: f64,
    pub emitters: Vec<FakeEmitter>,
    pub noise_amp: f32,
    pub tune_latency: Duration,
    pub stale_s: f64,
    /// Pace reads to real time (off in unit tests).
    pub realtime: bool,
    lo_hz: f64,
    prev_lo_hz: f64,
    stale_left: usize,
    t: f64,
    phases: Vec<f64>,
    rng: u64,
}

impl FakeBand {
    #[must_use]
    pub fn new(rate: f64, emitters: Vec<FakeEmitter>) -> Self {
        let n = emitters.len();
        Self {
            rate,
            emitters,
            noise_amp: 1e-5,
            tune_latency: Duration::ZERO,
            stale_s: 0.0,
            realtime: false,
            lo_hz: 0.0,
            prev_lo_hz: 0.0,
            stale_left: 0,
            t: 0.0,
            phases: vec![0.0; n],
            rng: 0x9E37_79B9_7F4A_7C15,
        }
    }

    /// A plausible VHF scene for the demo / UI: FM broadcast, airband
    /// bursts, NOAA weather, APRS bursts, a 433 MHz ISM burster.
    #[must_use]
    pub fn demo_scene(rate: f64) -> Self {
        let fm = |f: f64, amp: f32| FakeEmitter {
            freq_hz: f,
            amp,
            fm_dev_hz: 60e3,
            period_s: 0.0,
            on_s: 0.0,
        };
        let cw = |f: f64, amp: f32| FakeEmitter {
            freq_hz: f,
            amp,
            fm_dev_hz: 0.0,
            period_s: 0.0,
            on_s: 0.0,
        };
        let burst = |f: f64, amp: f32, period_s: f64, on_s: f64| FakeEmitter {
            freq_hz: f,
            amp,
            fm_dev_hz: 3e3,
            period_s,
            on_s,
        };
        let mut b = Self::new(
            rate,
            vec![
                fm(88.5e6, 0.02),
                fm(91.3e6, 0.004),
                fm(94.1e6, 0.03),
                fm(98.5e6, 0.08),
                fm(101.1e6, 0.01),
                fm(104.3e6, 0.006),
                fm(106.5e6, 0.05),
                burst(119.1e6, 0.003, 6.0, 2.0),
                burst(121.5e6, 0.001, 20.0, 1.0),
                burst(127.85e6, 0.002, 9.0, 3.0),
                burst(144.39e6, 0.004, 10.0, 0.6),
                cw(162.4e6, 0.006),
                cw(162.55e6, 0.003),
                burst(433.92e6, 0.01, 30.0, 0.4),
                burst(462.5625e6, 0.003, 15.0, 4.0),
            ],
        );
        b.tune_latency = Duration::from_millis(20);
        b.stale_s = 0.035;
        b.realtime = true;
        b
    }

    fn noise(&mut self) -> f32 {
        // xorshift64* → sum of 4 uniforms ≈ gaussian
        let mut acc = 0.0f32;
        for _ in 0..4 {
            self.rng ^= self.rng >> 12;
            self.rng ^= self.rng << 25;
            self.rng ^= self.rng >> 27;
            let u =
                (self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32;
            acc += u - 0.5;
        }
        acc * 1.732
    }
}

impl SweepRadio for FakeBand {
    fn rate_hz(&self) -> f64 {
        self.rate
    }

    fn tune(&mut self, hz: f64) -> Result<()> {
        if !self.tune_latency.is_zero() {
            std::thread::sleep(self.tune_latency);
        }
        self.prev_lo_hz = self.lo_hz;
        self.lo_hz = hz;
        self.stale_left = (self.stale_s * self.rate) as usize;
        Ok(())
    }

    fn read(&mut self, buf: &mut [Complex<f32>]) -> SweepRead {
        let n = buf.len().min(16_384);
        let half = self.rate / 2.0;
        for s in buf.iter_mut().take(n) {
            let lo = if self.stale_left > 0 {
                self.stale_left -= 1;
                self.prev_lo_hz
            } else {
                self.lo_hz
            };
            let (nr, ni) = (self.noise(), self.noise());
            let mut v = Complex::new(nr * self.noise_amp, ni * self.noise_amp);
            for (k, e) in self.emitters.iter().enumerate() {
                let off = e.freq_hz - lo;
                if off.abs() > half {
                    continue;
                }
                if e.period_s > 0.0 && (self.t % e.period_s) > e.on_s {
                    continue;
                }
                let inst = off + e.fm_dev_hz * (std::f64::consts::TAU * 1_000.0 * self.t).sin();
                self.phases[k] = (self.phases[k] + std::f64::consts::TAU * inst / self.rate)
                    % std::f64::consts::TAU;
                let ph = self.phases[k] as f32;
                v += Complex::new(ph.cos(), ph.sin()) * e.amp;
            }
            *s = v;
            self.t += 1.0 / self.rate;
        }
        if self.realtime {
            std::thread::sleep(Duration::from_secs_f64(n as f64 / self.rate));
        }
        SweepRead::Samples(n)
    }
}

/// Params of the simulated sweep: the sweep knobs + the fake rate.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SweepSimParams {
    #[serde(flatten)]
    pub sweep: SweepParams,
    pub sample_rate_hz: f64,
}

impl Default for SweepSimParams {
    fn default() -> Self {
        Self {
            sweep: SweepParams::default(),
            sample_rate_hz: 10e6,
        }
    }
}

/// Stepped sweep over a synthetic band ([`FakeBand::demo_scene`]) — the
/// spectrum analyzer with no radio. What `compose` substitutes for a
/// stepped source when the live source isn't hardware.
pub struct SweepSim {
    params: SweepSimParams,
    runner: SweepRunner,
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for SweepSim {
    fn spec() -> BlockSpec {
        static PARAMS: std::sync::OnceLock<&'static [ParamSpec]> = std::sync::OnceLock::new();
        BlockSpec {
            type_name: "SweepSim",
            placement: Placement::NativeOnly,
            inputs: &[],
            outputs: PANORAMA_OUTPUTS,
            params: PARAMS.get_or_init(|| {
                leak_params(sweep_params_with(&[ParamSpec {
                    key: "sample_rate_hz",
                    label: "Sample rate",
                    kind: ParamKind::Range { min: 1e5, max: 2e7, step: 1.0, default: 10e6, unit: "Hz" },
                    reconfig_scope: ReconfigureScope::SourceRestart,
                    ai_notes: "Rate of the simulated radio.",
                }]))
            }),
            ai_notes: "Simulated stepped sweep (spectrum analyzer) over a synthetic VHF scene — FM broadcast, airband/APRS/ISM bursts, NOAA carriers. Same rows as SweepSource on `events` → ui:panorama; used when no hardware radio is selected, and by tests.",
        }
    }

    fn init(&mut self, _ctx: &mut InitCtx<'_>) -> Result<()> {
        self.runner.start(
            FakeBand::demo_scene(self.params.sample_rate_hz),
            Duration::from_secs(3),
        )
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        Ok(self.runner.drain(io))
    }

    fn output_capacity_hints(&self) -> [usize; MAX_PORTS] {
        events_hints()
    }

    fn stop(&mut self) -> Result<()> {
        self.runner.stop();
        Ok(())
    }

    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let Some(obj) = delta.as_object() else {
            return Ok(false);
        };
        self.runner.apply_live(obj)
    }
}

impl BlockFactory for SweepSim {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: SweepSimParams = crate::block::deserialize_params(params)?;
        SweepPlan::new(&p.sweep, p.sample_rate_hz)?;
        Ok(Box::new(Self {
            runner: SweepRunner::new(p.sweep.clone()),
            params: p,
        }))
    }
}

impl SweepSim {
    /// Pass statistics for diagnostics.
    #[must_use]
    pub fn shared(&self) -> &Arc<SweepShared> {
        &self.runner.shared
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(lo: f64, hi: f64) -> SweepParams {
        SweepParams {
            sweep_start_hz: lo,
            sweep_stop_hz: hi,
            rbw_hz: 10_000.0,
            averages: 4,
            discard_ms: 1.0,
            ..SweepParams::default()
        }
    }

    #[test]
    fn plan_tiles_the_range_exactly() {
        let p = SweepPlan::new(&params(88e6, 108e6), 10e6).unwrap();
        assert_eq!(p.steps.first().unwrap().own_lo_hz, 88e6);
        assert_eq!(p.steps.last().unwrap().own_hi_hz, 108e6);
        for w in p.steps.windows(2) {
            assert!(
                (w[0].own_hi_hz - w[1].own_lo_hz).abs() < 1e-6,
                "gap/overlap {w:?}"
            );
        }
        // Each owned slice lies inside the flat part of its step.
        for s in &p.steps {
            assert!(
                s.own_lo_hz >= s.center_hz - 4e6 - 1.0 && s.own_hi_hz <= s.center_hz + 4e6 + 1.0
            );
        }
        assert_eq!(p.steps.len(), 3, "20 MHz / 7.6 MHz → 3 steps");
    }

    #[test]
    fn single_step_sweep_centres_its_lo() {
        let p = SweepPlan::new(&params(100e6, 101e6), 10e6).unwrap();
        assert_eq!(p.steps.len(), 1);
        assert!((p.steps[0].center_hz - 100.5e6).abs() < 1.0);
    }

    #[test]
    fn plan_rejects_empty_range() {
        assert!(SweepPlan::new(&params(100e6, 100e6), 10e6).is_err());
    }

    #[test]
    fn power_round_trips_and_masks() {
        let row = [-100.0f32, -12.34, 0.0, f32::NAN];
        let back = decode_power(&encode_power(&row)).unwrap();
        assert!((back[0] + 100.0).abs() < 0.01);
        assert!((back[1] + 12.34).abs() < 0.01);
        assert!(back[3].is_nan());
    }

    /// Run one pass over `band`; returns the decoded rows.
    fn one_pass(band: FakeBand, p: SweepParams) -> Vec<serde_json::Value> {
        let shared = Arc::new(SweepShared::new(p));
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, st2) = (Arc::clone(&shared), Arc::clone(&stop));
        let t = std::thread::spawn(move || {
            let mut band = band;
            run_sweep(&mut band, &s2, &st2, Instant::now(), Duration::from_secs(3));
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        while shared.passes.load(Ordering::Relaxed) < 1 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        let rows: Vec<serde_json::Value> = shared
            .rows
            .lock()
            .unwrap()
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .filter(|r: &serde_json::Value| r["pass"] == 0)
            .collect();
        rows
    }

    /// Stitch rows into (freq, dB) points.
    fn stitch(rows: &[serde_json::Value]) -> Vec<(f64, f32)> {
        let mut pts = Vec::new();
        for r in rows {
            let f_lo = r["f_lo_hz"].as_f64().unwrap();
            let bin = r["bin_hz"].as_f64().unwrap();
            for (k, v) in decode_power(r["power"].as_str().unwrap())
                .unwrap()
                .into_iter()
                .enumerate()
            {
                pts.push((f_lo + k as f64 * bin, v));
            }
        }
        pts
    }

    fn peak_near(pts: &[(f64, f32)], f: f64, tol: f64) -> f32 {
        pts.iter()
            .filter(|(x, _)| (x - f).abs() <= tol)
            .map(|(_, v)| *v)
            .fold(f32::NEG_INFINITY, f32::max)
    }

    #[test]
    fn carriers_land_at_their_frequency_across_steps() {
        let cw = |f: f64, amp: f32| FakeEmitter {
            freq_hz: f,
            amp,
            fm_dev_hz: 0.0,
            period_s: 0.0,
            on_s: 0.0,
        };
        // One carrier in each of the three steps, one near a step edge.
        let band = FakeBand::new(
            10e6,
            vec![cw(90.0e6, 0.1), cw(98.123e6, 0.01), cw(107.0e6, 0.05)],
        );
        let rows = one_pass(band, params(88e6, 108e6));
        assert_eq!(rows.len(), 3);
        let pts = stitch(&rows);
        // Contiguous coverage of the range.
        assert!(pts.first().unwrap().0 - 88e6 < 20e3);
        assert!(108e6 - pts.last().unwrap().0 < 20e3);
        let bin = rows[0]["bin_hz"].as_f64().unwrap();
        for (f, amp) in [(90.0e6, 0.1f32), (98.123e6, 0.01), (107.0e6, 0.05)] {
            let p = peak_near(&pts, f, bin);
            let want = 20.0 * amp.log10();
            assert!(
                (p - want).abs() < 2.0,
                "{f}: peak {p:.1} dBFS, want ≈{want:.1}"
            );
        }
        // Floor well below the carriers away from them.
        let floor = peak_near(&pts, 103e6, 1e6);
        assert!(floor < -60.0, "floor {floor:.1}");
    }

    #[test]
    fn retune_discard_keeps_old_lo_samples_out_of_the_row() {
        let cw = |f: f64, amp: f32| FakeEmitter {
            freq_hz: f,
            amp,
            fm_dev_hz: 0.0,
            period_s: 0.0,
            on_s: 0.0,
        };
        // A strong carrier only visible from step 0's LO, offset so that
        // stale step-0 samples would put it at +2 MHz in step 1's frame.
        let make = || {
            let mut b = FakeBand::new(10e6, vec![cw(92.0e6, 0.5)]);
            b.stale_s = 0.020; // 20 ms of old-LO samples after each tune
            b
        };
        let mut p = params(88e6, 103.2e6);
        p.averages = 2;
        p.discard_ms = 25.0;
        let clean = stitch(&one_pass(make(), p.clone()));
        p.discard_ms = 0.1;
        let dirty = stitch(&one_pass(make(), p));
        // Step 0 (LO 91.8 MHz) sees the 92 MHz carrier; step 1's slice
        // (95.6–103.2 MHz) is out of its reach — unless stale step-0
        // samples leak into step 1's frames, which puts it there.
        let s1 = |pts: &[(f64, f32)]| {
            pts.iter()
                .filter(|(f, _)| *f > 95.9e6 && *f < 103.1e6)
                .map(|(_, v)| *v)
                .fold(f32::NEG_INFINITY, f32::max)
        };
        assert!(
            s1(&clean) < -50.0,
            "with discard, step 1 is clean: {:.1}",
            s1(&clean)
        );
        assert!(
            s1(&dirty) > -40.0,
            "without discard, the old LO leaks in: {:.1}",
            s1(&dirty)
        );
    }

    #[test]
    fn cfg_outranks_earlier_instances() {
        let a = SweepShared::new(SweepParams::default());
        let b = SweepShared::new(SweepParams::default());
        let ca = a.cfg.lock().unwrap().0;
        let cb = b.cfg.lock().unwrap().0;
        assert!(cb >= ca + 1000, "a rebuilt sweep's rows win: {ca} → {cb}");
        assert!(ca > 1_000_000_000_000_000, "clock-seeded: {ca}");
    }

    #[test]
    fn dc_spur_is_interpolated_over() {
        let p = SweepPlan::new(&params(100e6, 101e6), 10e6).unwrap();
        let n = p.fft_size;
        let mut db = vec![-100.0f32; n];
        db[n / 2] = 0.0; // spur exactly at the LO
        let (_, row) = crop_row(&db, &p, &p.steps[0]);
        assert!(row.iter().all(|v| *v < -99.0), "spur removed");
    }

    #[test]
    fn config_change_restarts_the_pass() {
        let shared = Arc::new(SweepShared::new(params(88e6, 108e6)));
        let runner_shared = Arc::clone(&shared);
        let stop = Arc::new(AtomicBool::new(false));
        let st2 = Arc::clone(&stop);
        let t = std::thread::spawn(move || {
            let mut band = FakeBand::new(10e6, vec![]);
            run_sweep(
                &mut band,
                &runner_shared,
                &st2,
                Instant::now(),
                Duration::from_secs(3),
            );
        });
        std::thread::sleep(Duration::from_millis(50));
        let new_cfg = {
            let mut c = shared.cfg.lock().unwrap();
            c.0 += 1;
            c.1 = params(400e6, 401e6);
            c.0
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = false;
        while Instant::now() < deadline && !seen {
            seen = shared
                .rows
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains(&format!("\"cfg\":{new_cfg}")));
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert!(seen, "rows for the new config appear");
        let last: serde_json::Value =
            serde_json::from_str(shared.rows.lock().unwrap().back().unwrap()).unwrap();
        assert_eq!(last["range"][0], 400e6);
    }

    #[test]
    fn sim_block_emits_ndjson_rows() {
        use crate::block::{OutBuf, OutputPort, PortMeta};
        let mut b = SweepSim::construct(&serde_json::json!({
            "sweep_start_hz": 98e6, "sweep_stop_hz": 99e6, "rbw_hz": 10000.0, "averages": 2
        }))
        .unwrap();
        // Speed: init spawns the realtime demo scene; one 1-MHz step at
        // 10 MS/s is ~80 ms.
        let mut ctx = InitCtx {
            input_meta: &[],
            output_meta: &[],
            frames_hint: 1024,
        };
        b.init(&mut ctx).unwrap();
        let mut buf = vec![0u8; EVENTS_HINT];
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.is_empty() && Instant::now() < deadline {
            let mut outs = [OutputPort {
                name: "events",
                meta: PortMeta::default(),
                buf: OutBuf::Events(&mut buf),
            }];
            let mut io = BlockIo {
                inputs: &mut [],
                outputs: &mut outs,
            };
            let w = b.process(&mut io).unwrap();
            got.extend_from_slice(&buf[..w.produced[0]]);
            std::thread::sleep(Duration::from_millis(20));
        }
        b.stop().unwrap();
        let text = String::from_utf8(got).unwrap();
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert_eq!(first["steps"], 1);
        let pts = stitch(&[first]);
        assert!(
            peak_near(&pts, 98.5e6, 100e3) > -40.0,
            "the 98.5 MHz station shows"
        );
    }
}
