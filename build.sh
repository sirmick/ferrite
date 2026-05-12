#!/usr/bin/env bash
# Build everything: ferrited (release) + WASM artifacts + web bundle.
# Sources soapysdr/env.sh if present so the local SoapySDR prefix is used.

set -euo pipefail
cd "$(dirname "$0")"

if [ -f soapysdr/env.sh ]; then
  # shellcheck disable=SC1091
  . soapysdr/env.sh
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
