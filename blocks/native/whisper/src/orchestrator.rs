//! Transcription orchestrator — Rust port of the queue / drain / lead-
//! drop logic in `web/src/lib/transcribe/transcribeWorker.ts`.
//!
//! It owns the *portable* half of the pipeline — resample → VAD-segment
//! → backlog-bounded queue, and the post-inference bookkeeping (max-cut
//! lead drop, paragraph-break gap, wall-clock stamping, callsign bias
//! harvest into [`TranscriptState`]). Inference itself is **not** here:
//! `whisper_full` is a multi-second blocking call, so the caller (the
//! block's worker thread native, the Web Worker in the browser) pulls a
//! pending clip with [`next_pending`](Orchestrator::next_pending), runs
//! whisper on its own clock, and feeds the result back through
//! [`ingest_result`](Orchestrator::ingest_result). Keeping inference
//! pluggable makes the orchestrator unit-testable with a fake decoder
//! and lets the same struct drive both placements (Stage B).
//!
//! The split mirrors the TS worker exactly:
//! - `poll()` → [`feed`]: drain audio, resample, VAD, enqueue (shed
//!   oldest past `MAX_PENDING` so latency can't balloon).
//! - `transcribeOne()` → [`ingest_result`]: per-segment lead drop,
//!   `cont`/`gap`/`atMs`, record into the transcript.

use std::collections::VecDeque;

use crate::resample::LinearResampler;
use crate::transcript::{RawSegment, TranscriptState};
use crate::vad::{MeterState, VadConfig, VadSegmenter};
use crate::Segment;

/// Default backlog depth. A speaker faster than whisper sheds the
/// OLDEST queued utterance once this many are waiting, bounding latency
/// at the cost of a counted "missing section". Mirrors `MAX_PENDING`.
pub const MAX_PENDING: usize = 6;

/// A closed utterance waiting for (or returning from) whisper. 16 kHz
/// mono PCM plus the two timing hints the VAD attached.
#[derive(Debug, Clone)]
pub struct PendingSeg {
    /// Contiguous 16 kHz mono PCM for the clip.
    pub pcm: Vec<f32>,
    /// Lead-in (ms) at the front carried from a previous max-cut; whisper
    /// segments ending inside it were already emitted by the prior clip.
    pub lead_ms: f32,
    /// Silence (ms) before this utterance — paragraph-break hint; only
    /// the first kept segment of the clip carries it.
    pub gap_ms: f32,
}

/// Owns resample + VAD + the bounded pending queue. One per block.
pub struct Orchestrator {
    resampler: LinearResampler,
    segmenter: VadSegmenter,
    pending: VecDeque<PendingSeg>,
    max_pending: usize,
    dropped_total: u64,
}

impl Orchestrator {
    /// Build for a given source sample rate using the default VAD tuning
    /// and backlog depth.
    #[must_use]
    pub fn new(src_rate_hz: f64) -> Self {
        Self::with_config(src_rate_hz, VadConfig::default(), MAX_PENDING)
    }

    /// Build with explicit VAD config + backlog depth (tests / tuning).
    #[must_use]
    pub fn with_config(src_rate_hz: f64, vad: VadConfig, max_pending: usize) -> Self {
        Self {
            resampler: LinearResampler::new(src_rate_hz),
            segmenter: VadSegmenter::new(vad),
            pending: VecDeque::new(),
            max_pending: max_pending.max(1),
            dropped_total: 0,
        }
    }

    /// Feed source-rate mono audio: resample → VAD → enqueue any closed
    /// utterances. When the queue is already full the OLDEST pending clip
    /// is shed (counted in [`dropped_total`]). Returns the number shed
    /// this call. Mirrors `poll` + `enqueueSegment`.
    pub fn feed(&mut self, src_pcm: &[f32]) -> usize {
        let resampled = self.resampler.feed(src_pcm);
        let mut shed = 0;
        for seg in self.segmenter.feed(&resampled) {
            if self.pending.len() >= self.max_pending {
                // Whisper is behind the speaker — drop oldest so latency
                // stays bounded; surfaced as a glitch / loud log line.
                self.pending.pop_front();
                self.dropped_total += 1;
                shed += 1;
            }
            self.pending.push_back(PendingSeg {
                pcm: seg.pcm,
                lead_ms: seg.lead_overlap_ms,
                gap_ms: seg.gap_ms,
            });
        }
        shed
    }

    /// Pop the oldest pending clip for the caller to transcribe, or
    /// `None` when the queue is empty.
    pub fn next_pending(&mut self) -> Option<PendingSeg> {
        self.pending.pop_front()
    }

    /// Clips waiting for inference.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Total clips shed to backlog pressure since construction / reset.
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.dropped_total
    }

    /// Live gate/level snapshot for the UI meter.
    #[must_use]
    pub fn meter(&self) -> MeterState {
        self.segmenter.meter_state()
    }

    /// Apply a whisper result for `item` and record kept segments into
    /// `transcript`. `end_ms` is wall-clock (ms since epoch) at clip
    /// close — the caller stamps it so this crate stays clock-free.
    /// Returns the number of segments actually recorded.
    ///
    /// Port of `transcribeOne`'s segment loop: drop segments that fall
    /// entirely inside a carried max-cut lead (already emitted), compute
    /// `cont` / `gap` / `atMs`, and let [`TranscriptState::record`] do
    /// the ham post-process, empty-drop, and callsign harvest. `kept`
    /// counts only non-empty (recorded) segments, exactly as the TS does.
    pub fn ingest_result(
        &self,
        item: &PendingSeg,
        segments: &[Segment],
        end_ms: u64,
        transcript: &mut TranscriptState,
    ) -> usize {
        let lead_sec = f64::from(item.lead_ms) / 1000.0;
        // Whisper segment times place each segment relative to clip end:
        // anchor on the last segment's t1 so intra-utterance ordering
        // survives (otherwise every segment shares one timestamp).
        let last_t1 = segments.last().map_or(0.0, |s| s.t1);
        let mut kept = 0usize;
        for seg in segments {
            // Carried-over max-cut lead: a segment ending inside it was
            // already emitted by the previous clip — drop it. One that
            // straddles the boundary is kept (the word re-decoded with
            // its lead-in context, the whole point of the overlap).
            if lead_sec > 0.0 && seg.t1 <= lead_sec {
                continue;
            }
            // First kept segment continues the prior clip iff this clip
            // carried a max-cut lead (mid-utterance). Later sub-segments
            // are always continuous. cont=false ⇒ paragraph break.
            let cont = kept > 0 || item.lead_ms > 0.0;
            // Only the first kept chunk of a fresh utterance carries the
            // preceding pause; continuations report 0.
            let seg_gap_ms = if kept == 0 && item.lead_ms == 0.0 {
                item.gap_ms
            } else {
                0.0
            };
            let back_ms = ((last_t1 - seg.t1).max(0.0) * 1000.0).round() as u64;
            let at_ms = end_ms.saturating_sub(back_ms);
            let confidence = seg.avg_logprob.exp().clamp(0.0, 1.0) as f32;
            let raw = RawSegment {
                at_ms,
                vfo_hz: None,
                t0: seg.t0,
                t1: seg.t1,
                text: seg.text.clone(),
                confidence,
                no_speech_prob: seg.no_speech_prob as f32,
                cont,
                gap_ms: seg_gap_ms,
                speaker_turn: seg.speaker_turn,
            };
            // record() ham-post-processes, drops empties (returns None),
            // and harvests callsigns into the rolling prompt bias. Only
            // count the ones that survived — matching the TS `continue`.
            if transcript.record(raw).is_some() {
                kept += 1;
            }
        }
        kept
    }

    /// Full reset between sessions / on the block's reset knob: clears the
    /// queue, relearns the noise floor, drops any in-progress utterance
    /// and the resampler's carried phase. The drop counter is preserved
    /// (it's a lifetime glitch tally, like soapy's `ring_drops`).
    pub fn reset(&mut self) {
        self.resampler.reset();
        self.segmenter.reset();
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 16 kHz so the orchestrator's resampler is identity — keeps the
    /// VAD math identical to the standalone vad.rs tests.
    fn orch() -> Orchestrator {
        Orchestrator::new(16_000.0)
    }

    fn tone(n: usize, amp: f32) -> Vec<f32> {
        (0..n).map(|i| amp * (i as f32 * 0.17).sin()).collect()
    }

    fn seg(t0: f64, t1: f64, text: &str) -> Segment {
        Segment {
            t0,
            t1,
            text: text.to_string(),
            avg_logprob: -0.2,
            no_speech_prob: 0.01,
            speaker_turn: false,
        }
    }

    #[test]
    fn feed_enqueues_closed_utterances() {
        let mut o = orch();
        for _ in 0..20 {
            let _ = o.feed(&tone(320, 1e-4)); // prime floor
        }
        let _ = o.feed(&tone(16_000, 0.2)); // 1 s speech
        let _ = o.feed(&vec![0.0; 16_000]); // silence closes it
        assert_eq!(o.pending_len(), 1);
        let p = o.next_pending().expect("one clip");
        assert!(p.pcm.len() >= 16_000);
        assert_eq!(o.pending_len(), 0);
    }

    #[test]
    fn backlog_sheds_oldest_past_max_pending() {
        // max_pending=2; close 3 utterances without ever draining → 1 shed.
        let mut o = Orchestrator::with_config(16_000.0, VadConfig::default(), 2);
        for _ in 0..20 {
            let _ = o.feed(&tone(320, 1e-4));
        }
        let mut shed = 0;
        for _ in 0..3 {
            shed += o.feed(&tone(16_000, 0.2));
            shed += o.feed(&vec![0.0; 16_000]);
        }
        assert_eq!(shed, 1, "third utterance shed the oldest");
        assert_eq!(o.dropped_total(), 1);
        assert_eq!(o.pending_len(), 2, "queue capped at max_pending");
    }

    #[test]
    fn ingest_drops_segments_inside_the_lead() {
        let o = orch();
        let mut t = TranscriptState::new();
        let item = PendingSeg {
            pcm: vec![],
            lead_ms: 750.0, // 0.75 s carried lead
            gap_ms: 0.0,
        };
        // First segment ends at 0.5 s — inside the lead, must drop.
        // Second ends at 1.2 s — straddles/past the lead, kept.
        let segs = [seg(0.0, 0.5, "already heard"), seg(0.6, 1.2, "CQ CQ")];
        let kept = o.ingest_result(&item, &segs, 10_000, &mut t);
        assert_eq!(kept, 1, "only the post-lead segment recorded");
        let snap = t.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].raw_text, "CQ CQ");
        // Carried a lead ⇒ first kept segment continues the prior clip.
        assert!(snap[0].cont);
    }

    #[test]
    fn ingest_first_fresh_segment_carries_gap_not_cont() {
        let o = orch();
        let mut t = TranscriptState::new();
        let item = PendingSeg {
            pcm: vec![],
            lead_ms: 0.0,
            gap_ms: 1500.0,
        };
        let segs = [seg(0.0, 1.0, "seventy three"), seg(1.0, 2.0, "over")];
        let kept = o.ingest_result(&item, &segs, 20_000, &mut t);
        assert_eq!(kept, 2);
        let snap = t.snapshot();
        assert!(!snap[0].cont, "fresh utterance starts a paragraph");
        assert_eq!(snap[0].gap_ms, 1500.0, "first carries the pause");
        assert!(snap[1].cont, "later sub-segment is continuous");
        assert_eq!(snap[1].gap_ms, 0.0, "continuation reports no gap");
    }

    #[test]
    fn ingest_empty_cleanup_does_not_advance_kept() {
        // A segment that ham-post cleans to empty must be skipped without
        // making the next segment look continuous.
        let o = orch();
        let mut t = TranscriptState::new();
        let item = PendingSeg {
            pcm: vec![],
            lead_ms: 0.0,
            gap_ms: 800.0,
        };
        // "[BLANK_AUDIO]"-style noise cleans to empty; real text follows.
        let segs = [seg(0.0, 0.5, "   "), seg(0.5, 1.5, "CQ contest")];
        let kept = o.ingest_result(&item, &segs, 30_000, &mut t);
        assert_eq!(kept, 1);
        let snap = t.snapshot();
        assert_eq!(snap.len(), 1);
        // The surviving segment is still the FIRST kept ⇒ not cont, and
        // it inherits the utterance gap.
        assert!(!snap[0].cont);
        assert_eq!(snap[0].gap_ms, 800.0);
    }

    #[test]
    fn ingest_stamps_atms_relative_to_clip_end() {
        let o = orch();
        let mut t = TranscriptState::new();
        let item = PendingSeg {
            pcm: vec![],
            lead_ms: 0.0,
            gap_ms: 0.0,
        };
        // Last segment ends at t1=3.0; first ends at 1.0 ⇒ 2 s before end.
        let segs = [seg(0.0, 1.0, "alpha"), seg(2.0, 3.0, "bravo")];
        let _ = o.ingest_result(&item, &segs, 100_000, &mut t);
        let snap = t.snapshot();
        assert_eq!(snap[0].at_ms, 98_000, "first segment 2 s before end");
        assert_eq!(snap[1].at_ms, 100_000, "last segment at clip end");
    }

    #[test]
    fn reset_clears_queue() {
        let mut o = orch();
        for _ in 0..20 {
            let _ = o.feed(&tone(320, 1e-4));
        }
        let _ = o.feed(&tone(16_000, 0.2));
        let _ = o.feed(&vec![0.0; 16_000]);
        assert_eq!(o.pending_len(), 1);
        o.reset();
        assert_eq!(o.pending_len(), 0);
    }
}
