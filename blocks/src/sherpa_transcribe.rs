//! `SherpaTranscribe` — server-side speech-to-text via the sherpa-onnx
//! sidecar.
//!
//! The node-side counterpart to [`VoiceTranscribe`](crate::voice_transcribe)
//! (whisper). Same place in the chain (after NR, before `AudioSink`), same
//! pure-passthrough contract, same `events` → `ui:transcribe` output — but
//! instead of running whisper.cpp in-process it streams the tapped audio to
//! the sherpa-onnx sidecar (`tools/sherpa-asr/server.py`) over a WebSocket
//! and folds the returned segments into the *same*
//! [`ferrite_whisper::TranscriptState`] post-processor, so the cleaned
//! transcript shape on the wire is byte-for-byte what the UI already
//! consumes — no frontend change.
//!
//! **`Placement::NativeOnly`** — this is the heavy/server path only. The
//! browser keeps `VoiceTranscribe` (whisper WASM). The runtime injector
//! picks `SherpaTranscribe` when transcription is placed node-side.
//!
//! The sidecar owns VAD / endpointing / streaming, so this block is simpler
//! than `VoiceTranscribe`: tap → forward PCM → record `final` segments →
//! drain to the `events` port. Audio is never touched (passthrough / mute).

use anyhow::Result;
use serde::Deserialize;

use crate::block::{
    Block, BlockFactory, BlockIo, BlockSpec, InitCtx, InputPort, ParamKind, ParamSpec, Placement,
    PortSpec, PortType, ReconfigureScope, Work,
};
pub use crate::voice_transcribe::Mode;

const DEFAULT_BUFFER_SAMPLES: usize = 16_384;
const DEFAULT_BUFFER_SAMPLES_F64: f64 = 16_384.0;

/// Sidecar URL default; override with `FERRITE_SHERPA_ASR_URL`.
const DEFAULT_SIDECAR_URL: &str = "ws://127.0.0.1:10003";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SherpaTranscribeParams {
    /// `off` | `on` | `muted` — see [`Mode`].
    pub mode: String,
    /// Tap-ring capacity in samples (rounded to a power of two).
    pub buffer_samples: usize,
    /// Vocab-bias prompt seeded into the transcript post-processor
    /// (callsign/phonetic bias). Empty = the built-in ham corpus.
    pub prompt: String,
    /// Momentary: clear the rolling transcript + bias, and reset the
    /// sidecar's in-flight stream. Auto-clears.
    pub reset: bool,
}

impl Default for SherpaTranscribeParams {
    fn default() -> Self {
        Self {
            mode: "off".to_string(),
            buffer_samples: DEFAULT_BUFFER_SAMPLES,
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
    reconfig_scope: ReconfigureScope::SelfBlock,
    ai_notes: "Speech-to-text engagement (server-side, sherpa-onnx sidecar). 'off' = passthrough, zero cost. 'on' = audio plays AND is transcribed. 'muted' = transcribed only, audio silenced.",
};

const PROMPT_PARAM: ParamSpec = ParamSpec {
    key: "prompt",
    label: "Prompt",
    kind: ParamKind::Text { default: "" },
    reconfig_scope: ReconfigureScope::SelfBlock,
    ai_notes: "Vocab bias for the transcript post-processor (callsigns, NATO phonetics). Empty = built-in ham corpus.",
};

const RESET_PARAM: ParamSpec = ParamSpec {
    key: "reset",
    label: "Reset transcript",
    kind: ParamKind::Toggle { default: false },
    reconfig_scope: ReconfigureScope::SelfBlock,
    ai_notes: "Momentary: clear the rolling transcript + callsign bias and reset the sidecar stream. Auto-clears.",
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
    ai_notes: "Tap-ring capacity in samples forwarded to the sidecar. A few hundred ms at the audio rate; larger only helps if the link falls behind.",
};

// ─── Native worker: WS client to the sherpa-onnx sidecar ────────────────
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::io::ErrorKind;
    use std::net::TcpStream;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use ferrite_whisper::{RawSegment, TranscriptState};
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, WebSocket};

    use crate::spsc_ring::AudioRing;

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    pub struct Worker {
        pub tap: Arc<Mutex<AudioRing>>,
        pub transcript: Arc<Mutex<TranscriptState>>,
        pub dropped: Arc<AtomicU64>,
        stop: Arc<AtomicBool>,
        reset: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl Worker {
        pub fn spawn(src_rate_hz: f64, prompt_base: &str, tap_capacity: usize) -> Self {
            let tap = Arc::new(Mutex::new(AudioRing::new(tap_capacity)));
            let transcript = Arc::new(Mutex::new(TranscriptState::new()));
            if !prompt_base.is_empty() {
                transcript.lock().unwrap().set_prompt_base(prompt_base);
            }
            let dropped = Arc::new(AtomicU64::new(0));
            let stop = Arc::new(AtomicBool::new(false));
            let reset = Arc::new(AtomicBool::new(false));
            let url = std::env::var("FERRITE_SHERPA_ASR_URL")
                .unwrap_or_else(|_| super::DEFAULT_SIDECAR_URL.to_string());

            let handle = {
                let (tap, transcript, stop, reset) = (
                    Arc::clone(&tap),
                    Arc::clone(&transcript),
                    Arc::clone(&stop),
                    Arc::clone(&reset),
                );
                thread::Builder::new()
                    .name("sherpa-transcribe".into())
                    .spawn(move || run(&url, src_rate_hz, &tap, &transcript, &stop, &reset))
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

    /// Set the underlying TCP socket non-blocking so the poll loop can
    /// interleave sends and reads without stalling.
    fn set_nonblocking(sock: &mut WebSocket<MaybeTlsStream<TcpStream>>, nb: bool) {
        if let MaybeTlsStream::Plain(s) = sock.get_mut() {
            let _ = s.set_nonblocking(nb);
        }
    }

    fn is_would_block(e: &tungstenite::Error) -> bool {
        matches!(e, tungstenite::Error::Io(io) if io.kind() == ErrorKind::WouldBlock)
    }

    #[derive(serde::Deserialize)]
    struct SidecarSeg {
        text: String,
        #[serde(default, rename = "final")]
        is_final: bool,
        #[serde(default)]
        t0: f64,
        #[serde(default)]
        t1: f64,
    }

    fn run(
        url: &str,
        src_rate_hz: f64,
        tap: &Arc<Mutex<AudioRing>>,
        transcript: &Arc<Mutex<TranscriptState>>,
        stop: &Arc<AtomicBool>,
        reset: &Arc<AtomicBool>,
    ) {
        // Connect (blocking) + handshake the sample rate, then go non-blocking.
        let mut sock = match tungstenite::connect(url) {
            Ok((s, _)) => s,
            Err(e) => {
                tracing::warn!(target: "decoder::transcribe", error = %e, url, "sherpa sidecar unreachable; transcription disabled, audio unaffected");
                return;
            }
        };
        let hello = format!("{{\"sample_rate\":{}}}", src_rate_hz.round() as i64);
        if let Err(e) = sock.send(Message::Text(hello)) {
            tracing::warn!(target: "decoder::transcribe", error = %e, "sherpa sidecar handshake failed");
            return;
        }
        let _ = sock.flush();
        set_nonblocking(&mut sock, true);
        tracing::info!(target: "decoder::transcribe", url, rate = src_rate_hz, "sherpa transcriber connected");

        let mut scratch = vec![0.0_f32; 16_384];
        while !stop.load(Ordering::Relaxed) {
            if reset.swap(false, Ordering::Relaxed) {
                transcript.lock().unwrap().reset();
                let _ = sock.send(Message::Text("{\"reset\":true}".into()));
            }

            // Drain the tap → forward as little-endian f32 binary frames.
            loop {
                let n = tap.lock().unwrap().read(&mut scratch);
                if n == 0 {
                    break;
                }
                let mut bytes = Vec::with_capacity(n * 4);
                for &s in &scratch[..n] {
                    bytes.extend_from_slice(&s.to_le_bytes());
                }
                if let Err(e) = sock.write(Message::Binary(bytes)) {
                    if !is_would_block(&e) {
                        tracing::warn!(target: "decoder::transcribe", error = %e, "sherpa sidecar send failed; stopping");
                        return;
                    }
                }
                if n < scratch.len() {
                    break;
                }
            }
            let _ = sock.flush();

            // Drain available results.
            loop {
                match sock.read() {
                    Ok(Message::Text(txt)) => {
                        if let Ok(seg) = serde_json::from_str::<SidecarSeg>(&txt) {
                            if seg.is_final && !seg.text.trim().is_empty() {
                                let raw = RawSegment::new(now_ms(), seg.t0, seg.t1, &seg.text);
                                let _ = transcript.lock().unwrap().record(raw);
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        tracing::warn!(target: "decoder::transcribe", "sherpa sidecar closed");
                        return;
                    }
                    Ok(_) => {}
                    Err(e) if is_would_block(&e) => break,
                    Err(e) => {
                        tracing::warn!(target: "decoder::transcribe", error = %e, "sherpa sidecar read failed; stopping");
                        return;
                    }
                }
            }

            thread::sleep(Duration::from_millis(50));
        }
        let _ = sock.close(None);
    }
}

pub struct SherpaTranscribe {
    params: SherpaTranscribeParams,
    mode: Mode,
    input_rate_hz: f64,
    events_out: Vec<u8>,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<native::Worker>,
}

impl SherpaTranscribe {
    #[must_use]
    pub fn new(params: SherpaTranscribeParams) -> Self {
        let mode = Mode::parse(&params.mode);
        Self {
            mode,
            input_rate_hz: 0.0,
            events_out: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            worker: None,
            params,
        }
    }

    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

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
            tracing::info!(target: "decoder::transcribe", text = %entry.text, raw = %entry.raw_text);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn spawn_worker(&mut self) {
        if self.mode.taps() && self.worker.is_none() && self.input_rate_hz > 0.0 {
            self.worker = Some(native::Worker::spawn(
                self.input_rate_hz,
                &self.params.prompt,
                self.params.buffer_samples.next_power_of_two(),
            ));
        }
    }
}

#[ferrite_blocks_macros::ferrite_block]
impl Block for SherpaTranscribe {
    fn spec() -> BlockSpec {
        BlockSpec {
            type_name: "SherpaTranscribe",
            placement: Placement::NativeOnly,
            inputs: &[PortSpec {
                name: "in",
                port_type: PortType::RealF32,
            }],
            outputs: &[
                PortSpec {
                    name: "out",
                    port_type: PortType::RealF32,
                },
                PortSpec {
                    name: "events",
                    port_type: PortType::Events,
                },
            ],
            params: &[MODE_PARAM, PROMPT_PARAM, RESET_PARAM, BUFFER_SAMPLES_PARAM],
            ai_notes: "Server-side passthrough tap that transcribes demodulated voice via the sherpa-onnx sidecar (streaming, multilingual, VAD-endpointed). Node-only counterpart to VoiceTranscribe; emits the same transcript spots on 'events' → ui:transcribe. Audio passes through unchanged ('off'/'on') or silenced ('muted').",
        }
    }

    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()> {
        self.input_rate_hz = ctx.input_rate("in").unwrap_or(0.0);
        self.events_out.clear();
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.worker = None;
            self.spawn_worker();
        }
        Ok(())
    }

    fn apply_live_params(&mut self, delta: &serde_json::Value) -> Result<bool> {
        let mut changed = false;
        if let Some(m) = delta.get("mode").and_then(serde_json::Value::as_str) {
            self.params.mode = m.to_string();
            self.mode = Mode::parse(m);
            #[cfg(not(target_arch = "wasm32"))]
            {
                if self.mode.taps() {
                    self.spawn_worker();
                } else {
                    self.worker = None;
                }
            }
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
        let Some(out) = io
            .outputs
            .iter_mut()
            .find(|p| p.name == "out")
            .and_then(|p| p.as_real_f32_mut())
        else {
            return Ok(w);
        };
        let n = src.len().min(out.len());

        #[cfg(not(target_arch = "wasm32"))]
        if self.mode.taps() {
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

        if self.mode.audible() {
            out[..n].copy_from_slice(&src[..n]);
        } else {
            out[..n].fill(0.0);
        }
        w.consumed[0] = n;
        w.produced[0] = n;

        #[cfg(not(target_arch = "wasm32"))]
        self.drain_transcript();

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
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.worker = None;
        }
        Ok(())
    }
}

impl BlockFactory for SherpaTranscribe {
    fn construct(params: &serde_json::Value) -> Result<Box<dyn Block>> {
        let p: SherpaTranscribeParams = crate::block::deserialize_params(params)?;
        Ok(Box::new(SherpaTranscribe::new(p)))
    }
}
