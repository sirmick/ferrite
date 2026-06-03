ferrite-whisper — in-browser speech-to-text for the VoiceTranscribe block

whisper.cpp + Silero VAD compiled to WASM via emsdk, mirroring the
fldigi bridge. INERT until vendored + compiled: the app degrades
gracefully (audio passes through untouched; the transcript panel shows
"engine not built").

## Enable transcription

1. Vendor whisper.cpp at the pinned commit. Normally automatic —
   `scripts/bootstrap.sh` (also invoked by `./build.sh`) clones it at the
   PINNED commit below, or symlinks it from the primary worktree. By hand:

   git -C blocks/native/whisper clone --depth 1 \\
     https://github.com/ggml-org/whisper.cpp vendor/whisper.cpp

   Pin the commit here when you do:  PINNED: <fill-in-on-vendor>
   (keep scripts/bootstrap.sh's WHISPER_PIN in sync;
    glue targets the API around the VAD-params / no_speech_prob era;
    define WHISPER_HAS_NO_SPEECH_PROB if your commit has it.)

2. Install emsdk and `source emsdk_env.sh` (~/emsdk is present).

3. Fetch ggml models into web/static/models/ :
     ggml-small.en-q5_1.bin    default (best on noisy SSB)
     ggml-base.en-q5_1.bin     lighter, panel-selectable
     ggml-silero-v5.1.2.bin    VAD (optional, recommended)
     ggml-small.en-tdrz.bin    tinydiarize speaker-turn detection,
                               optional — pick from the model dropdown
   from https://huggingface.co/ggerganov/whisper.cpp (q5_1 quant).
   The tdrz model is unquantised (~465 MB) and English-only; it emits
   a per-segment speaker-turn flag the Transcript view renders as a
   divider. `tdrz_enable` is on unconditionally in the glue (no-op on
   non-tdrz models — swapping the .bin is the only switch).

4. pnpm wasm:build:whisper   (wired into pnpm wasm:build)

## Why browser-side

COOP/COEP are already set app-wide (the audio SAB ring needs cross-
origin isolation) so the threaded+SIMD whisper build runs with no
extra setup. Inference is off the audio + main threads (one Worker
per VoiceTranscribe block). Accuracy-over-latency: beam search, temp
fallback, rolling ham initial_prompt, deterministic phonetic→callsign
post-pass (web/src/lib/transcribe/hamPostProcess.ts).

PINNED: v1.8.6 (ggml-org/whisper.cpp release tag; link-tested 2026-06-03).
  Was bare master commit 968eebe7 (2026-05-15) — just tip-of-master on the
  vendor day, not a curated commit. Switched to the release tag: the glue
  only needs whisper_full_params.{vad,vad_model_path,tdrz_enable,
  no_speech_thold} + whisper_full_get_segment_no_speech_prob, all stable
  across the v1.7→v1.8 line. Bump to the newest release that still has them;
  rerun scripts/bootstrap.sh (delete vendor/ first) and confirm ferrited links.
