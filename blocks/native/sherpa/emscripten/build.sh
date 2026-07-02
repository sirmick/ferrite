#!/usr/bin/env bash
# Emscripten build of sherpa-onnx streaming ASR → web/src/lib/wasm/sherpa/.
#
# This is the *light-tier* (browser) speech-to-text engine — the same
# streaming Zipformer transducer the server sidecar runs, so browser and
# server transcription behave identically. It replaces the in-browser
# whisper.cpp build (blocks/native/whisper/emscripten/build.sh).
#
# Same stance as the whisper/fldigi builds: INERT until the toolchain
# *and* the vendored source exist. If `emcc` is missing or sherpa-onnx
# hasn't been vendored, this prints a notice and exits 0 so
# `pnpm wasm:build` never breaks and the app degrades gracefully (the
# VoiceTranscribe tap is pure passthrough — audio is unaffected; the
# transcript panel shows "engine not built").
#
# To enable in-browser transcription, deliberately (CI or a dev box):
#   1. Vendor sherpa-onnx at a pinned tag (scripts/bootstrap.sh does this):
#        git clone --depth 1 --branch v1.13.2 \
#          https://github.com/k2-fsa/sherpa-onnx \
#          blocks/native/sherpa/vendor/sherpa-onnx
#   2. Install emsdk and `source emsdk_env.sh` (4.0.23 is sherpa's known-
#      good; newer may work — we try whatever `emcc` is on PATH).
#   3. Provide the streaming-zipformer-en-20M model (tools/sherpa-asr/
#      setup.sh already downloads it; this script reuses that copy).
#   4. pnpm wasm:build:sherpa
#
# COOP/COEP are set app-wide already, so even a threaded build is fine —
# but sherpa's WASM ASR runs single-threaded, no SharedArrayBuffer needed.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
crate="$(cd "$here/.." && pwd)"                 # blocks/native/sherpa
root="$(cd "$crate/../../.." && pwd)"           # repo root
out_dir="${1:-$root/web/src/lib/wasm/sherpa}"
vendor="$crate/vendor/sherpa-onnx"
# The streaming Zipformer EN-20M model, shared with the server sidecar.
model_dir="${SHERPA_WASM_MODEL_DIR:-$root/tools/sherpa-asr/models/sherpa-onnx-streaming-zipformer-en-20M-2023-02-17}"

if ! command -v emcc >/dev/null 2>&1; then
	echo "sherpa/emscripten: emcc not found — skipping wasm build."
	echo "  (audio is unaffected; install emsdk + vendor sherpa-onnx,"
	echo "   then re-run pnpm wasm:build to enable transcription.)"
	exit 0
fi

if [[ ! -d "$vendor" || ! -f "$vendor/build-wasm-simd-asr.sh" ]]; then
	echo "sherpa/emscripten: sherpa-onnx not vendored at"
	echo "  $vendor — skipping (scripts/bootstrap.sh clones it)."
	exit 0
fi

# emsdk exports EMSCRIPTEN only sometimes; derive it from emcc so sherpa's
# CMake toolchain-file path resolves regardless of how emsdk was sourced.
EMSCRIPTEN="${EMSCRIPTEN:-$(dirname "$(realpath "$(command -v emcc)")")}"
export EMSCRIPTEN
if [[ ! -f "$EMSCRIPTEN/cmake/Modules/Platform/Emscripten.cmake" ]]; then
	echo "sherpa/emscripten: Emscripten.cmake not found under $EMSCRIPTEN — skipping." >&2
	exit 0
fi

# --- Stage the model into sherpa's preloaded asset dir ------------------
# sherpa's WASM ASR build bakes whatever is in wasm/asr/assets into the
# .data file (Emscripten --preload-file) and the recognizer expects the
# canonical names encoder.onnx / decoder.onnx / joiner.onnx / tokens.txt.
# We use the int8 variants (the .data stays ~43 MB, on par with whisper
# tiny). It's not an error that we rename *.int8.onnx → *.onnx.
assets="$vendor/wasm/asr/assets"
if [[ ! -f "$model_dir/encoder-epoch-99-avg-1.int8.onnx" ]]; then
	echo "sherpa/emscripten: model not found under $model_dir — skipping." >&2
	echo "  (run tools/sherpa-asr/setup.sh to fetch the EN-20M model.)" >&2
	exit 0
fi
mkdir -p "$assets"
cp -f "$model_dir/encoder-epoch-99-avg-1.int8.onnx" "$assets/encoder.onnx"
cp -f "$model_dir/decoder-epoch-99-avg-1.int8.onnx" "$assets/decoder.onnx"
cp -f "$model_dir/joiner-epoch-99-avg-1.int8.onnx" "$assets/joiner.onnx"
cp -f "$model_dir/tokens.txt" "$assets/tokens.txt"

# --- Build (mirrors vendor/build-wasm-simd-asr.sh, parallelised) --------
bld="$vendor/build-wasm-simd-asr"
mkdir -p "$bld"
# Stock sherpa emits a classic global-`Module` script (index.html loads it
# with <script>). Our transcription Worker imports an ES module instead, so
# layer MODULARIZE/EXPORT_ES6 on top of sherpa's own link flags (passed via
# the linker-flags cache var — sherpa appends its set after, and MODULARIZE
# doesn't collide). Result: `import createSherpaAsrModule from
# './sherpa-onnx-wasm-main-asr.js'` → a factory returning the Module.
# `node` in ENVIRONMENT lets the golden e2e (web/src/lib/transcribe/
# sherpaGoldenE2E.test.ts) run the module in a real Node process — same
# stance as the whisper build — while web,worker keep the browser path.
sherpa_link_flags="-sMODULARIZE=1 -sEXPORT_ES6=1 -sEXPORT_NAME=createSherpaAsrModule -sENVIRONMENT=web,worker,node"
cmake \
	-S "$vendor" -B "$bld" \
	-DCMAKE_INSTALL_PREFIX="$bld/install" \
	-DCMAKE_BUILD_TYPE=Release \
	-DCMAKE_EXE_LINKER_FLAGS="$sherpa_link_flags" \
	-DCMAKE_TOOLCHAIN_FILE="$EMSCRIPTEN/cmake/Modules/Platform/Emscripten.cmake" \
	-DSHERPA_ONNX_ENABLE_PYTHON=OFF \
	-DSHERPA_ONNX_ENABLE_TESTS=OFF \
	-DSHERPA_ONNX_ENABLE_CHECK=OFF \
	-DBUILD_SHARED_LIBS=OFF \
	-DSHERPA_ONNX_ENABLE_PORTAUDIO=OFF \
	-DSHERPA_ONNX_ENABLE_JNI=OFF \
	-DSHERPA_ONNX_ENABLE_C_API=ON \
	-DSHERPA_ONNX_ENABLE_TTS=OFF \
	-DSHERPA_ONNX_ENABLE_WEBSOCKET=OFF \
	-DSHERPA_ONNX_ENABLE_GPU=OFF \
	-DSHERPA_ONNX_ENABLE_WASM=ON \
	-DSHERPA_ONNX_ENABLE_WASM_ASR=ON \
	-DSHERPA_ONNX_ENABLE_BINARY=OFF \
	-DSHERPA_ONNX_LINK_LIBSTDCPP_STATICALLY=OFF
SHERPA_ONNX_IS_USING_BUILD_WASM_SH=ON make -C "$bld" -j"$(nproc)"
make -C "$bld" install

# --- Copy the web-loadable artifacts ------------------------------------
src="$bld/install/bin/wasm/asr"
if [[ ! -f "$src/sherpa-onnx-wasm-main-asr.js" ]]; then
	echo "sherpa/emscripten: build produced no wasm artifacts under $src — aborting." >&2
	exit 1
fi
mkdir -p "$out_dir"
# The Emscripten module (.js/.wasm/.data) + the JS API wrapper. We skip
# the demo app-asr.js / index.html — our own worker drives the API.
cp -f "$src/sherpa-onnx-wasm-main-asr.js" "$out_dir/"
cp -f "$src/sherpa-onnx-wasm-main-asr.wasm" "$out_dir/"
cp -f "$src/sherpa-onnx-wasm-main-asr.data" "$out_dir/"
cp -f "$src/sherpa-onnx-asr.js" "$out_dir/"

# The generated Emscripten glue is minified/untyped; the repo typechecks
# JS (checkJs). Durable opt-out, re-applied each build (same as whisper).
for f in sherpa-onnx-wasm-main-asr.js sherpa-onnx-asr.js; do
	printf '// @ts-nocheck — generated/vendored sherpa-onnx wasm glue\n%s' \
		"$(cat "$out_dir/$f")" > "$out_dir/$f"
done

# `sherpa-onnx-asr.js` exports its API via a node-guarded `module.exports`.
# That guard (`process.versions.node`) is TRUE under Node ESM too, where
# `module` is undefined → ReferenceError (hit by the golden e2e + any ESM
# import in node). Tighten the guard with a `typeof module` check so the
# CJS block only runs in real CommonJS; the browser path was already inert.
sed -i \
	"s/typeof process.versions.node == 'string') {/typeof process.versions.node == 'string' \&\& typeof module !== 'undefined') {/" \
	"$out_dir/sherpa-onnx-asr.js"
# Our Worker imports it as an ES module, so append a real named export.
# `createOnlineRecognizer` is a top-level fn; additive + idempotent.
if ! grep -q '^export { createOnlineRecognizer }' "$out_dir/sherpa-onnx-asr.js"; then
	printf '\nexport { createOnlineRecognizer };\n' >> "$out_dir/sherpa-onnx-asr.js"
fi

echo "sherpa/emscripten: wrote $out_dir/{sherpa-onnx-wasm-main-asr.{js,wasm,data},sherpa-onnx-asr.js}"
ls -lh "$out_dir"
