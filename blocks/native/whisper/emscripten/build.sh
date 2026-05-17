#!/usr/bin/env bash
# Emscripten build of whisper.cpp + Silero VAD + our thin C glue →
# web/src/lib/wasm/whisper/whisper.{mjs,wasm}.
#
# Same stance as blocks/native/fldigi/emscripten/build.sh: INERT until
# the toolchain *and* the vendored sources exist. If `emcc` is missing
# or whisper.cpp hasn't been vendored, this prints a notice and exits 0
# so `pnpm wasm:build` never breaks and the app degrades gracefully
# (the VoiceTranscribe block is pure passthrough — audio is unaffected;
# the transcript panel shows "engine not built").
#
# To enable transcription, deliberately (in CI or a dev box):
#
#   1. Vendor whisper.cpp at a pinned commit:
#        git -C blocks/native/whisper clone --depth 1 \
#          https://github.com/ggml-org/whisper.cpp vendor/whisper.cpp
#        (or add it as a submodule; pin the commit in README.md)
#   2. Install emsdk (https://emscripten.org) and `source emsdk_env.sh`.
#   3. Fetch the ggml models into web/static/models/ (see README.md):
#        ggml-small.en-q5_1.bin   (default)
#        ggml-base.en-q5_1.bin    (lighter, selectable)
#        ggml-silero-v5.1.2.bin   (VAD; optional but recommended)
#   4. pnpm wasm:build:whisper
#
# COOP/COEP are already set app-wide (the audio SAB ring needs cross-
# origin isolation), so the threaded+SIMD build runs without extra
# headers — the usual #1 blocker for in-browser whisper is already
# solved here.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
crate="$(cd "$here/.." && pwd)"                 # blocks/native/whisper
out_dir="${1:-$crate/../../../web/src/lib/wasm/whisper}"
vendor="$crate/vendor/whisper.cpp"

if ! command -v emcc >/dev/null 2>&1; then
	echo "whisper/emscripten: emcc not found — skipping wasm build."
	echo "  (audio is unaffected; install emsdk + vendor whisper.cpp,"
	echo "   then re-run pnpm wasm:build to enable transcription.)"
	exit 0
fi

if [[ ! -d "$vendor" || ! -f "$vendor/include/whisper.h" ]]; then
	echo "whisper/emscripten: whisper.cpp not vendored at"
	echo "  $vendor — skipping (see README.md for the clone command)."
	exit 0
fi

mkdir -p "$out_dir"

# whisper.cpp + ggml core. The repo layout moved ggml under ggml/src;
# glob defensively so a submodule bump doesn't silently drop a TU.
# Let whisper.cpp's own CMake build the static libs: its `if
# (EMSCRIPTEN)` block applies the required -pthread and, crucially,
# selects *only* the ggml-cpu backend (a hand-rolled source glob would
# drag in the CUDA/Vulkan/Metal/etc. backend dirs that don't compile
# under emcc). We then link our thin glue against libwhisper + libggml*.
bld="$crate/build-em"
emcmake cmake -S "$vendor" -B "$bld" \
	-DCMAKE_BUILD_TYPE=Release \
	-DBUILD_SHARED_LIBS=OFF \
	-DWHISPER_BUILD_EXAMPLES=OFF \
	-DWHISPER_BUILD_TESTS=OFF \
	-DWHISPER_BUILD_SERVER=OFF \
	-DGGML_OPENMP=OFF
emmake make -C "$bld" whisper -j"$(nproc)"

# Static archives (order matters for the final static link: whisper
# depends on ggml, ggml on ggml-cpu/ggml-base). Glob so a layout bump
# doesn't hard-break the path.
libs=()
while IFS= read -r a; do libs+=("$a"); done < <(
	find "$bld" -name 'libwhisper.a' -o -name 'libggml*.a' | sort
)
if [[ ${#libs[@]} -eq 0 ]]; then
	echo "whisper/emscripten: cmake build produced no static libs — aborting." >&2
	exit 1
fi

exports='["_wsp_init","_wsp_transcribe","_wsp_vad_available","_malloc","_free"]'
rt_methods='["HEAPU8","HEAPF32","stringToUTF8","UTF8ToString","lengthBytesUTF8"]'

set -x
emcc "$crate/shim/whisper_glue.c" "${libs[@]}" \
	-O3 -fexceptions -pthread \
	-DWHISPER_HAS_NO_SPEECH_PROB -DNDEBUG \
	-I"$vendor/include" -I"$vendor/ggml/include" -I"$crate/shim" \
	-Wno-deprecated -w \
	-sMODULARIZE=1 -sEXPORT_ES6=1 -sENVIRONMENT=worker,node \
	-sALLOW_MEMORY_GROWTH=1 -sMAXIMUM_MEMORY=2147483648 \
	-sFILESYSTEM=0 -sINITIAL_MEMORY=536870912 \
	-sPTHREAD_POOL_SIZE=4 -sWASM_BIGINT \
	-sEXPORTED_FUNCTIONS="$exports" \
	-sEXPORTED_RUNTIME_METHODS="$rt_methods" \
	-sEXPORT_NAME=createWhisperModule \
	-o "$out_dir/whisper.mjs"
set +x

# Generated Emscripten glue is minified/untyped; the repo typechecks JS
# (checkJs). Durable opt-out, re-applied on every rebuild (same as the
# fldigi build).
printf '// @ts-nocheck — generated Emscripten module\n%s' \
	"$(cat "$out_dir/whisper.mjs")" > "$out_dir/whisper.mjs"

echo "whisper/emscripten: wrote $out_dir/whisper.{mjs,wasm} (+ @ts-nocheck)"
