//! `WasmTranscriber` — the browser worker's handle to the *same* Rust
//! transcription core the node-side block runs.
//!
//! Stage B of the transcribe unification: the in-browser worker used to
//! carry its own TypeScript copy of VAD + ham-cleanup + resample + the
//! queue/drain orchestration (`vad.ts`, `hamPostProcess.ts`,
//! `resample.ts`, `transcribeWorker.ts`). That logic now lives once, in
//! `ferrite-whisper`, compiled to wasm32 alongside its native build —
//! so node and browser segment, clean, and bookkeep *identically* by
//! construction, and the TS can be deleted.
//!
//! The one thing that stays JavaScript is the **inference call** itself:
//! whisper.cpp runs as an Emscripten module the worker loads directly
//! (`whisperEngine.ts` → `_wsp_transcribe`). This crate's
//! [`ferrite_whisper`] dep stubs that FFI on wasm32 on purpose — the
//! orchestrator is built with inference *pluggable* so the caller drives
//! it. The worker loop is therefore:
//!
//! ```text
//! drain SAB → feed(pcm)                 // resample + VAD + enqueue
//!           → take_pending()            // pop one closed utterance (16 kHz)
//!           → whisperEngine.transcribe  // ← Emscripten, the only JS step
//!           → ingest_inflight(segs,now) // max-cut drop, cont/gap, record
//!           → drain_new()               // entries to post to the store
//! ```
//!
//! One clip is in flight at a time (the worker is single-threaded), so
//! the popped [`PendingSeg`] is stashed in `inflight` between
//! `take_pending` and `ingest_inflight` rather than crossing the JS
//! boundary as an opaque handle.

#![cfg(feature = "wasm")]

use ferrite_whisper::{Orchestrator, PendingSeg, Segment, TranscriptState};
use serde::Serialize;
use wasm_bindgen::prelude::*;

/// Browser-side transcription core. Wraps [`Orchestrator`] (resample +
/// VAD + bounded queue) and [`TranscriptState`] (rolling transcript +
/// prompt bias) behind a small JS-facing API.
#[wasm_bindgen]
pub struct WasmTranscriber {
    orch: Orchestrator,
    transcript: TranscriptState,
    /// The clip handed out by the last `take_pending`, awaiting its
    /// whisper result via `ingest_inflight`. `None` between cycles.
    inflight: Option<PendingSeg>,
}

/// Live gate/level snapshot for the panel meter — mirrors the TS
/// `meterState()` shape the old worker posted as telemetry.
#[derive(Serialize)]
struct JsMeter {
    level: f32,
    threshold: f32,
    speaking: bool,
}

#[wasm_bindgen]
impl WasmTranscriber {
    /// Build for a given source sample rate (Hz). Uses the default VAD
    /// tuning + backlog depth — identical to the node-side block.
    #[wasm_bindgen(constructor)]
    #[must_use]
    pub fn new(src_rate_hz: f64) -> Self {
        Self {
            orch: Orchestrator::new(src_rate_hz),
            transcript: TranscriptState::new(),
            inflight: None,
        }
    }

    /// Feed source-rate mono audio (the SAB drain). Resamples → VAD →
    /// enqueues closed utterances. Returns how many clips were shed to
    /// backlog pressure this call (whisper behind the speaker).
    pub fn feed(&mut self, src_pcm: &[f32]) -> usize {
        self.orch.feed(src_pcm)
    }

    /// Pop the next closed utterance for the caller to transcribe, as a
    /// 16 kHz mono `Float32Array`, or `undefined` when the queue is
    /// empty. The clip is held internally until [`Self::ingest_inflight`]
    /// (or the next `take_pending`, which drops an un-ingested one).
    #[wasm_bindgen(js_name = takePending)]
    pub fn take_pending(&mut self) -> Option<Vec<f32>> {
        let item = self.orch.next_pending()?;
        let pcm = item.pcm.clone();
        self.inflight = Some(item);
        Some(pcm)
    }

    /// The `initial_prompt` for the next `whisper.transcribe`: the base
    /// corpus plus recently-heard callsigns. Read once per clip.
    #[wasm_bindgen(js_name = rollingPrompt)]
    #[must_use]
    pub fn rolling_prompt(&self) -> String {
        self.transcript.rolling_prompt()
    }

    /// Apply the whisper result for the in-flight clip. `segments_json`
    /// is the `segments` array from `whisperEngine.transcribe` (the
    /// camelCase shape `Segment` deserializes: `t0`/`t1`/`text`/
    /// `avgLogprob`/`noSpeechProb`/`speakerTurn`). `end_ms` is wall-clock
    /// at clip close. No-op (returns 0) if there's no in-flight clip.
    #[wasm_bindgen(js_name = ingestInflight)]
    pub fn ingest_inflight(&mut self, segments_json: &str, end_ms: f64) -> Result<usize, JsError> {
        let Some(item) = self.inflight.take() else {
            return Ok(0);
        };
        let segs: Vec<Segment> = serde_json::from_str(segments_json)
            .map_err(|e| JsError::new(&format!("segments JSON: {e}")))?;
        Ok(self
            .orch
            .ingest_result(&item, &segs, end_ms as u64, &mut self.transcript))
    }

    /// Take the transcript entries appended since the last call — the
    /// worker posts these to the store. Serialises to the same camelCase
    /// `TranscriptEntry` shape the node path emits over `ui:transcribe`.
    #[wasm_bindgen(js_name = drainNew)]
    pub fn drain_new(&mut self) -> Result<JsValue, JsError> {
        let entries = self.transcript.drain_new();
        serde_wasm_bindgen::to_value(&entries)
            .map_err(|e| JsError::new(&format!("drain_new serialize: {e}")))
    }

    /// Override the prompt base (the Transcript tab's editable field).
    /// Empty restores the built-in ham corpus.
    #[wasm_bindgen(js_name = setPromptBase)]
    pub fn set_prompt_base(&mut self, text: &str) {
        self.transcript.set_prompt_base(text);
    }

    /// Clips waiting for inference (queue depth) — for the panel's
    /// backlog readout.
    #[wasm_bindgen(js_name = pendingLen)]
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.orch.pending_len()
    }

    /// Queued-but-not-yet-transcribed audio, in milliseconds (16 kHz).
    /// The worker adds this to the still-in-ring backlog for the panel's
    /// "~Ns behind" lag readout.
    #[wasm_bindgen(js_name = queuedMs)]
    #[must_use]
    pub fn queued_ms(&self) -> f64 {
        self.orch.queued_samples() as f64 / 16.0
    }

    /// Total clips shed to backlog pressure since construction / reset —
    /// the glitch counter the panel surfaces as "dropped".
    #[wasm_bindgen(js_name = droppedTotal)]
    #[must_use]
    pub fn dropped_total(&self) -> u64 {
        self.orch.dropped_total()
    }

    /// Live gate/level snapshot for the meter (`{level, threshold,
    /// speaking}`).
    #[wasm_bindgen]
    pub fn meter(&self) -> Result<JsValue, JsError> {
        let m = self.orch.meter();
        let js = JsMeter {
            level: m.level,
            threshold: m.threshold,
            speaking: m.speaking,
        };
        serde_wasm_bindgen::to_value(&js)
            .map_err(|e| JsError::new(&format!("meter serialize: {e}")))
    }

    /// Full reset between sessions / on the panel's reset: clears the
    /// queue, relearns the noise floor, drops any in-flight clip + the
    /// transcript history + the rolling callsign bias. The prompt base
    /// (an operator setting) survives.
    pub fn reset(&mut self) {
        self.orch.reset();
        self.transcript.reset();
        self.inflight = None;
    }
}
