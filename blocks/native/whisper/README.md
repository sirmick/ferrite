ferrite-whisper — speech-to-text core + the node-side whisper.cpp engine

Two things live here:

1. **The shared transcription core** (`src/`): resampling to 16 kHz, VAD
   segmentation with a hard `max_segment_ms` cut, the pending-clip queue,
   and the ham post-processor (phonetics → callsigns, Q-codes, RST). Pure
   Rust, compiled both natively and to wasm32 — the browser's
   `WasmTranscriber` and the node-side `VoiceTranscribe` block run the
   *same* code, so both sides segment and clean identically. The browser
   feeds its clips to the sherpa-onnx engine (blocks/native/sherpa).

2. **The native whisper.cpp engine** (`build.rs` + `shim/whisper_glue.c`):
   the heavy-tier STT that `VoiceTranscribe` runs inside `ferrited`
   (profile `audio_split=server`, or any node-placed tap). whisper.cpp is
   built by `build.rs` from the vendored source; without it the crate
   still compiles (FFI stub) but `ferrited` won't link.

The former in-browser whisper.cpp Emscripten build is gone — sherpa-onnx
replaced it (same model as the node sidecar in tools/sherpa-asr).

## Native engine setup

1. Vendor whisper.cpp at the pinned tag. Normally automatic —
   `scripts/bootstrap.sh` (also invoked by `./build.sh`) clones it at the
   PINNED tag below, or symlinks it from the primary worktree. By hand:

   git -C blocks/native/whisper clone --depth 1 --branch v1.8.6 \\
     https://github.com/ggml-org/whisper.cpp vendor/whisper.cpp

   (keep scripts/bootstrap.sh's WHISPER_PIN in sync; the glue targets
    the API around the VAD-params / no_speech_prob era.)

2. ggml models for the node-side block are located by `model_id` via
   `locate_model` in blocks/src/voice_transcribe.rs
   (`ggml-small.en-q5_1.bin` default; q5_1 quants from
   https://huggingface.co/ggerganov/whisper.cpp).

PINNED: v1.8.6 (ggml-org/whisper.cpp release tag; link-tested 2026-06-03).
  Was bare master commit 968eebe7 (2026-05-15) — just tip-of-master on the
  vendor day, not a curated commit. Switched to the release tag: the glue
  only needs whisper_full_params.{vad,vad_model_path,tdrz_enable,
  no_speech_thold} + whisper_full_get_segment_no_speech_prob, all stable
  across the v1.7→v1.8 line. Bump to the newest release that still has them;
  rerun scripts/bootstrap.sh (delete vendor/ first) and confirm ferrited links.
