ferrite-whisper — in-browser speech-to-text for the VoiceTranscribe block

whisper.cpp + Silero VAD compiled to WASM via emsdk, mirroring the
fldigi bridge. INERT until vendored + compiled: the app degrades
gracefully (audio passes through untouched; the transcript panel shows
"engine not built").

## Enable transcription

1. Vendor whisper.cpp at a pinned commit:

   git -C blocks/native/whisper clone --depth 1 \\
     https://github.com/ggml-org/whisper.cpp vendor/whisper.cpp

   Pin the commit here when you do:  PINNED: <fill-in-on-vendor>
   (glue targets the API around the VAD-params / no_speech_prob era;
    define WHISPER_HAS_NO_SPEECH_PROB if your commit has it.)

2. Install emsdk and `source emsdk_env.sh` (~/emsdk is present).

3. Fetch ggml models into web/static/models/ :
     ggml-small.en-q5_1.bin   default (best on noisy SSB)
     ggml-base.en-q5_1.bin    lighter, panel-selectable
     ggml-silero-v5.1.2.bin   VAD (optional, recommended)
   from https://huggingface.co/ggerganov/whisper.cpp (q5_1 quant).

4. pnpm wasm:build:whisper   (wired into pnpm wasm:build)

## Why browser-side

COOP/COEP are already set app-wide (the audio SAB ring needs cross-
origin isolation) so the threaded+SIMD whisper build runs with no
extra setup. Inference is off the audio + main threads (one Worker
per VoiceTranscribe block). Accuracy-over-latency: beam search, temp
fallback, rolling ham initial_prompt, deterministic phonetic→callsign
post-pass (web/src/lib/transcribe/hamPostProcess.ts).

PINNED: 968eebe77225d25e57a3f981da7c696310f0e881 (ggml-org/whisper.cpp, vendored 2026-05-17)
