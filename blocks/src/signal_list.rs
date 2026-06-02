//! `SignalList` — strongest-signal detector on a wideband FFT byte stream.
//!
//! Taps the same `FftU8` bytes the waterfall renders (the output of
//! [`LogMagU8`](crate::log_mag_u8)) and continuously reports the strongest
//! signals across the whole span as a ranked watchlist.
//!
//! Like [`RssiProbe`](crate::rssi_probe) it's an **inline pass-through**:
//! `FftU8` in → `FftU8` out (the bytes copied through unchanged) plus an
//! `Events` out carrying the ranked list. The runtime is one-wire-per-port
//! (fan-out needs a `Tee`), so rather than tee the waterfall stream the
//! block sits *on* it — splice it between `LogMagU8` and the `ui:fft` sink
//! and the waterfall is unaffected while the detector taps every frame.
//! Node-side only — the per-frame scan feeds the browser's "Strongest
//! Signals" panel and the `signals` MCP verb, not the byte stream itself.
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
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, OutBuf, OutputPort, ParamKind,
    ParamSpec, Placement, PortSpec, PortType, ReconfigureScope, Work, MAX_PORTS,
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
    /// Drop candidate groups *wider* than this (Hz). `0` = no ceiling. The
    /// carrier-hunter knob: broadcast-overload humps and DC sidelobes are
    /// hundreds of kHz wide and otherwise dominate the top-K; cap it near a
    /// channel width (e.g. 12 kHz for AM, 5 kHz for narrow tones) to surface
    /// real carriers instead of blobs.
    pub max_bw_hz: f32,
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
    /// Peak-hold window (seconds). `0` = off (detect on the instantaneous
    /// frame). When > 0, the detector runs on a per-bin **max-hold** that
    /// decays a full-scale peak to the floor over this window — so a
    /// *transient* burst (APRS chirps, FT8/packet, pagers) that an
    /// instantaneous snapshot samples between is held long enough to be
    /// detected. The trade is staleness: a signal lingers in the list up
    /// to this long after it stops. ~8 s is a good band-survey value.
    pub peak_hold_secs: f32,
}

impl Default for SignalListParams {
    fn default() -> Self {
        Self {
            size: 16_384,
            threshold_db: 10.0,
            min_bw_hz: 0.0,
            max_bw_hz: 0.0,
            top_k: 16,
            persist_window: 5,
            persist_hits: 3,
            track_bucket_hz: 2_500.0,
            emit_interval_ms: 250.0,
            peak_hold_secs: 0.0,
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
    /// Latest estimated noise floor + effective detection threshold (dBFS),
    /// stashed each frame so `build_payload` can report them — disambiguates
    /// an empty watchlist ("quiet" vs "threshold too high").
    last_noise_floor_db: f32,
    last_threshold_db: f32,
    /// Per-bin max-hold accumulator (byte units) when `peak_hold_secs > 0`;
    /// empty otherwise. Decays toward 0 between frames so a transient peak
    /// lingers ~`peak_hold_secs`, then detection runs on this instead of the
    /// raw frame.
    held: Vec<u8>,
    /// JSON bytes waiting to drain to the `events` output.
    pending: Vec<u8>,
    #[cfg(not(target_arch = "wasm32"))]
    last_emit: Option<Instant>,
    /// Instant of the previous frame, for time-based peak-hold decay.
    #[cfg(not(target_arch = "wasm32"))]
    last_frame_at: Option<Instant>,
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
            last_noise_floor_db: SERVER_FLOOR_DBFS,
            last_threshold_db: SERVER_FLOOR_DBFS,
            held: Vec::new(),
            pending: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            last_emit: None,
            #[cfg(not(target_arch = "wasm32"))]
            last_frame_at: None,
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

    /// Fold one frame into the per-bin max-hold accumulator: each bin
    /// decays toward 0 by the time elapsed since the last frame (a full-
    /// scale peak fades over `peak_hold_secs`), then takes the max with the
    /// new value. A transient burst's peak therefore lingers ~`peak_hold_secs`
    /// so the snapshot detector can catch it between bursts.
    fn update_held(&mut self, row: &[u8]) {
        let n = self.params.size.min(row.len());
        if self.held.len() != self.params.size {
            self.held = vec![0u8; self.params.size];
        }
        #[cfg(not(target_arch = "wasm32"))]
        let decay: u8 = {
            let now = Instant::now();
            let dt = self
                .last_frame_at
                .map_or(0.0, |t| now.duration_since(t).as_secs_f32());
            self.last_frame_at = Some(now);
            let secs = self.params.peak_hold_secs.max(0.001);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let d = (255.0 / secs * dt).round().clamp(0.0, 255.0) as u8;
            d
        };
        // NativeOnly block — wasm path is dead; a fixed per-frame decay keeps
        // it compiling without pulling in a clock.
        #[cfg(target_arch = "wasm32")]
        let decay: u8 = 4;
        for (h, &r) in self.held[..n].iter_mut().zip(&row[..n]) {
            *h = h.saturating_sub(decay).max(r);
        }
    }

    /// Scan one frame into raw candidate groups, returning them alongside
    /// the estimated noise floor and the effective detection threshold (both
    /// dBFS). Reporting the floor + threshold lets a caller tell an empty
    /// list apart from "threshold too high for this band" — `0 signals` with
    /// `noise_floor_dbfs` well below `threshold_dbfs` means "quiet"; close
    /// together means "lower the threshold".
    fn detect(&self, row: &[u8]) -> (Vec<Candidate>, f32, f32) {
        let n = self.params.size.min(row.len());
        if n == 0 {
            return (Vec::new(), SERVER_FLOOR_DBFS, SERVER_FLOOR_DBFS);
        }
        let floor_byte = compute_spectrum_stats(&row[..n]).p10;
        let floor_db = byte_to_dbfs(floor_byte);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let thresh_byte = (f32::from(floor_byte) + self.params.threshold_db / DB_PER_BYTE)
            .clamp(0.0, 255.0) as u8;
        let threshold_db = byte_to_dbfs(thresh_byte);
        let bin_hz = self.sample_rate_hz / self.params.size as f64;

        // The zero-IF DC spike at the tuned centre is removed upstream by
        // the injected complex `DcBlock` (one source of truth for the
        // artifact), so the detector trusts the spectrum as-is — no
        // per-detector frequency notch here.
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
            // Bandwidth gate: drop runs narrower than `min_bw_hz` (noise
            // spikes) or — when `max_bw_hz > 0` — wider than it. The ceiling
            // turns `signals` into a carrier hunter: broadcast-overload humps
            // and DC sidelobes are 100s of kHz wide and crowd out the narrow
            // tones you're actually after.
            if bw_hz < f64::from(self.params.min_bw_hz)
                || (self.params.max_bw_hz > 0.0 && bw_hz > f64::from(self.params.max_bw_hz))
            {
                continue;
            }
            let freq_hz = self.bin_to_hz(peak_idx);
            // Drop unphysical negative absolute frequencies — the lower span
            // half below 0 Hz is image/alias junk, not receivable RF.
            if freq_hz < 0.0 {
                continue;
            }
            out.push(Candidate {
                freq_hz,
                power_db: byte_to_dbfs(peak),
                bw_hz,
                snr_db: byte_to_dbfs(peak) - floor_db,
            });
        }
        // Coalesce candidates closer than `track_bucket_hz` into one — a
        // carrier whose above-threshold run is split by a noise notch yields
        // several adjacent runs in a single frame, which would otherwise
        // become several separate tracks (the persistence bucket only dedups
        // *across* frames, one candidate per track per frame). Merge keeps
        // the strongest sub-peak's freq/power/snr and the union bandwidth.
        let out = coalesce_candidates(out, f64::from(self.params.track_bucket_hz).max(1.0));
        (out, floor_db, threshold_db)
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
            // Floor + effective threshold so an empty list is unambiguous:
            // floor well below threshold = quiet band; close = raise sensitivity.
            "noise_floor_dbfs": round2(self.last_noise_floor_db),
            "threshold_dbfs": round2(self.last_threshold_db),
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

/// Merge same-frame candidates whose centres are within `merge_dist_hz` of
/// each other into one — collapses a single carrier that fragmented into
/// several adjacent above-threshold runs (noise notches on the carrier)
/// into one detection. Keeps the strongest sub-peak's freq/power/snr and
/// the union bandwidth so the reported width still spans the whole carrier.
fn coalesce_candidates(mut cands: Vec<Candidate>, merge_dist_hz: f64) -> Vec<Candidate> {
    if cands.len() < 2 {
        return cands;
    }
    cands.sort_by(|a, b| {
        a.freq_hz
            .partial_cmp(&b.freq_hz)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut out: Vec<Candidate> = Vec::with_capacity(cands.len());
    for c in cands {
        if let Some(last) = out.last_mut() {
            if (c.freq_hz - last.freq_hz).abs() <= merge_dist_hz {
                let lo = (last.freq_hz - last.bw_hz / 2.0).min(c.freq_hz - c.bw_hz / 2.0);
                let hi = (last.freq_hz + last.bw_hz / 2.0).max(c.freq_hz + c.bw_hz / 2.0);
                if c.power_db > last.power_db {
                    last.freq_hz = c.freq_hz;
                    last.power_db = c.power_db;
                    last.snr_db = c.snr_db;
                }
                last.bw_hz = hi - lo;
                continue;
            }
        }
        out.push(c);
    }
    out
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
            outputs: &[
                // Pass-through of the input FftU8 frame, unchanged — lets
                // the block splice inline ahead of the `ui:fft` waterfall
                // sink without a Tee.
                PortSpec {
                    name: "out",
                    port_type: PortType::FftU8,
                },
                PortSpec {
                    name: "events",
                    port_type: PortType::Events,
                },
            ],
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
                    label: "Min SNR",
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
                    key: "max_bw_hz",
                    label: "Max bandwidth",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1_000_000.0,
                        step: 1_000.0,
                        default: 0.0,
                        unit: "Hz",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Carrier-hunter ceiling: drop groups wider than this. 0 = no limit. Set near a channel width (~12 kHz AM, ~5 kHz narrow tones) to filter out broadcast-overload humps and DC sidelobes that would otherwise dominate the list.",
                },
                ParamSpec {
                    key: "top_k",
                    label: "Top N",
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
                ParamSpec {
                    key: "peak_hold_secs",
                    label: "Peak hold",
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 60.0,
                        step: 0.5,
                        default: 0.0,
                        unit: "s",
                    },
                    reconfig_scope: ReconfigureScope::SelfBlock,
                    ai_notes: "Detect on a decaying per-bin max-hold over this window instead of the instantaneous frame. 0 = off. Set ~8 s to catch bursty/transient signals (APRS chirps, FT8, packet, pagers) a snapshot would miss — at the cost of a signal lingering in the list up to this long after it stops.",
                },
            ],
            ai_notes: "Strongest-signal detector on the wideband FFT byte stream. Inline pass-through (FftU8 in → FftU8 out, like RssiProbe): splice it between LogMagU8 and the ui:fft sink — logmag.out → signals.in → ui:fft, with the watchlist on signals.events → ui:signals. NativeOnly (node-side). Emits a ranked list of {id, freq_hz, power_db, bw_hz, snr_db} JSON events ~4 Hz, driving the UI's 'Strongest Signals' panel and the `signals` MCP verb; click/tune a row to retune there. The zero-IF DC/LO spike at the tuned centre is removed upstream by the injected complex `DcBlock`, so it isn't reported here — no per-detector notch.",
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
        // Re-read the input centre/rate.
        let new_rate = ctx.input_rate("in").unwrap_or(self.sample_rate_hz);
        let new_center = ctx
            .input_meta
            .iter()
            .find(|(n, _)| *n == "in")
            .map(|(_, m)| m.center_freq_hz)
            .unwrap_or(self.center_freq_hz);
        // Only invalidate the tracked set when the centre/rate ACTUALLY
        // changed (a real retune). The runtime re-runs `update_rates` on
        // every reconfigure — AFC nudges, unrelated source-param edits, a
        // re-compose — and clearing unconditionally blanked the whole list
        // for `persist_hits` frames each time, which the UI saw as the list
        // flickering out and back. A genuine retune still resets the
        // now-stale frequency buckets.
        let moved = (new_center - self.center_freq_hz).abs() > 0.5
            || (new_rate - self.sample_rate_hz).abs() > 0.5;
        self.sample_rate_hz = new_rate;
        self.center_freq_hz = new_center;
        if moved {
            self.tracks.clear();
            // The held spectrum belongs to the old centre — drop it too.
            self.held.clear();
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.last_frame_at = None;
            }
        }
        Ok(())
    }

    fn output_capacity_hints(&self) -> [usize; MAX_PORTS] {
        // The pass-through `out` carries one `size`-byte FftU8 frame.
        //
        // The runtime's per-tick output budget is the *minimum* writable
        // length across all of a block's output ports (see
        // `try_run_block`: `output_budget = min(per_port_cap…)`, and each
        // port is then handed `write_peek(output_budget)`). A port whose
        // hint is 0 caps at `frames_hint` (~1024), which would drag the
        // shared budget far below the 16384-byte frame the `out`
        // pass-through must emit — so `out` would only ever get a partial
        // slice and the waterfall would starve to ~1 fps. Give `events`
        // the same `size` hint so it never throttles the frame-sized
        // pass-through (it also sizes the events ring to comfortably hold
        // a JSON emission).
        let mut h = [0; MAX_PORTS];
        h[0] = self.params.size;
        h[1] = self.params.size;
        h
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
        // Detect on the peak-held spectrum (catches transient bursts) when
        // enabled, else on the raw frame.
        let use_hold = self.params.peak_hold_secs > 0.0;
        if use_hold {
            self.update_held(&row[..n]);
        }
        let spectrum: &[u8] = if use_hold { &self.held[..n] } else { &row[..n] };
        let (cands, floor_db, threshold_db) = self.detect(spectrum);
        self.last_noise_floor_db = floor_db;
        self.last_threshold_db = threshold_db;
        self.update_tracks(&cands);

        // Throttled emission: detection ran above on this frame, but the
        // ranked list is only published every `emit_interval_ms`.
        if self.emit_due() {
            let payload = self.build_payload();
            // Compact single-line JSON object — same bytes go to the
            // events port (for the browser WS) and the decoder trace. The
            // trace message is the raw JSON so `/api/signals` can parse it
            // directly into the current watchlist without a WS subscription.
            let json = serde_json::to_string(&payload).unwrap_or_default();
            tracing::info!(target: "decoder::signals", "{json}");
            self.pending.extend_from_slice(json.as_bytes());
            self.pending.push(b'\n');
        }

        // Copy the input frame to the pass-through `out` (waterfall) and
        // drain pending JSON to `events`. Ports are matched by name — the
        // scheduler's `outputs` order isn't guaranteed.
        let mut produced_iq = 0;
        let mut produced_events = 0;
        for port in io.outputs.iter_mut() {
            match port.name {
                "out" => {
                    if let Some(dst) = OutputPort::as_fft_u8_mut(port) {
                        let k = dst.len().min(n);
                        dst[..k].copy_from_slice(&row[..k]);
                        produced_iq = k;
                    }
                }
                "events" => {
                    if let OutBuf::Events(dst) = &mut port.buf {
                        let take = self.pending.len().min(dst.len());
                        if take > 0 {
                            dst[..take].copy_from_slice(&self.pending[..take]);
                            self.pending.drain(..take);
                            produced_events = take;
                        }
                    }
                }
                _ => {}
            }
        }

        let mut w = Work::new();
        w.consumed[0] = n;
        w.produced[0] = produced_iq;
        w.produced[1] = produced_events;
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
        let mut passthrough = vec![0u8; row.len()];
        let mut events = vec![0u8; 64 * 1024];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::FftU8(row),
        }];
        let mut outputs = [
            OutputPort {
                name: "out",
                meta: PortMeta::default(),
                buf: OutBuf::FftU8(&mut passthrough),
            },
            OutputPort {
                name: "events",
                meta: PortMeta::default(),
                buf: OutBuf::Events(&mut events),
            },
        ];
        let mut io = BlockIo {
            inputs: &mut inputs,
            outputs: &mut outputs,
        };
        let w = b.process(&mut io).unwrap();
        // Pass-through must copy the input frame verbatim.
        assert_eq!(w.produced[0], row.len(), "passthrough produced full frame");
        assert_eq!(&passthrough, row, "passthrough bytes match input");
        let len = w.produced[1];
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
            // The empty list is *explained*: floor ≈ -118 dBFS, threshold a
            // clear `threshold_db` above it — so a caller sees "quiet band",
            // not an ambiguous zero.
            let floor = v["noise_floor_dbfs"].as_f64().unwrap();
            let thresh = v["threshold_dbfs"].as_f64().unwrap();
            assert!((floor - -118.0).abs() < 3.0, "floor {floor} ≈ -118 dBFS");
            assert!(
                thresh > floor + 5.0,
                "threshold {thresh} should sit ~10 dB above floor {floor}"
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

    #[test]
    fn max_bw_drops_wide_blobs_keeps_narrow_carriers() {
        // A wide overload hump next to a narrow carrier; the ceiling should
        // surface only the carrier.
        let n = 16384;
        let mut b = block(
            n,
            SignalListParams {
                persist_hits: 1,
                emit_interval_ms: 0.0,
                threshold_db: 8.0,
                max_bw_hz: 10_000.0,
                track_bucket_hz: 1_000.0,
                ..Default::default()
            },
        );
        // bin_hz = 2.4e6/16384 ≈ 146 Hz. A ~300-bin-wide hump (~44 kHz) and a
        // ~5-bin (~730 Hz) carrier, well separated.
        let row = make_row(n, -120.0, &[(5000, 150, -70.0), (10000, 3, -75.0)]);
        let v = push(&mut b, &row).unwrap();
        let sigs = v["signals"].as_array().unwrap();
        assert_eq!(sigs.len(), 1, "wide blob filtered, narrow carrier kept");
        // The survivor is the narrow one near bin 10000.
        let f = sigs[0]["freq_hz"].as_f64().unwrap();
        let expected = 100_000_000.0 + (10000.0 - 8192.0) / 16384.0 * 2_400_000.0;
        assert!(
            (f - expected).abs() < 5_000.0,
            "survivor is the narrow carrier"
        );
    }

    #[test]
    fn negative_absolute_freqs_are_filtered() {
        // Centre low enough that the span's lower half maps below 0 Hz.
        let n = 4096;
        let mut b = SignalList::new(SignalListParams {
            size: n,
            persist_hits: 1,
            emit_interval_ms: 0.0,
            threshold_db: 8.0,
            ..Default::default()
        })
        .unwrap();
        b.sample_rate_hz = 2_400_000.0;
        b.center_freq_hz = 300_000.0; // span -900k..1500k → bins below 0 Hz
                                      // A bump deep in the negative-frequency half (bin 200 → way below 0).
        let row = make_row(n, -120.0, &[(200, 10, -70.0), (3000, 10, -75.0)]);
        let v = push(&mut b, &row).unwrap();
        let sigs = v["signals"].as_array().unwrap();
        assert!(
            sigs.iter().all(|s| s["freq_hz"].as_f64().unwrap() >= 0.0),
            "no negative absolute frequencies reported"
        );
        assert!(!sigs.is_empty(), "the positive-side carrier still survives");
    }

    #[test]
    fn split_carrier_coalesces_into_one() {
        // A carrier whose run is broken into three adjacent sub-runs by a
        // notch should report as ONE track, not three.
        let n = 8192;
        let mut b = block(
            n,
            SignalListParams {
                persist_hits: 1,
                emit_interval_ms: 0.0,
                threshold_db: 8.0,
                track_bucket_hz: 5_000.0,
                ..Default::default()
            },
        );
        // bin_hz ≈ 293 Hz. Three 1-bin peaks at 4000/4003/4006 (~880 Hz apart,
        // well within the 5 kHz bucket) with sub-threshold gaps between them.
        let mut row = make_row(n, -120.0, &[]);
        for bin in [4000usize, 4003, 4006] {
            row[bin] = dbfs_to_byte(-70.0);
        }
        let v = push(&mut b, &row).unwrap();
        let sigs = v["signals"].as_array().unwrap();
        assert_eq!(sigs.len(), 1, "split carrier coalesces to one track");
    }

    #[test]
    fn peak_hold_keeps_a_transient_detected_after_it_stops() {
        // A transient burst then a run of quiet frames. Without peak-hold the
        // track evicts within `persist_window` misses; with peak-hold the
        // held spectrum keeps the burst's peak, so it's re-detected every
        // frame and stays in the list — the APRS-chirp case.
        let n = 4096;
        let burst = make_row(n, -120.0, &[(3000, 20, -70.0)]);
        let quiet = make_row(n, -120.0, &[]);
        let params = |hold: f32| SignalListParams {
            size: n,
            peak_hold_secs: hold,
            persist_window: 3,
            persist_hits: 1,
            emit_interval_ms: 0.0,
            threshold_db: 8.0,
            ..Default::default()
        };

        // No hold: the transient is evicted once it stops.
        let mut off = block(n, params(0.0));
        push(&mut off, &burst);
        let mut last = None;
        for _ in 0..6 {
            last = push(&mut off, &quiet);
        }
        assert_eq!(
            last.unwrap()["signals"].as_array().unwrap().len(),
            0,
            "without peak-hold the transient is gone after it stops"
        );

        // Peak-hold: the held peak keeps it detected across the quiet frames.
        let mut on = block(n, params(10.0));
        push(&mut on, &burst);
        let mut last = None;
        for _ in 0..6 {
            last = push(&mut on, &quiet);
        }
        assert_eq!(
            last.unwrap()["signals"].as_array().unwrap().len(),
            1,
            "peak-hold retains the transient after it stops"
        );
    }
}
