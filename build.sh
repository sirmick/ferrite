#!/usr/bin/env bash
# Build everything: ferrited (release) + WASM artifacts + web bundle.
# Sources soapysdr/env.sh if present so the local SoapySDR prefix is used.

set -euo pipefail
cd "$(dirname "$0")"

if [ -f soapysdr/env.sh ]; then
  # shellcheck disable=SC1091
  . soapysdr/env.sh
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
