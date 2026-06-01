//! `SignalList` — strongest-signal detector on a wideband FFT byte stream.
//!
//! Taps the same `FftU8` bytes the waterfall renders (the output of
//! [`LogMagU8`](crate::log_mag_u8)) and continuously reports the strongest
//! signals across the whole span as a ranked watchlist. One `FftU8` input,
//! one `Events` output. Node-side only — it's a cheap per-frame scan that
//! feeds the browser's "Strongest Signals" panel and the `signals` MCP
//! verb, neither of which needs the byte stream itself.
//!
//! ### Algorithm (per emitted frame)
//!
//! 1. **Noise floor** — `p10` of the row's bytes (via
//!    [`compute_spectrum_stats`](crate::render::compute_spectrum_stats)),
//!    the level cleared by 90% of bins. Robust against a band packed with
//!    carriers because the gaps between them still pull the 10th percentile
//!    down to the true floor.
//! 2. **Threshold** — `floor + threshold_db`, converted to byte units
//!    (the server quantises a fixed [−160, 0] dBFS window into 0..255, so
//!    one byte ≈ 0.627 dB).
//! 3. **Peak grouping** — contiguous runs of above-threshold bins collapse
//!    into one candidate; its peak bin sets the power, the run width sets
//!    the bandwidth, and the bin index maps to an absolute RF frequency via
//!    the input port's `center_freq_hz` + `sample_rate_hz` (the FFT block
//!    fft-shifts, so DC sits at bin `N/2`).
//! 4. **M-of-N persistence** — candidates are matched frame-to-frame by an
//!    absolute-frequency bucket; a track is only *reported* once it has
//!    been seen in at least `persist_hits` frames, and is evicted after
//!    `persist_window` consecutive misses. Debounces noise spikes without
//!    holding a signal on screen after it drops.
//! 5. **Top-K** — reported tracks are sorted by power and the strongest
//!    `top_k` are emitted as one JSON object on the `events` port, plus a
//!    `decoder::signals` trace line so `recent_decodes` can read them.
//!
//! Emission is throttled to `emit_interval_ms` (default 250 ms = 4 Hz) so
//! the panel updates smoothly without flooding the WS — detection still
//! runs on every frame so persistence sees the full frame history.

#[cfg(not(target_arch = "wasm32"))]
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutBuf, ParamKind, ParamSpec,
    Placement, PortSpec, PortType, ReconfigureScope, Work, MAX_PORTS,
};
use crate::log_mag_u8::{SERVER_CEIL_DBFS, SERVER_FLOOR_DBFS};
use crate::render::compute_spectrum_stats;

/// dB represented by one byte step on the server's fixed quantisation
/// window. `(0 − −160) / 255 ≈ 0.627 dB`.
const DB_PER_BYTE: f32 = (SERVER_CEIL_DBFS - SERVER_FLOOR_DBFS) / 255.0;

/// Convert a quantised FFT byte back to dBFS.
#[inline]
fn byte_to_dbfs(b: u8) -> f32 {
    f32::from(b) * DB_PER_BYTE + SERVER_FLOOR_DBFS
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default)]
pub struct SignalListParams {
    /// FFT bin count per frame. Must match the upstream `LogMagU8` / `Fft`
    /// `size` — that's the length of one `FftU8` frame on the wire.
    pub size: usize,
    /// How far above the estimated noise floor a bin must sit to count as
    /// signal (dB). Higher = only the loudest carriers; lower = more
    /// marginal signals (and more noise to debounce).
    pub threshold_db: f32,
    /// Drop candidate groups narrower than this (Hz). `0` keeps every
    /// above-threshold run, including single-bin spikes (persistence then
    /// does the debouncing). Raise it to ignore CW-width blips on a band
    /// where you only care about wide carriers.
    pub min_bw_hz: f32,
    /// Maximum signals reported per emission, strongest first.
    pub top_k: usize,
    /// Persistence window: a track is evicted after this many consecutive
    /// frames with no matching detection.
    pub persist_window: u32,
    /// A track is only reported once it has been detected in at least this
    /// many frames (M-of-N, the M).
    pub persist_hits: u32,
    /// Two detections within this many Hz of each other are treated as the
    /// same tracked signal across frames. Keeps a wide carrier's wandering
    /// peak bound to one track.
    pub track_bucket_hz: f32,
    /// Emission throttle (ms). Detection runs every frame; the ranked list
    /// is published at most this often. 250 ms = 4 Hz.
    pub emit_interval_ms: f32,
}

impl Default for SignalListParams {
    fn default() -> Self {
        Self {
            size: 16_384,
            threshold_db: 10.0,
            min_bw_hz: 0.0,
            top_k: 16,
            persist_window: 5,
            persist_hits: 3,
            track_bucket_hz: 2_500.0,
            emit_interval_ms: 250.0,
        }
    }
}

/// A raw above-threshold group found in one frame, before persistence.
#[derive(Debug, Clone, Copy)]
struct Candidate {
    freq_hz: f64,
    power_db: f32,
    bw_hz: f64,
    snr_db: f32,
}

/// A signal tracked across frames for M-of-N persistence.
#[derive(Debug, Clone)]
struct Track {
    id: u64,
    freq_hz: f64,
    power_db: f32,
    bw_hz: f64,
    snr_db: f32,
    /// Frames detected within the current window (saturates at
    /// `persist_window`); a track is reported once this reaches
    /// `persist_hits`.
    hits: u32,
    /// Consecutive frames with no match; evicted past `persist_window`.
    misses: u32,
    first_seen_frame: u64,
    last_seen_frame: u64,
}

pub struct SignalList {
    params: SignalListParams,
    sample_rate_hz: f64,
    center_freq_hz: f64,
    tracks: Vec<Track>,
    next_id: u64,
    frame: u64,
    /// JSON bytes waiting to drain to the `events` output.
    pending: Vec<u8>,
    #[cfg(not(target_arch = "wasm32"))]
    last_emit: Option<Instant>,
}

impl SignalList {
    pub fn new(params: SignalListParams) -> Result<Self> {
        if params.size == 0 {
            bail!("signal_list size must be > 0");
        }
        if !(params.emit_interval_ms.is_finite() && params.emit_interval_ms >= 0.0) {
            bail!(
                "signal_list emit_interval_ms must be >= 0 (got {})",
                params.emit_interval_ms
            );
        }
        if params.persist_hits > params.persist_window {
            bail!(
                "signal_list persist_hits ({}) must be <= persist_window ({})",
                params.persist_hits,
                params.persist_window
            );
        }
        Ok(Self {
            params,
            sample_rate_hz: 0.0,
            center_freq_hz: 0.0,
            tracks: Vec::new(),
            next_id: 1,
            frame: 0,
            pending: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            last_emit: None,
        })
    }

    /// Map an fft-shifted bin index to its absolute RF frequency. The FFT
    /// block puts DC at `size/2`, so bin `i` is offset `(i − N/2)/N` of the
    /// sample rate from the tuned centre.
    #[inline]
    fn bin_to_hz(&self, bin: usize) -> f64 {
        let n = self.params.size as f64;
        #[allow(clippy::cast_precision_loss)]
        let offset = (bin as f64 - n / 2.0) / n * self.sample_rate_hz;
        self.center_freq_hz + offset
    }

    /// Scan one frame into raw candidate groups.
    fn detect(&self, row: &[u8]) -> Vec<Candidate> {
        let n = self.params.size.min(row.len());
        if n == 0 {
            return Vec::new();
        }
        let floor_byte = compute_spectrum_stats(&row[..n]).p10;
        let floor_db = byte_to_dbfs(floor_byte);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let thresh_byte = (f32::from(floor_byte) + self.params.threshold_db / DB_PER_BYTE)
            .clamp(0.0, 255.0) as u8;
        let bin_hz = self.sample_rate_hz / self.params.size as f64;

        let mut out = Vec::new();
        let mut i = 0;
        while i < n {
            if row[i] < thresh_byte {
                i += 1;
                continue;
            }
            // Walk the contiguous above-threshold run; remember its peak.
            let start = i;
            let mut peak = row[i];
            let mut peak_idx = i;
            while i < n && row[i] >= thresh_byte {
                if row[i] > peak {
                    peak = row[i];
                    peak_idx = i;
                }
                i += 1;
            }
            let width_bins = i - start;
            let bw_hz = width_bins as f64 * bin_hz;
            if bw_hz < f64::from(self.params.min_bw_hz) {
                continue;
            }
            out.push(Candidate {
                freq_hz: self.bin_to_hz(peak_idx),
                power_db: byte_to_dbfs(peak),
                bw_hz,
                snr_db: byte_to_dbfs(peak) - floor_db,
            });
        }
        out
    }

    /// Fold this frame's candidates into the persistent track set.
    fn update_tracks(&mut self, cands: &[Candidate]) {
        let bucket = f64::from(self.params.track_bucket_hz).max(1.0);
        let mut matched = vec![false; self.tracks.len()];

        for c in cands {
            // Nearest existing track within the frequency bucket.
            let mut best: Option<(usize, f64)> = None;
            for (idx, t) in self.tracks.iter().enumerate() {
                if matched[idx] {
                    continue;
                }
                let d = (t.freq_hz - c.freq_hz).abs();
                if d <= bucket && best.is_none_or(|(_, bd)| d < bd) {
                    best = Some((idx, d));
                }
            }
            if let Some((idx, _)) = best {
                matched[idx] = true;
                let t = &mut self.tracks[idx];
                // Light EMA on frequency keeps a wide carrier's reported
                // centre from jittering bin-to-bin; power/bw/snr take the
                // fresh reading.
                t.freq_hz = 0.7 * t.freq_hz + 0.3 * c.freq_hz;
                t.power_db = c.power_db;
                t.bw_hz = c.bw_hz;
                t.snr_db = c.snr_db;
                t.hits = (t.hits + 1).min(self.params.persist_window);
                t.misses = 0;
                t.last_seen_frame = self.frame;
            } else {
                let id = self.next_id;
                self.next_id += 1;
                self.tracks.push(Track {
                    id,
                    freq_hz: c.freq_hz,
                    power_db: c.power_db,
                    bw_hz: c.bw_hz,
                    snr_db: c.snr_db,
                    hits: 1,
                    misses: 0,
                    first_seen_frame: self.frame,
                    last_seen_frame: self.frame,
                });
                // Keep `matched` aligned with the grown track set — the new
                // track counts as matched this frame, and later candidates'
                // nearest-search indexes `matched` by track position.
                matched.push(true);
            }
        }

        // Age + evict unmatched tracks.
        let window = self.params.persist_window;
        for (idx, t) in self.tracks.iter_mut().enumerate() {
            if !matched[idx] {
                t.misses += 1;
            }
        }
        self.tracks.retain(|t| t.misses <= window);
    }

    /// Build the JSON payload for the currently-reported tracks (those past
    /// the M-of-N gate), strongest first, capped at `top_k`.
    fn build_payload(&self) -> serde_json::Value {
        let mut reported: Vec<&Track> = self
            .tracks
            .iter()
            .filter(|t| t.hits >= self.params.persist_hits)
            .collect();
        reported.sort_by(|a, b| {
            b.power_db
                .partial_cmp(&a.power_db)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        reported.truncate(self.params.top_k);

        let signals: Vec<serde_json::Value> = reported
            .iter()
            .map(|t| {
                serde_json::json!({
                    "id": t.id,
                    "freq_hz": t.freq_hz,
                    "power_db": round2(t.power_db),
                    "bw_hz": t.bw_hz.round(),
                    "snr_db": round2(t.snr_db),
                    "first_seen_frame": t.first_seen_frame,
                    "last_seen_frame": t.last_seen_frame,
                })
            })
            .collect();

        serde_json::json!({
            "signals": signals,
            "center_freq_hz": self.center_freq_hz,
            "span_hz": self.sample_rate_hz,
            "frame": self.frame,
        })
    }

    /// True once `emit_interval_ms` has elapsed since the last emission
    /// (always true on the first call). On wasm there's no monotonic clock
    /// and the block never runs there in production, so it emits every
    /// frame — keeps the type compiling for the shared blocks crate.
    fn emit_due(&mut self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let interval = Duration::from_secs_f32(self.params.emit_interval_ms * 1e-3);
            let now = Instant::now();
            match self.last_emit {
                Some(prev) if now.duration_since(prev) < interval => false,
                _ => {
                    self.last_emit = Some(now);
                    true
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            true
        }
    }
}

/// Two-decimal rounding for the dB fields — keeps the JSON compact without
/// pulling float formatting into the cold emission path's structure.
fn round2(v: f32) -> f64 {
    f64::from((v * 100.0).round()) / 100.0
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for SignalList {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "SignalList",
            // Node-side: a per-frame scan feeding the UI/MCP watchlist, not
            // the byte stream itself — no reason to ship it to the browser.
            placement: Placement::NativeOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::FftU8,
            }],
            outputs: &[PortSpec {
                name: "events",
                port_type: PortType::Events,
            }],
            params: &[
                ParamSpec {
                    key: "size",
                    label: "FFT size",
                    kind: ParamKind::EnumNumeric {
                        values: &[
                            1024.0, 2048.0, 4096.0, 8192.0, 16384.0, 32768.0, 65536.0, 131072.0,
                        ],
                        default: 16384.0,
                        unit: "bins",
                    },
                    reconfig_scope: ReconfigureScope::Downstream,
                    ai_notes: "Must match the upstream FFT / LogMagU8 `size` — it's the length of one FftU8 frame.",
                },
                ParamSpec {
                    key: "threshold_db",
                    label: "Detect threshold",
                    kind: ParamKind::Range {
                        min: 3.0,
                        max: 60.0,
                        step: 1.0,
                        default: 10.0,
                        unit: "dB",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "dB above the estimated noise floor a bin must clear to count as signal. Raise to list only strong carriers, lower to catch marginal ones.",
                },
                ParamSpec {
                    key: "min_bw_hz",
                    label: "Min bandwidth",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 50_000.0,
                        step: 100.0,
                        default: 0.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Ignore above-threshold groups narrower than this. 0 keeps everything (persistence debounces spikes).",
                },
                ParamSpec {
                    key: "top_k",
                    label: "Max signals",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 64.0,
                        step: 1.0,
                        default: 16.0,
                        unit: "",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "How many of the strongest signals to report per update.",
                },
                ParamSpec {
                    key: "persist_window",
                    label: "Persist window",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 60.0,
                        step: 1.0,
                        default: 5.0,
                        unit: "frames",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Frames of consecutive misses before a signal drops off the list.",
                },
                ParamSpec {
                    key: "persist_hits",
                    label: "Persist hits",
                    kind: ParamKind::Range {
                        min: 1.0,
                        max: 60.0,
                        step: 1.0,
                        default: 3.0,
                        unit: "frames",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Frames a signal must be detected in before it's reported (M-of-N debounce). Must be <= persist_window.",
                },
                ParamSpec {
                    key: "track_bucket_hz",
                    label: "Track bucket",
                    kind: ParamKind::Range {
                        min: 100.0,
                        max: 100_000.0,
                        step: 100.0,
                        default: 2_500.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Detections within this distance across frames are the same tracked signal. Set near a typical signal's width.",
                },
                ParamSpec {
                    key: "emit_interval_ms",
                    label: "Emit interval",
                    kind: ParamKind::Range {
                        min: 100.0,
                        max: 2_000.0,
                        step: 10.0,
                        default: 250.0,
                        unit: "ms",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "How often to publish the ranked list. 250 ms = 4 Hz. Detection still runs every frame.",
                },
            ],
            ai_notes: "Strongest-signal detector on the wideband FFT byte stream (taps LogMagU8.out). Emits a ranked watchlist of {id, freq_hz, power_db, bw_hz, snr_db} as JSON events, driving the UI's 'Strongest Signals' panel and the `signals` MCP verb; click/tune a row to retune there.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        self.sample_rate_hz = ctx.input_rate("in").unwrap_or(0.0);
        self.center_freq_hz = ctx
            .input_meta
            .iter()
            .find(|(n, _)| *n == "in")
            .map(|(_, m)| m.center_freq_hz)
            .unwrap_or(0.0);
        Ok(())
    }

    fn update_rates(&mut self, ctx: &InitCtx<'_>) -> Result<()> {
        // Re-read on retune so reported frequencies track the new centre.
        self.sample_rate_hz = ctx.input_rate("in").unwrap_or(self.sample_rate_hz);
        self.center_freq_hz = ctx
            .input_meta
            .iter()
            .find(|(n, _)| *n == "in")
            .map(|(_, m)| m.center_freq_hz)
            .unwrap_or(self.center_freq_hz);
        // A retune invalidates the old frequency buckets — clear tracks so
        // stale signals from the previous centre don't linger.
        self.tracks.clear();
        Ok(())
    }

    fn forecast(&self, _noutput_items: usize) -> Option<[usize; MAX_PORTS]> {
        // Operate on whole FFT frames: don't run until a full `size`-byte
        // frame is available (upstream LogMagU8 emits exactly `size` at a
        // time).
        let mut f = [0; MAX_PORTS];
        f[0] = self.params.size;
        Some(f)
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let n = self.params.size;
        let row = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_fft_u8);
        let Some(row) = row else {
            return Ok(Work::new());
        };
        if row.len() < n {
            return Ok(Work::new());
        }

        self.frame += 1;
        let cands = self.detect(&row[..n]);
        self.update_tracks(&cands);

        // Throttled emission: detection ran above on this frame, but the
        // ranked list is only published every `emit_interval_ms`.
        if self.emit_due() {
            let payload = self.build_payload();
            // Compact single-line JSON object on the events port.
            let mut line = serde_json::to_string(&payload).unwrap_or_default();
            line.push('\n');
            self.pending.extend_from_slice(line.as_bytes());
            // Decoder trace so `recent_decodes`/log readers can see the
            // watchlist without a WS subscription.
            let count = payload
                .get("signals")
                .and_then(|s| s.as_array())
                .map_or(0, Vec::len);
            tracing::info!(target: "decoder::signals", "{count} signals: {payload}");
        }

        // Drain pending JSON to the events output.
        let mut produced_events = 0;
        for port in io.outputs.iter_mut() {
            if port.name == "events" {
                if let OutBuf::Events(dst) = &mut port.buf {
                    let take = self.pending.len().min(dst.len());
                    if take > 0 {
                        dst[..take].copy_from_slice(&self.pending[..take]);
                        self.pending.drain(..take);
                        produced_events = take;
                    }
                }
            }
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = produced_events;
        Ok(w)
    }
}

impl BlockFactory for SignalList {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: SignalListParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(SignalList::new(p)?))
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
mod tests {
    use super::{byte_to_dbfs, SignalList, SignalListParams};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, PortMeta};
    use crate::log_mag_u8::{SERVER_CEIL_DBFS, SERVER_FLOOR_DBFS};

    /// dBFS → the byte LogMagU8 would emit for it.
    fn dbfs_to_byte(db: f32) -> u8 {
        let scale = 255.0 / (SERVER_CEIL_DBFS - SERVER_FLOOR_DBFS);
        ((db - SERVER_FLOOR_DBFS) * scale).clamp(0.0, 255.0) as u8
    }

    /// Build a flat-floor row with Gaussian-ish bumps at the given bins.
    /// Each bump is (center_bin, half_width_bins, peak_dbfs).
    fn make_row(n: usize, floor_db: f32, bumps: &[(usize, usize, f32)]) -> Vec<u8> {
        let mut row = vec![dbfs_to_byte(floor_db); n];
        for &(center, half, peak) in bumps {
            let lo = center.saturating_sub(half);
            let hi = (center + half).min(n - 1);
            for (offset, cell) in row[lo..=hi].iter_mut().enumerate() {
                let b = lo + offset;
                // simple triangular profile so the peak is at center
                let dist = (b as i64 - center as i64).unsigned_abs() as usize;
                let frac = 1.0 - (dist as f32 / (half.max(1) as f32 + 1.0));
                let db = floor_db + (peak - floor_db) * frac;
                let byte = dbfs_to_byte(db);
                if byte > *cell {
                    *cell = byte;
                }
            }
        }
        row
    }

    fn block(n: usize, params: SignalListParams) -> SignalList {
        let mut b = SignalList::new(SignalListParams { size: n, ..params }).unwrap();
        // Pretend init negotiated a 2.4 MHz span centred at 100 MHz.
        b.sample_rate_hz = 2_400_000.0;
        b.center_freq_hz = 100_000_000.0;
        b
    }

    /// Push one frame; return the parsed emission JSON (or None if throttled
    /// — but with emit_interval_ms=0 every frame emits).
    fn push(b: &mut SignalList, row: &[u8]) -> Option<serde_json::Value> {
        let mut events = vec![0u8; 64 * 1024];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::FftU8(row),
        }];
        let mut outputs = [OutputPort {
            name: "events",
            meta: PortMeta::default(),
            buf: OutBuf::Events(&mut events),
        }];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = b.process(&mut io).unwrap();
        let len = w.produced[0];
        if len == 0 {
            return None;
        }
        let text = std::str::from_utf8(&events[..len]).unwrap();
        let last = text.lines().last().unwrap();
        Some(serde_json::from_str(last).unwrap())
    }

    #[test]
    fn byte_roundtrips_to_dbfs() {
        assert!((byte_to_dbfs(0) - SERVER_FLOOR_DBFS).abs() < 0.01);
        assert!((byte_to_dbfs(255) - SERVER_CEIL_DBFS).abs() < 1.0);
    }

    #[test]
    fn constructor_rejects_hits_gt_window() {
        assert!(SignalList::new(SignalListParams {
            persist_window: 2,
            persist_hits: 3,
            ..Default::default()
        })
        .is_err());
    }

    #[test]
    fn single_strong_peak_is_detected_after_persistence() {
        let n = 4096;
        let mut b = block(
            n,
            SignalListParams {
                persist_window: 5,
                persist_hits: 3,
                emit_interval_ms: 0.0,
                threshold_db: 10.0,
                ..Default::default()
            },
        );
        // Strong bump 40 dB over a −120 dBFS floor, well right of centre.
        let row = make_row(n, -120.0, &[(3000, 20, -80.0)]);

        // First two frames: track exists but below persist_hits → empty.
        let v1 = push(&mut b, &row).unwrap();
        assert_eq!(v1["signals"].as_array().unwrap().len(), 0);
        let v2 = push(&mut b, &row).unwrap();
        assert_eq!(v2["signals"].as_array().unwrap().len(), 0);
        // Third frame: persist_hits=3 reached → reported.
        let v3 = push(&mut b, &row).unwrap();
        let sigs = v3["signals"].as_array().unwrap();
        assert_eq!(sigs.len(), 1, "one signal expected once persisted");

        // Frequency maps near bin 3000: offset (3000-2048)/4096 * 2.4e6.
        let expected = 100_000_000.0 + (3000.0 - 2048.0) / 4096.0 * 2_400_000.0;
        let got = sigs[0]["freq_hz"].as_f64().unwrap();
        assert!(
            (got - expected).abs() < 5_000.0,
            "freq {got} should be ~{expected}"
        );
        // SNR should be roughly the 40 dB we put in.
        let snr = sigs[0]["snr_db"].as_f64().unwrap();
        assert!(snr > 30.0, "snr {snr} should reflect the ~40 dB bump");
    }

    #[test]
    fn pure_noise_floor_reports_nothing() {
        let n = 2048;
        let mut b = block(
            n,
            SignalListParams {
                emit_interval_ms: 0.0,
                persist_hits: 1,
                ..Default::default()
            },
        );
        let row = make_row(n, -118.0, &[]);
        for _ in 0..5 {
            let v = push(&mut b, &row).unwrap();
            assert_eq!(
                v["signals"].as_array().unwrap().len(),
                0,
                "flat floor must yield no signals"
            );
        }
    }

    #[test]
    fn two_peaks_ranked_by_power_strongest_first() {
        let n = 8192;
        let mut b = block(
            n,
            SignalListParams {
                persist_hits: 1,
                emit_interval_ms: 0.0,
                threshold_db: 8.0,
                ..Default::default()
            },
        );
        // Weak peak left of centre, strong peak right of centre.
        let row = make_row(n, -120.0, &[(2000, 15, -90.0), (6000, 15, -70.0)]);
        let v = push(&mut b, &row).unwrap();
        let sigs = v["signals"].as_array().unwrap();
        assert_eq!(sigs.len(), 2, "both peaks detected");
        let p0 = sigs[0]["power_db"].as_f64().unwrap();
        let p1 = sigs[1]["power_db"].as_f64().unwrap();
        assert!(p0 > p1, "strongest first: {p0} then {p1}");
        // Strongest should be the right (6000-bin) peak → higher freq.
        assert!(sigs[0]["freq_hz"].as_f64().unwrap() > b.center_freq_hz);
    }

    #[test]
    fn signal_drops_after_persist_window_of_misses() {
        let n = 4096;
        let mut b = block(
            n,
            SignalListParams {
                persist_window: 3,
                persist_hits: 1,
                emit_interval_ms: 0.0,
                ..Default::default()
            },
        );
        let with = make_row(n, -120.0, &[(3000, 20, -80.0)]);
        let without = make_row(n, -120.0, &[]);
        // Establish the signal.
        let v = push(&mut b, &with).unwrap();
        assert_eq!(v["signals"].as_array().unwrap().len(), 1);
        // Three misses (== persist_window) keep it; the fourth evicts.
        push(&mut b, &without);
        push(&mut b, &without);
        push(&mut b, &without);
        let v = push(&mut b, &without).unwrap();
        assert_eq!(
            v["signals"].as_array().unwrap().len(),
            0,
            "signal gone after > persist_window misses"
        );
    }

    #[test]
    fn top_k_caps_reported_count() {
        let n = 16384;
        let mut b = block(
            n,
            SignalListParams {
                persist_hits: 1,
                emit_interval_ms: 0.0,
                threshold_db: 8.0,
                top_k: 3,
                track_bucket_hz: 1_000.0,
                ..Default::default()
            },
        );
        // Six well-separated peaks of distinct strengths.
        let bumps: Vec<(usize, usize, f32)> = (0..6)
            .map(|i| (1500 + i * 2000, 10, -95.0 + i as f32 * 4.0))
            .collect();
        let row = make_row(n, -120.0, &bumps);
        let v = push(&mut b, &row).unwrap();
        assert_eq!(
            v["signals"].as_array().unwrap().len(),
            3,
            "top_k=3 caps the list"
        );
    }
}
