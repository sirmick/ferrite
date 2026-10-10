ferrite sherpa — in-browser speech-to-text for the VoiceTranscribe block

sherpa-onnx's streaming Zipformer transducer (EN-20M, int8) compiled to
WASM via emsdk — the *light tier* engine that replaced whisper.cpp in the
browser (same model the node sidecar in tools/sherpa-asr runs, so browser
and server transcription behave identically). INERT until vendored +
compiled: the app degrades gracefully (audio passes through untouched;
the transcript panel shows "engine not built").

## Enable transcription

1. Vendor sherpa-onnx + fetch the model. Normally automatic —
   `scripts/bootstrap.sh` (also invoked by `./build.sh`) clones the source
   at the PINNED tag below into `vendor/sherpa-onnx` and downloads the
   model tarball into `tools/sherpa-asr/models/`, or symlinks both from
   the primary worktree. By hand:

       git clone --depth 1 --branch v1.13.2 \
         https://github.com/k2-fsa/sherpa-onnx \
         blocks/native/sherpa/vendor/sherpa-onnx
       tools/sherpa-asr/setup.sh      # also fetches the model

   PINNED: v1.13.2  (keep scripts/bootstrap.sh's SHERPA_PIN in sync)

2. Install emsdk and `source emsdk_env.sh` (5.0.7 is what we build with;
   emscripten/build.sh uses whatever `emcc` is on PATH).

3. `pnpm wasm:build:sherpa` (or the umbrella `pnpm wasm:build`). Takes
   ~3 min on a fast box — onnxruntime arrives prebuilt, only sherpa itself
   compiles. Output lands in `web/src/lib/wasm/sherpa/` (gitignored):

       sherpa-onnx-wasm-main-asr.{js,wasm,data}   Emscripten module; the
                                                  .data bakes the model
       sherpa-onnx-asr.js                         JS API glue (patched for
                                                  ESM, see build.sh)

   `web/src/lib/wasm/sherpa/sherpa-onnx.d.ts` is hand-written and tracked.

## How it's loaded

`web/src/lib/transcribe/sherpaEngine.ts` pulls the artifacts in via
`import.meta.glob`, never a literal import — the glob is `{}` when the
build output is absent, so `vite build` succeeds on a tree without emsdk.
The transcription Worker turns that into `EngineUnavailableError`.

## Gates

`web/src/lib/transcribe/sherpaGoldenE2E.test.ts` runs the real module in
Node against a golden clip when the artifacts exist, and skips otherwise.
Treat a skip on a dev box as "not provisioned", not as green.
