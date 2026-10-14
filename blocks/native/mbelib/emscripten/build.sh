#!/usr/bin/env bash
# Build the browser vocoder: vendored mbelib + shim → a small sibling
# Emscripten module, web/src/lib/wasm/vocoder/mbe.{mjs,wasm}. The browser
# runner imports it lazily (mbeBridge.ts) only when a flowgraph decodes
# voice browser-side. Skips cleanly without emsdk; native decode is
# unaffected either way.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
crate="$(cd "$here/.." && pwd)"
out_dir="${1:-$crate/../../../web/src/lib/wasm/vocoder}"
if ! command -v emcc >/dev/null 2>&1; then
	echo "mbelib/emscripten: emcc not found — skipping the browser vocoder."
	exit 0
fi
mkdir -p "$out_dir"
exports='["_ferrite_mbe_create","_ferrite_mbe_decode_ambe2450","_ferrite_mbe_destroy","_malloc","_free"]'
set -x
emcc "$crate"/vendor/{mbelib,ecc,ambe3600x2400,ambe3600x2450,imbe7200x4400,imbe7100x4400}.c \
	"$crate/shim/mbe_shim.c" \
	-O2 -I"$crate/vendor" -w \
	-sMODULARIZE=1 -sEXPORT_ES6=1 -sENVIRONMENT=worker,node,web \
	-sALLOW_MEMORY_GROWTH=1 -sFILESYSTEM=0 \
	-sEXPORTED_FUNCTIONS="$exports" \
	-sEXPORTED_RUNTIME_METHODS='["HEAPU8","HEAPF32"]' \
	-sEXPORT_NAME=createMbeModule \
	-o "$out_dir/mbe.mjs"
set +x
printf '// @ts-nocheck — generated Emscripten module\n%s' "$(cat "$out_dir/mbe.mjs")" > "$out_dir/mbe.mjs"
echo "mbelib/emscripten: wrote $out_dir/mbe.{mjs,wasm}"
