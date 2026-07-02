//! Transcript state — the rolling, resettable record the block owns and
//! the UI renders. The Rust counterpart of
//! `web/src/lib/transcribe/store.svelte.ts` (display side) plus the
//! Worker's rolling-prompt / callsign-bias bookkeeping
//! (`transcribeWorker.ts`), unified into one struct.
//!
//! It deliberately keeps **both** the raw whisper output and the
//! ham-post-processed text per entry, so the UI can show either (and so
//! debugging a bad cleanup never loses the original). `reset` clears
//! everything — transcript history, the rolling callsign prompt bias,
//! and the running id counter — for a clean "start fresh on this
//! frequency". The owning block also resets its VAD segmenter alongside.
//!
//! No audio, no inference here: the worker thread feeds finished
//! segments in; this is pure bookkeeping + the prompt the worker reads
//! back before each `whisper.transcribe`.

use serde::Serialize;

use crate::ham_post;

/// Default ham-vocabulary `initial_prompt`, mirrored from
/// `web/src/lib/transcribe/hamPrompt.ts`. Biases whisper toward Q-codes,
/// prosigns, and phonetics so spoken callsigns transcribe phonetically
/// (then `ham_post` spells them). Overridable per-session.
pub const DEFAULT_HAM_PROMPT: &str = "CQ CQ contest. QSL QRZ QSO QTH QRM QRN QSY. \
RST five nine. seventy three. roger, over, break. \
whiskey one alpha bravo, kilo two x-ray yankee. grid square. \
seventy three and best regards.";

/// Keep at most this many recently-heard callsigns appended to the
/// rolling prompt (the tail self-reinforces within a QSO). Mirrors
/// `MAX_PROMPT_CALLS` in the worker.
const MAX_PROMPT_CALLS: usize = 16;
/// Hard cap on the callsign history vec so it can't grow unbounded.
const MAX_CALL_HISTORY: usize = MAX_PROMPT_CALLS * 3;
/// Display-ring cap, mirrors `MAX_SEGMENTS` in the svelte store.
const MAX_ENTRIES: usize = 2000;

/// One closed-utterance transcript entry. Serialises to the same shape
/// the UI consumes (camelCase) so it can ride the Events wire / log and
/// land in the browser `transcript` store unchanged.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptEntry {
    /// Monotonic id within this session (resets on `reset`).
    pub id: u64,
    /// Wall-clock at segment close, ms since epoch. Stamped by the
    /// caller (the runtime owns the clock; this crate stays clock-free).
    pub at_ms: u64,
    /// VFO absolute frequency (Hz) at capture, or None when unknown.
    pub vfo_hz: Option<f64>,
    /// Segment start/end within the captured clip (seconds).
    pub t0: f64,
    pub t1: f64,
    /// Raw whisper text, before ham post-processing. Kept so the UI can
    /// toggle raw/clean and so a bad cleanup never destroys the source.
    pub raw_text: String,
    /// Final ham-post-processed text (callsigns/reports/Q-codes applied).
    pub text: String,
    /// Segment confidence (avg token log-prob mapped to 0..1).
    pub confidence: f32,
    /// Whisper no-speech probability — high ⇒ likely VAD false-fire.
    pub no_speech_prob: f32,
    /// Continues the previous entry with no speaker pause between.
    pub cont: bool,
    /// Silence (ms) before this utterance; 0 mid-utterance. UI breaks a
    /// paragraph when it exceeds a threshold.
    pub gap_ms: f32,
    /// tinydiarize speaker-turn flag (only meaningful on tdrz models).
    pub speaker_turn: bool,
}

/// The block's rolling transcript + prompt bias. Cheap to clone-free
/// share behind a `Mutex` between the worker thread (writer) and the
/// block's drain (reader).
#[derive(Debug, Default)]
pub struct TranscriptState {
    entries: Vec<TranscriptEntry>,
    /// Entries appended since the last `drain_new` — the delta the block
    /// ships out as Events / log lines each tick.
    pending: Vec<TranscriptEntry>,
    recent_calls: Vec<String>,
    prompt_base: String,
    next_id: u64,
}

impl TranscriptState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            pending: Vec::new(),
            recent_calls: Vec::new(),
            prompt_base: DEFAULT_HAM_PROMPT.to_string(),
            next_id: 1,
        }
    }

    /// Override the prompt base (the Transcript tab's editable field).
    /// Empty restores the built-in ham corpus.
    pub fn set_prompt_base(&mut self, text: &str) {
        self.prompt_base = if text.trim().is_empty() {
            DEFAULT_HAM_PROMPT.to_string()
        } else {
            text.to_string()
        };
    }

    /// The `initial_prompt` for the next `whisper.transcribe`: the base
    /// plus the last N recently-heard callsigns. Read by the worker
    /// before each inference.
    #[must_use]
    pub fn rolling_prompt(&self) -> String {
        if self.recent_calls.is_empty() {
            return self.prompt_base.clone();
        }
        let tail_start = self.recent_calls.len().saturating_sub(MAX_PROMPT_CALLS);
        let tail = self.recent_calls[tail_start..].join(" ");
        format!("{} {tail}", self.prompt_base)
    }

    /// Record a finished segment: post-processes raw → clean, harvests
    /// callsigns into the prompt bias, assigns an id, then appends to both
    /// the history ring and the pending delta. Returns the stored entry
    /// (with id) or `None` when the cleaned text is empty (dropped —
    /// silence / pure noise).
    pub fn record(&mut self, raw: RawSegment) -> Option<TranscriptEntry> {
        let clean = ham_post::apply(&raw.text);
        if clean.is_empty() {
            return None;
        }
        for c in ham_post::extract_callsigns(&clean) {
            if !self.recent_calls.contains(&c) {
                self.recent_calls.push(c);
                if self.recent_calls.len() > MAX_CALL_HISTORY {
                    self.recent_calls.remove(0);
                }
            }
        }
        let entry = TranscriptEntry {
            id: self.next_id,
            at_ms: raw.at_ms,
            vfo_hz: raw.vfo_hz,
            t0: raw.t0,
            t1: raw.t1,
            raw_text: raw.text,
            text: clean,
            confidence: raw.confidence,
            no_speech_prob: raw.no_speech_prob,
            cont: raw.cont,
            gap_ms: raw.gap_ms,
            speaker_turn: raw.speaker_turn,
        };
        self.next_id += 1;
        self.entries.push(entry.clone());
        if self.entries.len() > MAX_ENTRIES {
            self.entries.remove(0);
        }
        self.pending.push(entry.clone());
        Some(entry)
    }

    /// Take the entries appended since the last call — the block drains
    /// these each tick into Events / decoder-log output.
    pub fn drain_new(&mut self) -> Vec<TranscriptEntry> {
        std::mem::take(&mut self.pending)
    }

    /// Full snapshot of the rolling transcript (newest last). For a UI
    /// state request / snapshot replay.
    #[must_use]
    pub fn snapshot(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    /// Clear **everything**: transcript history, pending delta, rolling
    /// callsign bias, and the id counter. The prompt base is preserved
    /// (it's an operator setting, not session state). The owning block
    /// resets its VAD segmenter alongside this.
    pub fn reset(&mut self) {
        self.entries.clear();
        self.pending.clear();
        self.recent_calls.clear();
        self.next_id = 1;
    }
}

/// Input to [`TranscriptState::record`] — a decoded segment before
/// cleanup. (The clock-stamped fields are filled by the runtime caller.)
#[derive(Debug, Clone)]
pub struct RawSegment {
    pub at_ms: u64,
    pub vfo_hz: Option<f64>,
    pub t0: f64,
    pub t1: f64,
    pub text: String,
    pub confidence: f32,
    pub no_speech_prob: f32,
    pub cont: bool,
    pub gap_ms: f32,
    pub speaker_turn: bool,
}

impl RawSegment {
    /// Minimal constructor for the common case (timing + text), other
    /// fields defaulted; the worker fills confidence/gap/etc.
    #[must_use]
    pub fn new(at_ms: u64, t0: f64, t1: f64, text: impl Into<String>) -> Self {
        Self {
            at_ms,
            vfo_hz: None,
            t0,
            t1,
            text: text.into(),
            confidence: 0.0,
            no_speech_prob: 0.0,
            cont: false,
            gap_ms: 0.0,
            speaker_turn: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(text: &str) -> RawSegment {
        RawSegment::new(0, 0.0, 1.0, text)
    }

    #[test]
    fn record_keeps_raw_and_clean() {
        let mut t = TranscriptState::new();
        let e = t.record(raw("whiskey one alpha bravo")).expect("kept");
        assert_eq!(e.raw_text, "whiskey one alpha bravo");
        assert_eq!(e.text, "W1AB");
        assert_eq!(e.id, 1);
    }

    #[test]
    fn empty_cleanup_is_dropped() {
        let mut t = TranscriptState::new();
        assert!(t.record(raw("   ")).is_none());
        assert!(t.snapshot().is_empty());
    }

    #[test]
    fn rolling_prompt_biases_toward_heard_callsigns() {
        let mut t = TranscriptState::new();
        let _ = t.record(raw("thanks whiskey one alpha bravo"));
        assert!(t.rolling_prompt().contains("W1AB"));
    }

    #[test]
    fn drain_new_returns_only_the_delta() {
        let mut t = TranscriptState::new();
        let _ = t.record(raw("CQ CQ"));
        assert_eq!(t.drain_new().len(), 1);
        assert_eq!(t.drain_new().len(), 0, "delta consumed");
        let _ = t.record(raw("seventy three"));
        assert_eq!(t.drain_new().len(), 1);
    }

    #[test]
    fn reset_clears_history_and_prompt_bias_but_keeps_base() {
        let mut t = TranscriptState::new();
        t.set_prompt_base("custom base");
        let _ = t.record(raw("thanks W1AB over")); // wait: cleaned form
        let _ = t.record(raw("whiskey one alpha bravo"));
        assert!(!t.snapshot().is_empty());
        t.reset();
        assert!(t.snapshot().is_empty());
        assert_eq!(t.next_id, 1);
        // Prompt base survives; callsign bias is gone.
        assert!(t.rolling_prompt().starts_with("custom base"));
        assert!(!t.rolling_prompt().contains("W1AB"));
    }

    #[test]
    fn ids_are_monotonic_and_history_capped() {
        let mut t = TranscriptState::new();
        for _ in 0..3 {
            let _ = t.record(raw("CQ"));
        }
        let ids: Vec<u64> = t.snapshot().iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }
}
