#!/usr/bin/env bash
# Build everything: ferrited (release) + WASM artifacts + web bundle.
# Sources soapysdr/env.sh if present so the local SoapySDR prefix is used.

set -euo pipefail
cd "$(dirname "$0")"

if [ -f soapysdr/env.sh ]; then
  # shellcheck disable=SC1091
  . soapysdr/env.sh
fi

# Put emsdk on PATH if installed but not already sourced — otherwise
# wasm:build:{fldigi,whisper} silently no-op and the subsequent vite
# build hard-fails on the missing `./fldigi.mjs` dynamic import.
# Honors $EMSDK, then the conventional ~/emsdk checkout.
if ! command -v emcc >/dev/null 2>&1; then
  for _emsdk_env in "${EMSDK:-}/emsdk_env.sh" "$HOME/emsdk/emsdk_env.sh"; do
    if [ -f "$_emsdk_env" ]; then
      # shellcheck disable=SC1090
      . "$_emsdk_env" >/dev/null 2>&1
      break
    fi
  done
fi
if ! command -v emcc >/dev/null 2>&1; then
  echo "warning: emcc not found — fldigi/whisper wasm modules won't" >&2
  echo "  build and 'vite build' will fail on the missing imports." >&2
  echo "  Install emsdk (https://emscripten.org) or set \$EMSDK." >&2
fi

if [ "${BUILD_FORCE:-0}" != "1" ] && pgrep -x ferrited >/dev/null 2>&1; then
  echo "ferrited is still running — it's probably holding your SDR." >&2
  echo "Stop it first:  ./stop.sh" >&2
  echo "Or override:    BUILD_FORCE=1 ./build.sh" >&2
  exit 1
fi

echo "==> cargo build --release (workspace)"
cargo build --release

echo "==> pnpm install"
pnpm install --frozen-lockfile

echo "==> pnpm build (wasm + vite)"
pnpm --filter @ferrite/web build

echo
echo "==> done"
echo "    binary:    target/release/ferrited"
echo "    web dist:  web/build/"
echo "    wasm pkgs: web/src/lib/wasm/{blocks,runtime}/"
