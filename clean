#!/usr/bin/env bash
# Remove build artifacts so the next ./build.sh is from-scratch.
#
# Default: everything ./build.sh and ./package.sh produce — cargo
# target/, the web bundle + .svelte-kit, the generated Emscripten wasm
# modules, and the packaging staging/output. Tracked files (incl. the
# committed wasm-pack output for blocks/runtime, which build.sh
# overwrites in place) are never touched.
#
#   ./clean.sh           # build outputs only (fast to rebuild)
#   ./clean.sh --deep     # also node_modules + vendored whisper + models
#
# Never removes soapysdr/ — that's a slow rebuild owned by
# scripts/build-soapysdr.sh, not this script.

set -euo pipefail
cd "$(dirname "$0")"

DEEP=0
case "${1:-}" in
  --deep) DEEP=1 ;;
  "")     ;;
  *) echo "usage: $0 [--deep]" >&2; exit 2 ;;
esac

echo "==> cargo clean (target/)"
cargo clean

echo "==> web bundle + svelte-kit cache"
rm -rf web/build web/.svelte-kit

echo "==> generated Emscripten wasm modules"
rm -f web/src/lib/wasm/fldigi/fldigi.mjs \
      web/src/lib/wasm/fldigi/fldigi.wasm \
      web/src/lib/wasm/whisper/whisper.mjs \
      web/src/lib/wasm/whisper/whisper.wasm

echo "==> packaging staging + output"
rm -rf dist
rm -f packaging/build-ctx/*.tar.xz packaging/build-ctx/*.spec

echo "==> dev-server logs"
rm -f run.log dev-server.log web/dev-server.log

if [ "$DEEP" -eq 1 ]; then
  echo "==> [deep] node_modules"
  rm -rf web/node_modules tools/ferrite-ai/node_modules tools/screenshot/node_modules
  echo "==> [deep] vendored whisper.cpp + em build tree + ggml models"
  rm -rf blocks/native/whisper/vendor blocks/native/whisper/build-em web/static/models
fi

echo
echo "==> done — run ./build.sh to rebuild"
