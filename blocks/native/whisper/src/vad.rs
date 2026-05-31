//! Voice-activity segmenter — Rust port of `web/src/lib/transcribe/vad.ts`.
//!
//! Adaptive energy gate with hangover: feed resampled 16 kHz mono f32;
//! each time the gate closes it yields one contiguous utterance plus two
//! timing hints the orchestrator needs:
//!
//! * `lead_overlap_ms` — audio (ms) at the *front* of the buffer carried
//!   from a previous max-cut, so a word straddling the hard segment
//!   boundary keeps its lead-in context. The decoder drops the redundant
//!   re-decode by whisper timestamp. 0 for silence-closed / first
//!   segments.
//! * `gap_ms` — silence that preceded this utterance (the inter-speaker
//!   pause), so the rolling transcript can break a paragraph. Only the
//!   first chunk of an utterance carries it; mid-utterance max-cut
//!   continuations report 0.
//!
//! This is a straight, behaviour-preserving port of the TS so the node
//! and browser sides segment identically. The "real" Silero gate lives
//! inside whisper.cpp; this energy fallback is what actually runs (the
//! glue keeps in-whisper VAD off), so it is the production path on both
//! sides. Pure + unit-testable; no allocation on the hot frame path
//! beyond the accumulator growth.

/// Tuning knobs. Defaults match `DEFAULT_VAD` in the TS verbatim.
#[derive(Debug, Clone, Copy)]
pub struct VadConfig {
    /// Input sample rate — always 16 kHz post-resample.
    pub rate_hz: u32,
    /// Speech must exceed `noise_floor * open_ratio` to open a segment.
    pub open_ratio: f32,
    /// Trailing silence before a segment closes (ms). Ham PTT gaps are
    /// long — generous so words aren't clipped.
    pub hangover_ms: f32,
    /// Drop segments shorter than this (ms) — key clicks, splatter.
    pub min_speech_ms: f32,
    /// Hard cap so a stuck-open gate still flushes (ms). Also the
    /// worst-case latency on continuous speech.
    pub max_segment_ms: f32,
    /// Audio (ms) carried from a max-cut into the next segment as lead-in.
    pub overlap_ms: f32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            rate_hz: 16_000,
            open_ratio: 3.0,
            hangover_ms: 700.0,
            min_speech_ms: 350.0,
            max_segment_ms: 10_000.0,
            overlap_ms: 750.0,
        }
    }
}

/// 20 ms @ 16 kHz — the VAD analysis frame.
const FRAME: usize = 320;

/// One closed utterance handed to the caller.
#[derive(Debug, Clone)]
pub struct VadSegment {
    /// Contiguous 16 kHz mono PCM for the utterance.
    pub pcm: Vec<f32>,
    /// Lead-in (ms) at the front carried from a previous max-cut.
    pub lead_overlap_ms: f32,
    /// Silence (ms) before this utterance; 0 for mid-utterance chunks.
    pub gap_ms: f32,
}

fn rms(buf: &[f32]) -> f32 {
    if buf.is_empty() {
        return 0.0;
    }
    let sum: f32 = buf.iter().map(|x| x * x).sum();
    (sum / buf.len() as f32).sqrt()
}

/// Streaming energy-VAD segmenter. Drive with [`feed`](Self::feed); pull
/// completed utterances with the closure-free [`take`](Self::take)
/// pattern via the returned vec, or poll [`meter_state`](Self::meter_state)
/// for the live gate/level readout.
pub struct VadSegmenter {
    cfg: VadConfig,
    noise_floor: f32,
    speaking: bool,
    silence_ms: f32,
    acc: Vec<f32>,
    held: Vec<f32>,
    last_level: f32,
    lead_overlap_ms: f32,
    idle_ms: f32,
    utterance_gap_ms: f32,
    /// Completed utterances awaiting the caller. Drained by `feed`'s
    /// return value; this avoids the TS callback so the segmenter stays
    /// a plain value with no borrow gymnastics.
    ready: Vec<VadSegment>,
}

/// Live gate/level snapshot for the UI meter.
#[derive(Debug, Clone, Copy)]
pub struct MeterState {
    pub level: f32,
    pub threshold: f32,
    pub speaking: bool,
}

impl VadSegmenter {
    #[must_use]
    pub fn new(cfg: VadConfig) -> Self {
        Self {
            cfg,
            noise_floor: 1e-4,
            speaking: false,
            silence_ms: 0.0,
            acc: Vec::new(),
            held: Vec::new(),
            last_level: 0.0,
            lead_overlap_ms: 0.0,
            idle_ms: 0.0,
            utterance_gap_ms: 0.0,
            ready: Vec::new(),
        }
    }

    #[must_use]
    pub fn meter_state(&self) -> MeterState {
        MeterState {
            level: self.last_level,
            threshold: self.noise_floor * self.cfg.open_ratio,
            speaking: self.speaking,
        }
    }

    /// Feed resampled 16 kHz mono. Returns any utterances that closed
    /// during this call (usually empty; one or more on a gate close /
    /// max-cut).
    pub fn feed(&mut self, chunk: &[f32]) -> Vec<VadSegment> {
        // Reframe to fixed 20 ms windows, carrying a partial across calls.
        if self.held.is_empty() {
            let mut off = 0;
            while off + FRAME <= chunk.len() {
                self.process_frame(&chunk[off..off + FRAME]);
                off += FRAME;
            }
            if off < chunk.len() {
                self.held.extend_from_slice(&chunk[off..]);
            }
        } else {
            let mut data = std::mem::take(&mut self.held);
            data.extend_from_slice(chunk);
            let mut off = 0;
            while off + FRAME <= data.len() {
                self.process_frame(&data[off..off + FRAME]);
                off += FRAME;
            }
            if off < data.len() {
                self.held.extend_from_slice(&data[off..]);
            }
        }
        std::mem::take(&mut self.ready)
    }

    fn process_frame(&mut self, frame: &[f32]) {
        let level = rms(frame);
        self.last_level = level;
        let frame_ms = (FRAME as f32 / self.cfg.rate_hz as f32) * 1000.0;
        let open = level > self.noise_floor * self.cfg.open_ratio;

        if open {
            if !self.speaking {
                self.speaking = true;
                // New utterance — accumulated idle is the pause before it.
                self.utterance_gap_ms = self.idle_ms;
                self.idle_ms = 0.0;
            }
            self.silence_ms = 0.0;
            self.acc.extend_from_slice(frame);
        } else if self.speaking {
            // Accumulate through the hangover so trailing consonants /
            // short inter-word gaps stay in one segment. Do NOT adapt the
            // floor here (hangover is soft speech, not silence).
            self.acc.extend_from_slice(frame);
            self.silence_ms += frame_ms;
            if self.silence_ms >= self.cfg.hangover_ms {
                self.flush(false);
            }
        } else {
            // True idle: the only safe place to learn the floor, and
            // where the inter-utterance pause is measured.
            self.noise_floor = self.noise_floor * 0.97 + level * 0.03;
            self.idle_ms += frame_ms;
        }

        // Hard cap on continuous speech — cut mid-word, carry a tail.
        if self.acc.len() as f32 / self.cfg.rate_hz as f32 >= self.cfg.max_segment_ms / 1000.0 {
            self.flush(true);
        }
    }

    fn flush(&mut self, max_cut: bool) {
        let ms = (self.acc.len() as f32 / self.cfg.rate_hz as f32) * 1000.0;
        let emitted_lead_ms = self.lead_overlap_ms;
        let emitted_gap_ms = self.utterance_gap_ms;
        self.utterance_gap_ms = 0.0;

        let pcm: Vec<f32> = if max_cut {
            // Still mid-utterance: keep the last `overlap_ms` as lead-in
            // for the next segment; emit a copy of the full accumulator.
            let full = self.acc.clone();
            let tail_n = self
                .acc
                .len()
                .min(((self.cfg.overlap_ms / 1000.0) * self.cfg.rate_hz as f32).round() as usize);
            let keep_from = self.acc.len() - tail_n;
            self.acc.drain(..keep_from);
            self.lead_overlap_ms = (self.acc.len() as f32 / self.cfg.rate_hz as f32) * 1000.0;
            self.silence_ms = 0.0;
            // `speaking` stays true — we never stopped.
            full
        } else {
            let full = std::mem::take(&mut self.acc);
            self.lead_overlap_ms = 0.0;
            self.speaking = false;
            self.silence_ms = 0.0;
            // The hangover that closed this is real dead air — count it
            // toward the next utterance's gap.
            self.idle_ms = self.cfg.hangover_ms;
            full
        };

        if ms >= self.cfg.min_speech_ms {
            self.ready.push(VadSegment {
                pcm,
                lead_overlap_ms: emitted_lead_ms,
                gap_ms: emitted_gap_ms,
            });
        }
    }

    /// Full reset — clears the accumulator, relearns the floor, forgets
    /// any in-progress utterance. Used by the block's `reset`.
    pub fn reset(&mut self) {
        self.speaking = false;
        self.silence_ms = 0.0;
        self.acc.clear();
        self.held.clear();
        self.noise_floor = 1e-4;
        self.last_level = 0.0;
        self.lead_overlap_ms = 0.0;
        self.idle_ms = 0.0;
        self.utterance_gap_ms = 0.0;
        self.ready.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: f32) -> Vec<f32> {
        // 440-ish Hz sine at 16k so RMS is well above the noise floor.
        (0..n).map(|i| amp * (i as f32 * 0.17).sin()).collect()
    }

    #[test]
    fn silence_closes_an_utterance_after_hangover() {
        let mut v = VadSegmenter::new(VadConfig::default());
        // Prime the floor with quiet noise so the gate has a baseline.
        for _ in 0..20 {
            let _ = v.feed(&tone(FRAME, 1e-4));
        }
        // ~1 s of speech-level tone, then >700 ms of silence to close.
        let mut got = Vec::new();
        got.extend(v.feed(&tone(16_000, 0.2)));
        got.extend(v.feed(&vec![0.0; 16_000])); // 1 s silence
        assert_eq!(got.len(), 1, "exactly one utterance closed");
        let seg = &got[0];
        assert!(seg.pcm.len() >= 16_000, "utterance holds the speech");
        assert_eq!(seg.lead_overlap_ms, 0.0, "silence-closed ⇒ no lead");
    }

    #[test]
    fn max_cut_emits_and_carries_lead_overlap() {
        let cfg = VadConfig {
            max_segment_ms: 1000.0, // force a cut quickly
            ..VadConfig::default()
        };
        let mut v = VadSegmenter::new(cfg);
        for _ in 0..20 {
            let _ = v.feed(&tone(FRAME, 1e-4));
        }
        // 1.5 s continuous tone → at least one max-cut at the 1 s mark.
        let got = v.feed(&tone(24_000, 0.2));
        assert!(!got.is_empty(), "max-cut produced a segment");
        // A subsequent close should report a non-zero lead (the carried
        // overlap tail from the max-cut).
        let more = v.feed(&vec![0.0; 16_000]);
        if let Some(seg) = more.first() {
            assert!(seg.lead_overlap_ms > 0.0, "continuation carries lead-in");
        }
    }

    #[test]
    fn min_speech_counts_accumulated_audio_including_hangover() {
        // TS-faithful semantics: the hangover silence is appended to the
        // accumulator before flush, so even a short blip accumulates well
        // past `min_speech_ms` (blip + 700 ms hangover) and IS emitted.
        // This documents the real behaviour both sides must share — the
        // `min_speech_ms` guard only drops sub-frame transients that
        // never sustain the gate, not short-but-real speech.
        let mut v = VadSegmenter::new(VadConfig::default());
        for _ in 0..20 {
            let _ = v.feed(&tone(FRAME, 1e-4));
        }
        let mut got = Vec::new();
        got.extend(v.feed(&tone(1600, 0.2))); // 100 ms speech
        got.extend(v.feed(&vec![0.0; 16_000])); // silence closes it
        assert_eq!(got.len(), 1, "blip + hangover exceeds min_speech ⇒ emitted");
        // And the emitted buffer carries the trailing hangover padding.
        assert!(
            got[0].pcm.len() as f32 / 16_000.0 * 1000.0 >= 350.0,
            "accumulated length includes hangover"
        );
    }

    #[test]
    fn pure_silence_emits_nothing() {
        // The genuine "nothing to decode" path: no frame ever opens the
        // gate, so no utterance closes.
        let mut v = VadSegmenter::new(VadConfig::default());
        let got = v.feed(&vec![0.0; 48_000]); // 3 s of silence
        assert!(got.is_empty());
        assert!(!v.meter_state().speaking);
    }

    #[test]
    fn reset_clears_in_progress_utterance() {
        let mut v = VadSegmenter::new(VadConfig::default());
        let _ = v.feed(&tone(8_000, 0.2)); // open, mid-utterance
        v.reset();
        assert!(!v.meter_state().speaking);
        // Silence after reset must not flush a leftover utterance.
        let got = v.feed(&vec![0.0; 16_000]);
        assert!(got.is_empty());
    }
}
