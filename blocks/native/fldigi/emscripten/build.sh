#!/usr/bin/env bash
# Emscripten build of the curated fldigi cores → sibling fldigi.wasm.
#
# The link-vs-bridge wasm path (see the approved plan / build.rs): the
# Rust blocks/runtime wasm (wasm-bindgen, wasm32-unknown-unknown) does
# NOT contain fldigi — `build.rs` compiles nothing for wasm32, leaving
# the fldigi C ABI as undefined wasm imports. This script builds the
# *same* curated C++ (shim/ + vendor/) as a standalone Emscripten
# module that exports that ABI. JS thunks (web/src/lib/wasm/fldigi/
# fldigiBridge.ts) marshal calls + buffers between the two modules'
# linear memories. Emscripten gives full libc++ + exceptions for free.
#
# Inert until the toolchain exists: if `emcc` is not on PATH this exits
# 0 with a notice so `pnpm wasm:build` never breaks. Install emsdk
# (https://emscripten.org) deliberately in CI / a dev box, not silently.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
crate="$(cd "$here/.." && pwd)"
out_dir="${1:-$crate/../../../web/src/lib/wasm/fldigi}"

if ! command -v emcc >/dev/null 2>&1; then
	echo "fldigi/emscripten: emcc not found — skipping wasm bridge build."
	echo "  (native fldigi decode is unaffected; install emsdk to enable"
	echo "   the in-browser bridge, then re-run pnpm wasm:build.)"
	exit 0
fi

mkdir -p "$out_dir"

# Same source set + include order as build.rs (shim first so the
# replacement headers win), minus the wasm-skip branch.
#
# INCLUDE_FRAGMENTS: a few vendored .cxx are include fragments, not
# standalone TUs (e.g. rsid_defs.cxx holds the RSID tables and is
# `#include`d into rsid.cxx). build.rs skips these by name; mirror that
# exact list here or the standalone compile fails (undeclared cRsId).
INCLUDE_FRAGMENTS=(rsid_defs.cxx)
srcs=()
while IFS= read -r f; do
	skip=
	for frag in "${INCLUDE_FRAGMENTS[@]}"; do
		[[ "$(basename "$f")" == "$frag" ]] && skip=1 && break
	done
	[[ -n "$skip" ]] || srcs+=("$f")
done < <(
	find "$crate/shim" "$crate/vendor" -maxdepth 1 \
		\( -name '*.cxx' -o -name '*.cpp' -o -name '*.cc' -o -name '*.c' \) | sort
)

# The pull/drain C ABI (src/lib.rs) + the libc allocator the JS thunks
# need to stage buffers in the Emscripten heap.
exports='["_fldigi_modem_create","_fldigi_modem_rx","_fldigi_modem_set_param","_fldigi_modem_drain_text","_fldigi_modem_drain_status","_fldigi_modem_drain_scope","_fldigi_modem_drain_image","_fldigi_modem_destroy","_malloc","_free"]'
rt_methods='["HEAPU8","HEAPF32","HEAP32","stringToUTF8","UTF8ToString","lengthBytesUTF8"]'

set -x
emcc "${srcs[@]}" \
	-O2 -std=c++17 -fexceptions \
	-I"$crate/shim" -I"$crate/vendor" \
	-Wno-deprecated -w \
	-sMODULARIZE=1 -sEXPORT_ES6=1 -sENVIRONMENT=worker,node \
	-sALLOW_MEMORY_GROWTH=1 -sFILESYSTEM=0 \
	-sEXPORTED_FUNCTIONS="$exports" \
	-sEXPORTED_RUNTIME_METHODS="$rt_methods" \
	-sEXPORT_NAME=createFldigiModule \
	-o "$out_dir/fldigi.mjs"
set +x

# Emscripten's glue .mjs is generated, minified, untyped boilerplate
# with no .d.ts. The repo typechecks JS (checkJs), so mark it opt-out
# (durable: re-applied on every rebuild here, not editable-and-lost).
printf '// @ts-nocheck — generated Emscripten module\n%s' "$(cat "$out_dir/fldigi.mjs")" > "$out_dir/fldigi.mjs"

echo "fldigi/emscripten: wrote $out_dir/fldigi.{mjs,wasm} (+ @ts-nocheck)"
