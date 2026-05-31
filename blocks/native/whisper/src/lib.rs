//! Native whisper.cpp speech-to-text, behind a safe Rust wrapper.
//!
//! Wraps the `wsp_*` C ABI from `shim/whisper_glue.c` (see `build.rs`
//! for how it's compiled + linked). The engine is the *same*
//! whisper.cpp the browser runs via Emscripten, the *same* ggml model
//! files — so node-side and browser-side decode are identical by
//! construction. This crate is the node-side half: a batch decoder
//! (feed PCM, get segments) that the `transcribe_file` op and a future
//! node-side `VoiceTranscribe` build on.
//!
//! ## Targets
//!
//! Native: real FFI into the statically-linked engine. wasm32: a stub
//! that reports "unavailable" — the browser path doesn't use this crate
//! (it loads the Emscripten module from the TS worker directly), but the
//! stub keeps the crate compilable for any all-targets workspace build.
//!
//! ## Threading
//!
//! whisper.cpp keeps a single global model context (`g_ctx` in the
//! glue). The wrapper is therefore **not** safe to call concurrently
//! from multiple threads against one loaded model; callers should own a
//! single [`Whisper`] and serialize `transcribe` calls (the capture
//! pipeline is naturally serial). Marked accordingly — no `Sync`.

use serde::Deserialize;

pub mod ham_post;
pub mod orchestrator;
pub mod resample;
pub mod transcript;
pub mod vad;

pub use orchestrator::{Orchestrator, PendingSeg, MAX_PENDING};
pub use resample::{LinearResampler, WHISPER_RATE};
pub use transcript::{RawSegment, TranscriptEntry, TranscriptState};
pub use vad::{MeterState, VadConfig, VadSegment, VadSegmenter};

/// One decoded segment: half-open time range (**seconds** from clip
/// start) + text. The glue emits `t0`/`t1` as whisper centiseconds
/// divided by 100, i.e. fractional seconds (`1.50` = 1.5 s).
///
/// The confidence/diarization fields are emitted by the glue in
/// camelCase and parsed best-effort (`#[serde(default)]`): older glue
/// builds that omit them still deserialize. They drive the UI's
/// per-segment dimming and speaker-turn marks; the orchestrator maps
/// `avg_logprob` to a 0..1 confidence.
#[derive(Debug, Clone, Deserialize)]
pub struct Segment {
    /// Segment start, seconds from the start of the fed PCM.
    pub t0: f64,
    /// Segment end, seconds.
    pub t1: f64,
    /// Decoded text (leading space as whisper emits it; callers trim).
    pub text: String,
    /// Mean token log-probability (≤ 0). `exp` of it ≈ confidence.
    #[serde(default, rename = "avgLogprob")]
    pub avg_logprob: f64,
    /// Whisper's no-speech probability — high ⇒ likely VAD false-fire.
    #[serde(default, rename = "noSpeechProb")]
    pub no_speech_prob: f64,
    /// tinydiarize speaker-turn flag (only meaningful on tdrz models).
    #[serde(default, rename = "speakerTurn")]
    pub speaker_turn: bool,
}

#[derive(Debug, Deserialize)]
struct TranscribeResult {
    segments: Vec<Segment>,
}

/// A loaded whisper model. Construct with [`Whisper::from_model_bytes`],
/// then call [`Whisper::transcribe`] with 16 kHz mono f32 PCM.
///
/// Not `Clone`/`Sync`: the underlying glue holds one global context.
pub struct Whisper {
    // Marker only — the real state lives in the C module's globals. The
    // non-Send/Sync raw pointer keeps callers from sharing it across
    // threads, matching the single-global-context reality.
    _not_sync: std::marker::PhantomData<*const ()>,
}

impl Whisper {
    /// Load a ggml model from an in-memory buffer (the contents of a
    /// `ggml-*.bin` file). Returns `None` if the engine isn't available
    /// (wasm stub, or vendor absent) or the model failed to load.
    ///
    /// The Silero VAD model is left unset (the glue ignores it — VAD is
    /// the caller's job, matching the browser worker), so this loads the
    /// ASR model only.
    #[must_use]
    pub fn from_model_bytes(model: &[u8]) -> Option<Self> {
        // SAFETY: `model`/len describe a valid readable buffer for the
        // duration of the call; the glue copies what it needs into the
        // whisper context and does not retain the pointer. The VAD args
        // are (null, 0): no Silero (the glue stubs that path anyway).
        let ok = unsafe {
            ffi::wsp_init(
                model.as_ptr(),
                model.len() as std::os::raw::c_int,
                std::ptr::null(),
                0,
            )
        };
        (ok != 0).then_some(Self {
            _not_sync: std::marker::PhantomData,
        })
    }

    /// Transcribe 16 kHz mono f32 PCM. `prompt` seeds whisper's
    /// `initial_prompt` (pass `""` for none). Returns the decoded
    /// segments, or `None` on engine error.
    #[must_use]
    pub fn transcribe(&self, pcm_16k_mono: &[f32], prompt: &str) -> Option<Vec<Segment>> {
        if pcm_16k_mono.is_empty() {
            return Some(Vec::new());
        }
        let cprompt = std::ffi::CString::new(prompt).ok()?;
        // SAFETY: pointers are valid for the call. `wsp_transcribe`
        // returns a `malloc`'d, NUL-terminated buffer the caller owns;
        // we copy it into an owned String and `free` it (the browser
        // path frees via Module._free — same contract). On NULL the
        // engine failed; return None without freeing.
        let json = unsafe {
            let p = ffi::wsp_transcribe(
                pcm_16k_mono.as_ptr(),
                pcm_16k_mono.len() as std::os::raw::c_int,
                cprompt.as_ptr(),
            );
            if p.is_null() {
                return None;
            }
            let owned = std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned();
            ffi::wsp_free(p);
            owned
        };
        serde_json::from_str::<TranscribeResult>(&json)
            .ok()
            .map(|r| r.segments)
    }

    /// Whether the engine bundled a Silero VAD model. Currently always
    /// false (the glue stubs it); callers fall back to an energy gate.
    #[must_use]
    pub fn vad_available() -> bool {
        // SAFETY: no arguments, pure query.
        unsafe { ffi::wsp_vad_available() != 0 }
    }
}

// ─── FFI: native links the glue; wasm32 stubs it ────────────────────────

#[cfg(not(target_arch = "wasm32"))]
mod ffi {
    use std::os::raw::{c_char, c_int};
    extern "C" {
        pub fn wsp_init(
            model: *const u8,
            model_len: c_int,
            vad: *const u8,
            vad_len: c_int,
        ) -> c_int;
        pub fn wsp_transcribe(pcm: *const f32, n: c_int, prompt: *const c_char) -> *mut c_char;
        pub fn wsp_vad_available() -> c_int;
        pub fn wsp_free(p: *mut c_char);
    }
}

// On wasm32 the crate compiles but the engine is the browser's
// Emscripten module, not this code. Provide inert symbols so the safe
// wrapper above type-checks and reports "unavailable".
#[cfg(target_arch = "wasm32")]
mod ffi {
    use std::os::raw::{c_char, c_int};
    pub unsafe fn wsp_init(
        _model: *const u8,
        _model_len: c_int,
        _vad: *const u8,
        _vad_len: c_int,
    ) -> c_int {
        0
    }
    pub unsafe fn wsp_transcribe(
        _pcm: *const f32,
        _n: c_int,
        _prompt: *const c_char,
    ) -> *mut c_char {
        std::ptr::null_mut()
    }
    pub unsafe fn wsp_vad_available() -> c_int {
        0
    }
    pub unsafe fn wsp_free(_p: *mut c_char) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_glue_json_shape() {
        // The exact shape wsp_transcribe emits: t0/t1 are fractional
        // seconds, plus extra fields (speakerTurn/noSpeechProb/tokens/
        // avgLogprob) that we deliberately ignore.
        let json = r#"{"segments":[{"t0":0.00,"t1":1.50,"speakerTurn":false,"text":" hello","noSpeechProb":0.01,"tokens":[{"text":"hi","p":0.9}],"avgLogprob":-0.2}]}"#;
        let r: TranscribeResult = serde_json::from_str(json).unwrap();
        assert_eq!(r.segments.len(), 1);
        assert!((r.segments[0].t0 - 0.0).abs() < 1e-9);
        assert!((r.segments[0].t1 - 1.5).abs() < 1e-9);
        assert_eq!(r.segments[0].text.trim(), "hello");
    }

    #[test]
    fn empty_pcm_is_empty_not_error() {
        // No engine load needed: empty input short-circuits before FFI.
        let w = Whisper {
            _not_sync: std::marker::PhantomData,
        };
        assert_eq!(w.transcribe(&[], "").unwrap().len(), 0);
    }
}
