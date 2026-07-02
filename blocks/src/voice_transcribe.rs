//! `VoiceTranscribe` — speech-to-text tap that runs on **either** side.
//!
//! Drops into the audio chain *after* noise-reduction and *before*
//! `AudioSink`:
//!
//! ```text
//! … → AudioNr → VoiceTranscribe → AudioSink
//! ```
//!
//! It is a **pure passthrough** on the audio path: every input sample is
//! copied to `out` unchanged (or zeroed in `muted` mode) so inserting it
//! never alters what the operator hears. Separately, when transcription
//! is active it taps the (clean) input and turns it into text.
//!
//! ## Two placements, one behaviour
//!
//! `Placement::Either` — the profile's `transcribe_placement` (via the
//! `"placement_role": "transcribe"` tag) chooses the side:
//!
//! * **Node (native):** the block owns the whole pipeline. `process()`
//!   mirrors the clean input into a shared tap ring; a worker thread
//!   drains it on its own coarse clock, resamples → VAD-segments →
//!   runs `whisper.cpp` → post-processes, and appends to a shared
//!   [`TranscriptState`]. Each tick `process()` drains the new entries
//!   to the `events` port (→ `ui:transcribe`, reaching the browser
//!   panel) **and** the `decoder::transcribe` log (→ `recent_decodes`).
//!   This is the headless path: transcription with no browser at all.
//!   The soapy-source pattern — a producer thread feeding a `Mutex`-
//!   shared buffer the scheduler tick drains — applied to inference.
//!
//! * **Browser (wasm):** unchanged from the original. The block mirrors
//!   input into an [`AudioRing`] that the browser runner drains into a
//!   `SharedArrayBuffer` for a Web Worker (Silero VAD + whisper.cpp
//!   WASM). The worker still posts results to the transcript store. (The
//!   VAD/cleanup/whisper logic the worker runs is the *same* Rust core
//!   this block uses node-side, compiled to wasm — see `ferrite-whisper`
//!   — so the two sides decode identically. The remaining browser-side
//!   wiring to the `events` port is the Stage B cutover.)
//!
//! Both sides share the spec (the `events` port, the params); only the
//! mechanics behind `process()` differ, cfg-split below. Inference is
//! never on the audio/scheduler thread on either side.
//!
//! ### Tri-state `mode`
//!
//! - `off`   — passthrough only. Zero cost: no tap, no transcription.
//! - `on`    — passthrough **and** transcription (hear it *and* log it).
//! - `muted` — transcription only; `out` is silenced. "Leave it logging
//!   a frequency you're not actively listening to."

use anyhow::Result;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};
// The owned tap ring is browser-only; node-side the tap lives inside the
// worker thread (see `mod native`).
#[cfg(target_arch = "wasm32")]
use crate::spsc_ring::AudioRing;

/// Ring capacity default — ~340 ms at 48 kHz. Larger than `AudioSink`'s
/// because the consumer is a VAD/STT worker on a coarser cadence than
/// the audio clock, so it tolerates (and benefits from) more slack.
const DEFAULT_BUFFER_SAMPLES: usize = 16_384;
const DEFAULT_BUFFER_SAMPLES_F64: f64 = 16_384.0;

/// Transcription engagement state. String-valued so the preset / UI
/// control reads `off` / `on` / `muted` rather than a magic number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Passthrough only — no tap, no transcription.
    Off,
    /// Passthrough **and** tap for transcription.
    On,
    /// Tap for transcription; audio output silenced.
    Muted,
}

impl Mode {
    pub fn parse(s: &str) -> Self {
        match s {
            "on" => Self::On,
            "muted" => Self::Muted,
            // Unknown / "off" — fail safe to the zero-cost state.
            _ => Self::Off,
        }
    }

    /// True when the block should mirror input into the tap ring.
    #[must_use]
    pub fn taps(self) -> bool {
        matches!(self, Self::On | Self::Muted)
    }

    /// True when the audio output should be passed through audibly.
    #[must_use]
    pub fn audible(self) -> bool {
        matches!(self, Self::Off | Self::On)
    }
}

/// Whisper ggml model ids — must match the keys in `MODELS`
/// (`web/src/lib/transcribe/whisperEngine.ts`). Node-side the worker
/// maps the id to a `ggml-*.bin` on disk; browser-side the JS worker
/// fetches the matching URL.
const MODEL_IDS: &[&str] = &["tiny.en-q5", "base.en-q5", "small.en-q5", "small.en-tdrz"];
const DEFAULT_MODEL_ID: &str = "tiny.en-q5";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VoiceTranscribeParams {
    /// `off` | `on` | `muted` — see [`Mode`].
    pub mode: String,
    /// Tap-ring capacity in samples (power of two).
    pub buffer_samples: usize,
    /// Whisper ggml model id — see [`MODEL_IDS`].
    pub model_id: String,
    /// Whisper `initial_prompt` override — vocab bias for the decoder.
    /// Empty = use the built-in ham corpus (see `hamPrompt.ts` /
    /// `ferrite_whisper::DEFAULT_HAM_PROMPT`).
    pub prompt: String,
    /// Momentary: set true to clear the rolling transcript, VAD floor,
    /// and callsign bias for a clean start on a new frequency. The block
    /// consumes it and reports it back false.
    pub reset: bool,
}

impl Default for VoiceTranscribeParams {
    fn default() -> Self {
        Self {
            mode: "off".to_string(),
            buffer_samples: DEFAULT_BUFFER_SAMPLES,
            model_id: DEFAULT_MODEL_ID.to_string(),
            prompt: String::new(),
            reset: false,
        }
    }
}

const MODE_PARAM: ParamSpec = ParamSpec {
    key: "mode",
    label: "Transcription",
    kind: ParamKind::EnumString {
        values: &["off", "on", "muted"],
        default: "off",
    },
    // Toggling engagement must take effect without tearing down the
    // source — it's the whole point of a "leave it running" control.
    reconfig_scope: ReconfigureScope::SelfBlock,
    ai_notes: "Speech-to-text engagement. 'off' = audio passes through untouched, no transcription (zero cost). 'on' = audio plays AND is transcribed. 'muted' = transcribed only, audio silenced — for logging a frequency you're not listening to.",
};

const MODEL_ID_PARAM: ParamSpec = ParamSpec {
    key: "model_id",
    label: "Model",
    kind: ParamKind::EnumString {
        values: MODEL_IDS,
        default: DEFAULT_MODEL_ID,
    },
    // Browser reloads live; node-side the worker loads the model once at
    // start, so a model change there takes effect on the next pipeline
    // restart. Both are non-disruptive to the audio source.
    reconfig_scope: ReconfigureScope::SelfBlock,
    ai_notes: "Whisper ggml model id. tiny is fast and bundled by default; small is better on noisy SSB; small.en-tdrz adds tinydiarize speaker-turn detection. The .bin must exist under web/static/models/ (browser) or be locatable via FERRITE_MODELS_DIR (node).",
};

const PROMPT_PARAM: ParamSpec = ParamSpec {
    key: "prompt",
    label: "Prompt",
    kind: ParamKind::Text { default: "" },
    reconfig_scope: ReconfigureScope::SelfBlock,
    ai_notes: "Whisper initial_prompt override — vocab bias steering the decoder. Empty = use the built-in dense ham corpus. The single biggest accuracy lever for ham voice.",
};

const RESET_PARAM: ParamSpec = ParamSpec {
    key: "reset",
    label: "Reset transcript",
    kind: ParamKind::Toggle { default: false },
    reconfig_scope: ReconfigureScope::SelfBlock,
    ai_notes: "Momentary: set true to clear the rolling transcript, the adaptive VAD noise floor, and the recently-heard-callsign bias — a clean start when you retune. Auto-clears.",
};

const BUFFER_SAMPLES_PARAM: ParamSpec = ParamSpec {
    key: "buffer_samples",
    label: "Tap ring capacity",
    kind: ParamKind::Range {
        min: 4_096.0,
        max: 1_048_576.0,
        step: 1.0,
        default: DEFAULT_BUFFER_SAMPLES_F64,
        unit: "samples",
    },
    reconfig_scope: ReconfigureScope::SourceRestart,
    ai_notes: "Tap-ring capacity in samples handed to the transcriber. Sized to a few hundred ms at the audio rate; larger only helps if the worker falls behind.",
};

// ─── Native worker plumbing (node-side, headless transcription) ─────────
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use ferrite_whisper::{Orchestrator, TranscriptState, Whisper};

    use crate::spsc_ring::AudioRing;

    /// Map a model id to its on-disk `ggml-*.bin` filename. Mirrors the
    /// `MODELS` url tails in `whisperEngine.ts`.
    fn model_filename(id: &str) -> &'static str {
        match id {
            "small.en-q5" => "ggml-small.en-q5_1.bin",
            "small.en-tdrz" => "ggml-small.en-tdrz.bin",
            "base.en-q5" => "ggml-base.en-q5_1.bin",
            // tiny.en-q5 and anything unknown → the bundled default.
            _ => "ggml-tiny.en-q5_1.bin",
        }
    }

    /// Find the model file across the usual roots. `FERRITE_MODELS_DIR`
    /// wins; then `FERRITE_STATIC_ROOT/models`; then the dev-tree and
    /// dist defaults relative to the working directory.
    fn locate_model(id: &str) -> Option<PathBuf> {
        let fname = model_filename(id);
        let mut dirs: Vec<PathBuf> = Vec::new();
        if let Some(d) = std::env::var_os("FERRITE_MODELS_DIR") {
            dirs.push(PathBuf::from(d));
        }
        if let Some(r) = std::env::var_os("FERRITE_STATIC_ROOT") {
            dirs.push(PathBuf::from(r).join("models"));
        }
        dirs.push(PathBuf::from("web/static/models"));
        dirs.push(PathBuf::from("web-dist/models"));
        dirs.into_iter().map(|d| d.join(fname)).find(|p| p.exists())
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Shared state between the scheduler tick (`process()`, the producer)
    /// and the inference worker thread (the consumer). All `Arc` so the
    /// block stays `Send` and the worker can be joined on `stop`.
    pub struct Worker {
        /// Tap ring — `process()` writes clean audio, the worker drains.
        pub tap: Arc<Mutex<AudioRing>>,
        /// Rolling transcript — the worker appends, `process()` drains
        /// the delta out to the `events` port + decoder log.
        pub transcript: Arc<Mutex<TranscriptState>>,
        /// Samples dropped because the tap was full (worker behind).
        pub dropped: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
        reset: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl Worker {
        /// Spawn the inference thread for `src_rate_hz` audio using the
        /// given model id + initial prompt. The thread loads the model
        /// once; if it can't be found/loaded it logs and exits, leaving
        /// the block a pure passthrough (audio is never affected).
        pub fn spawn(
            src_rate_hz: f64,
            model_id: &str,
            prompt_base: &str,
            tap_capacity: usize,
        ) -> Self {
            let tap = Arc::new(Mutex::new(AudioRing::new(tap_capacity)));
            let transcript = Arc::new(Mutex::new(TranscriptState::new()));
            if !prompt_base.is_empty() {
                transcript.lock().unwrap().set_prompt_base(prompt_base);
            }
            let dropped = Arc::new(AtomicU64::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let reset = Arc::new(AtomicBool::new(false));

            let model_path = locate_model(model_id);
            let handle = {
                let tap = Arc::clone(&tap);
                let transcript = Arc::clone(&transcript);
                let stop = Arc::clone(&stop);
                let reset = Arc::clone(&reset);
                thread::Builder::new()
                    .name("voice-transcribe".into())
                    .spawn(move || {
                        run(src_rate_hz, model_path, &tap, &transcript, &stop, &reset);
                    })
                    .ok()
            };

            Self {
                tap,
                transcript,
                dropped,
                stop,
                reset,
                handle,
            }
        }

        /// Request a transcript/VAD/bias reset on the worker.
        pub fn request_reset(&self) {
            self.reset.store(true, Ordering::Relaxed);
        }
    }

    impl Drop for Worker {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(h) = self.handle.take() {
                let _ = h.join();
            }
        }
    }

    /// The worker loop: drain tap → orchestrator (resample/VAD/queue) →
    /// whisper → transcript. Whisper lives entirely on this thread (it's
    /// `!Send` — one global context), exactly the constraint the wrapper
    /// documents.
    fn run(
        src_rate_hz: f64,
        model_path: Option<PathBuf>,
        tap: &Arc<Mutex<AudioRing>>,
        transcript: &Arc<Mutex<TranscriptState>>,
        stop: &Arc<AtomicBool>,
        reset: &Arc<AtomicBool>,
    ) {
        let Some(path) = model_path else {
            tracing::warn!(
                target: "decoder::transcribe",
                "no whisper model found (set FERRITE_MODELS_DIR); transcription disabled, audio unaffected"
            );
            return;
        };
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(target: "decoder::transcribe", error = %e, path = %path.display(), "read model failed");
                return;
            }
        };
        let Some(whisper) = Whisper::from_model_bytes(&bytes) else {
            tracing::warn!(target: "decoder::transcribe", path = %path.display(), "whisper engine failed to load model");
            return;
        };
        drop(bytes);
        tracing::info!(target: "decoder::transcribe", path = %path.display(), rate = src_rate_hz, "transcriber ready");

        let mut orch = Orchestrator::new(src_rate_hz);
        let mut scratch = vec![0.0_f32; 16_384];

        while !stop.load(Ordering::Relaxed) {
            if reset.swap(false, Ordering::Relaxed) {
                orch.reset();
                transcript.lock().unwrap().reset();
            }

            // Drain everything currently in the tap into the orchestrator.
            loop {
                let n = tap.lock().unwrap().read(&mut scratch);
                if n == 0 {
                    break;
                }
                orch.feed(&scratch[..n]);
                if n < scratch.len() {
                    break;
                }
            }

            // Run whisper on each closed utterance, recording results.
            // `rolling_prompt` is re-read each clip so the callsign bias
            // self-reinforces within a QSO.
            while let Some(clip) = orch.next_pending() {
                let prompt = transcript.lock().unwrap().rolling_prompt();
                let Some(segs) = whisper.transcribe(&clip.pcm, &prompt) else {
                    continue;
                };
                let end_ms = now_ms();
                let mut t = transcript.lock().unwrap();
                orch.ingest_result(&clip, &segs, end_ms, &mut t);
            }

            thread::sleep(Duration::from_millis(100));
        }
    }
}

pub struct VoiceTranscribe {
    params: VoiceTranscribeParams,
    mode: Mode,
    /// Negotiated input sample rate, captured at `init`. The transcriber
    /// needs it to resample to whisper's 16 kHz mono.
    input_rate_hz: f64,
    /// JSON spots queued for drain on the `events` output port (the
    /// ft8/wspr idiom). Filled from the transcript delta each tick.
    events_out: Vec<u8>,

    // Browser path: the block owns the tap ring; the browser runner
    // drains it via `ring_mut()` into a SharedArrayBuffer.
    #[cfg(target_arch = "wasm32")]
    ring: AudioRing,
    #[cfg(target_arch = "wasm32")]
    dropped_samples: u64,

    // Node path: a worker thread owns inference; the block shares a tap
    // ring + transcript with it. `None` until `init` spawns it (and when
    // no model is available).
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<native::Worker>,
}

impl VoiceTranscribe {
    #[must_use]
    pub fn new(params: VoiceTranscribeParams) -> Self {
        let mode = Mode::parse(&params.mode);
        #[cfg(target_arch = "wasm32")]
        let ring = AudioRing::new(params.buffer_samples.next_power_of_two());
        Self {
            mode,
            input_rate_hz: 0.0,
            events_out: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            ring,
            #[cfg(target_arch = "wasm32")]
            dropped_samples: 0,
            #[cfg(not(target_arch = "wasm32"))]
            worker: None,
            params,
        }
    }

    /// Cumulative count of samples dropped because the tap ring was full
    /// (the transcriber fell behind). Surfaced as a glitch counter.
    #[must_use]
    pub fn dropped_samples(&self) -> u64 {
        #[cfg(target_arch = "wasm32")]
        {
            self.dropped_samples
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::sync::atomic::Ordering;
            self.worker
                .as_ref()
                .map_or(0, |w| w.dropped.load(Ordering::Relaxed))
        }
    }

    /// Mutable view of the tap ring — drained by the browser runner after
    /// each tick into the worker's `SharedArrayBuffer`. Browser-only: the
    /// node path owns its tap inside the worker.
    #[cfg(target_arch = "wasm32")]
    pub fn ring_mut(&mut self) -> &mut AudioRing {
        &mut self.ring
    }

    /// Negotiated input sample rate (Hz), known after `init`. 0.0 before.
    #[must_use]
    pub fn input_rate_hz(&self) -> f64 {
        self.input_rate_hz
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    #[must_use]
    pub fn params(&self) -> &VoiceTranscribeParams {
        &self.params
    }

    /// Drain any new transcript entries into `events_out` as newline-
    /// delimited JSON and mirror each to the `decoder::transcribe` log.
    /// Node-only — the browser still feeds its store via the worker's
    /// postMessage (the Stage B cutover replaces that with this port).
    #[cfg(not(target_arch = "wasm32"))]
    fn drain_transcript(&mut self) {
        let Some(worker) = self.worker.as_ref() else {
            return;
        };
        let new = {
            let mut t = worker.transcript.lock().unwrap();
            t.drain_new()
        };
        for entry in &new {
            if let Ok(line) = serde_json::to_string(entry) {
                self.events_out.extend_from_slice(line.as_bytes());
                self.events_out.push(b'\n');
            }
            // The log path → `recent_decodes` MCP tool / decoder feed.
            tracing::info!(target: "decoder::transcribe", text = %entry.text, raw = %entry.raw_text);
        }
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for VoiceTranscribe {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "VoiceTranscribe",
            placement: Placement::Either,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[
                PortSpec {
                    name: "out",
                    port_type: PortType::RealF32,
                },
                // Structured transcript spots (raw + cleaned text, timing,
                // confidence) — same `PortType::Events` transport FT8/WSPR/
                // RDS use. Wire `voice_transcribe.events → ui:transcribe`;
                // env_split turns it into a WsBridge (node) or EventsSink
                // (browser) automatically.
                PortSpec {
                    name: "events",
                    port_type: PortType::Events,
                },
            ],
            params: &[
                MODE_PARAM,
                MODEL_ID_PARAM,
                PROMPT_PARAM,
                RESET_PARAM,
                BUFFER_SAMPLES_PARAM,
            ],
            ai_notes: "Passthrough tap that transcribes demodulated voice to text (Silero/energy VAD + whisper.cpp). Insert between AudioNr and AudioSink. Runs node-side (headless) or browser-side via placement. Audio passes through unchanged ('off'/'on') or silenced ('muted'); it never degrades the signal. Emits transcript spots on the 'events' port (raw + cleaned text) and the decoder log. Built for noisy SSB/AM voice, accuracy over latency.",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        self.input_rate_hz = ctx.input_rate("in").unwrap_or(0.0);
        self.events_out.clear();

        #[cfg(target_arch = "wasm32")]
        {
            self.ring.reset();
            self.dropped_samples = 0;
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            // Spawn the inference worker when transcription is engaged and
            // we know the audio rate. `off` stays zero-cost (no thread).
            self.worker = None;
            if self.mode.taps() && self.input_rate_hz > 0.0 {
                self.worker = Some(native::Worker::spawn(
                    self.input_rate_hz,
                    &self.params.model_id,
                    &self.params.prompt,
                    self.params.buffer_samples.next_power_of_two(),
                ));
            }
        }
        Ok(())
    }

    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let mut changed = false;
        if let Some(m) = delta.get("mode").and_then(serde_json::Value::as_str) {
            self.params.mode = m.to_string();
            let new_mode = Mode::parse(m);
            #[cfg(not(target_arch = "wasm32"))]
            {
                // Engage → spawn (if we have a rate and no worker yet);
                // disengage → tear the worker down so it stops decoding.
                if new_mode.taps() && self.worker.is_none() && self.input_rate_hz > 0.0 {
                    self.worker = Some(native::Worker::spawn(
                        self.input_rate_hz,
                        &self.params.model_id,
                        &self.params.prompt,
                        self.params.buffer_samples.next_power_of_two(),
                    ));
                } else if !new_mode.taps() {
                    self.worker = None;
                }
            }
            self.mode = new_mode;
            changed = true;
        }
        if let Some(v) = delta.get("model_id").and_then(serde_json::Value::as_str) {
            if !MODEL_IDS.contains(&v) {
                anyhow::bail!("VoiceTranscribe: model_id must be one of {MODEL_IDS:?}, got {v:?}");
            }
            self.params.model_id = v.to_string();
            changed = true;
        }
        if let Some(v) = delta.get("prompt").and_then(serde_json::Value::as_str) {
            self.params.prompt = v.to_string();
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(w) = self.worker.as_ref() {
                w.transcript.lock().unwrap().set_prompt_base(v);
            }
            changed = true;
        }
        if delta
            .get("reset")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(w) = self.worker.as_ref() {
                w.request_reset();
            }
            // Momentary — never latch true.
            self.params.reset = false;
            changed = true;
        }
        Ok(changed)
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let mut w = Work::new();
        let Some(src) = io
            .inputs
            .iter()
            .find(|p| p.name == "in")
            .and_then(InputPort::as_real_f32)
        else {
            return Ok(w);
        };
        // Borrow `io.outputs` directly (disjoint from the `io.inputs`
        // borrow held by `src`). Mirrors `TeeRealF32`.
        let Some(out) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(|p| p.as_real_f32_mut())
        else {
            return Ok(w);
        };
        let n = src.len().min(out.len());

        // Tap the *clean* input first (regardless of mute state).
        if self.mode.taps() {
            #[cfg(target_arch = "wasm32")]
            {
                let written = self.ring.write(&src[..n]);
                if written < n {
                    // Drop on overflow — the worker clock must never
                    // back-pressure the audio pipeline.
                    self.dropped_samples += (n - written) as u64;
                }
            }
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(worker) = self.worker.as_ref() {
                use std::sync::atomic::Ordering;
                let written = worker.tap.lock().unwrap().write(&src[..n]);
                if written < n {
                    worker
                        .dropped
                        .fetch_add((n - written) as u64, Ordering::Relaxed);
                }
            }
        }

        // Passthrough (or silence in `muted`).
        if self.mode.audible() {
            out[..n].copy_from_slice(&src[..n]);
        } else {
            out[..n].fill(0.0);
        }
        w.consumed[0] = n;
        w.produced[0] = n;

        // Drain new transcript entries into the events buffer (node path).
        #[cfg(not(target_arch = "wasm32"))]
        self.drain_transcript();

        // Spill the events buffer into the `events` port (idx 1). Any
        // remainder stays for the next tick — audio flows continuously so
        // `process()` is called again soon.
        if !self.events_out.is_empty() {
            for port in io.outputs.iter_mut() {
                if port.name == "events" {
                    if let crate::block::OutBuf::Events(dst) = &mut port.buf {
                        let take = self.events_out.len().min(dst.len());
                        if take > 0 {
                            dst[..take].copy_from_slice(&self.events_out[..take]);
                            self.events_out.drain(..take);
                            w.produced[1] = take;
                        }
                    }
                }
            }
        }

        Ok(w)
    }

    fn stop(&mut self) -> Result<()> {
        self.events_out.clear();
        #[cfg(target_arch = "wasm32")]
        self.ring.reset();
        #[cfg(not(target_arch = "wasm32"))]
        {
            // Dropping the worker sets its stop flag and joins the thread.
            self.worker = None;
        }
        Ok(())
    }
}

impl BlockFactory for VoiceTranscribe {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: VoiceTranscribeParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(VoiceTranscribe::new(p)))
    }
}

#[cfg(test)]
mod tests {
    use super::{Mode, VoiceTranscribe, VoiceTranscribeParams, DEFAULT_BUFFER_SAMPLES};
    use crate::block::{Block, BlockIo, InBuf, InputPort, OutBuf, OutputPort, Placement, PortMeta};

    /// Run one `process` with the given input; returns (out audio, events
    /// bytes). Provides both the `out` and `events` ports the spec now
    /// declares.
    fn run(vt: &mut VoiceTranscribe, samples: &[f32]) -> (Vec<f32>, Vec<u8>) {
        let mut out = vec![-1.0_f32; samples.len()];
        let mut events = vec![0u8; 4096];
        let mut inputs = [InputPort {
            name: "in",
            meta: PortMeta::default(),
            buf: InBuf::RealF32(samples),
        }];
        let mut outputs = [
            OutputPort {
                name: "out",
                meta: PortMeta::default(),
                buf: OutBuf::RealF32(&mut out),
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
        let work = vt.process(&mut io).unwrap();
        assert_eq!(work.consumed[0], samples.len());
        assert_eq!(work.produced[0], samples.len());
        let used = work.produced[1];
        out.truncate(samples.len());
        events.truncate(used);
        (out, events)
    }

    #[test]
    fn spec_is_either_with_events_port() {
        let s = VoiceTranscribe::spec();
        assert_eq!(s.type_name, "VoiceTranscribe");
        assert!(matches!(s.placement, Placement::Either));
        assert_eq!(s.inputs.len(), 1);
        assert_eq!(s.outputs.len(), 2);
        assert_eq!(s.outputs[0].name, "out");
        assert_eq!(s.outputs[0].port_type, crate::block::PortType::RealF32);
        assert_eq!(s.outputs[1].name, "events");
        assert_eq!(s.outputs[1].port_type, crate::block::PortType::Events);
    }

    #[test]
    fn default_mode_is_off_zero_cost() {
        let p = VoiceTranscribeParams::default();
        assert_eq!(p.mode, "off");
        assert_eq!(p.buffer_samples, DEFAULT_BUFFER_SAMPLES);
        let mut vt = VoiceTranscribe::new(p);
        assert_eq!(vt.mode(), Mode::Off);
        let (out, events) = run(&mut vt, &[0.1, 0.2, 0.3]);
        // Passthrough verbatim, nothing tapped, no events.
        assert_eq!(out, vec![0.1, 0.2, 0.3]);
        assert!(events.is_empty());
        assert_eq!(vt.dropped_samples(), 0);
    }

    #[test]
    fn on_mode_passes_through_audio() {
        let mut vt = VoiceTranscribe::new(VoiceTranscribeParams {
            mode: "on".into(),
            buffer_samples: 16,
            ..Default::default()
        });
        let (out, _) = run(&mut vt, &[0.5, -0.5, 0.25]);
        assert_eq!(out, vec![0.5, -0.5, 0.25], "audio passes through");
    }

    #[test]
    fn muted_mode_silences_output() {
        let mut vt = VoiceTranscribe::new(VoiceTranscribeParams {
            mode: "muted".into(),
            buffer_samples: 16,
            ..Default::default()
        });
        let (out, _) = run(&mut vt, &[0.5, -0.5, 0.25]);
        assert_eq!(out, vec![0.0, 0.0, 0.0], "output silenced");
    }

    #[test]
    fn buffer_samples_rounded_to_power_of_two_wasm() {
        // Browser path keeps the owned ring; capacity rounds to pow2.
        #[cfg(target_arch = "wasm32")]
        {
            let mut vt = VoiceTranscribe::new(VoiceTranscribeParams {
                mode: "off".into(),
                buffer_samples: 5000,
                ..Default::default()
            });
            assert_eq!(vt.ring_mut().capacity(), 8192);
        }
        // On native the tap lives in the worker; just assert construction
        // is fine with a non-pow2 request (it's rounded at spawn).
        #[cfg(not(target_arch = "wasm32"))]
        {
            let vt = VoiceTranscribe::new(VoiceTranscribeParams {
                mode: "off".into(),
                buffer_samples: 5000,
                ..Default::default()
            });
            assert_eq!(vt.mode(), Mode::Off);
        }
    }

    /// The structured-events path: push a transcript entry into the shared
    /// state the worker would write, then assert `process()` drains it as
    /// newline-delimited JSON on the `events` port. No model / whisper
    /// needed — this tests the block's UI-write contract directly.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn node_drains_transcript_to_events_port() {
        use ferrite_whisper::RawSegment;

        let mut vt = VoiceTranscribe::new(VoiceTranscribeParams {
            mode: "on".into(),
            buffer_samples: 1024,
            ..Default::default()
        });
        // Force a worker so the shared transcript exists. (It may have no
        // model and thus decode nothing — irrelevant here; we write the
        // transcript directly.)
        vt.input_rate_hz = 16_000.0;
        vt.worker = Some(super::native::Worker::spawn(
            16_000.0,
            "tiny.en-q5",
            "",
            1024,
        ));
        // Inject a finished segment as the worker would.
        {
            let w = vt.worker.as_ref().unwrap();
            let mut t = w.transcript.lock().unwrap();
            let seg = RawSegment::new(1_000, 0.0, 1.0, "whiskey one alpha bravo");
            t.record(seg).expect("recorded");
        }
        let (out, events) = run(&mut vt, &[0.0, 0.0, 0.0]);
        assert_eq!(out, vec![0.0, 0.0, 0.0]); // on-mode passthrough of silence
        let text = String::from_utf8(events).unwrap();
        assert!(
            text.contains("\"text\":\"W1AB\""),
            "cleaned text on wire: {text}"
        );
        assert!(
            text.contains("\"rawText\":\"whiskey one alpha bravo\""),
            "raw text on wire: {text}"
        );
        assert!(text.ends_with('\n'), "newline-delimited");
    }
}
