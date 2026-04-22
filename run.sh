#!/usr/bin/env bash
# Dev run: ferrited + vite dev, both bound to 0.0.0.0.
# Logs from both processes are combined onto this script's stdout/stderr,
# prefixed with [ferrited] / [web]. Ctrl-C stops both.
#
# No flowgraph or source is started — pick those from the UI:
#   1. open http://<host>:5173/
#   2. choose a preset from the header dropdown
#   3. configure the source via the Source dialog
#   4. click Start

set -uo pipefail
cd "$(dirname "$0")"

if [ -f soapysdr/env.sh ]; then
  # shellcheck disable=SC1091
  . soapysdr/env.sh
fi

# ferrited requires --flowgraph; pass any preset as a placeholder. The
# pipeline isn't started until the UI presses Start, and the user can
# swap presets first via POST /api/preset (the header dropdown).
PLACEHOLDER_FLOWGRAPH="${FERRITE_FLOWGRAPH:-flowgraphs/wbfm.json}"

if [ ! -x target/release/ferrited ]; then
  echo "ferrited binary missing at target/release/ferrited" >&2
  echo "run ./build.sh first" >&2
  exit 1
fi

if [ ! -f web/src/lib/wasm/runtime/runtime_bg.wasm ] \
   || [ ! -f web/src/lib/wasm/blocks/ferrite_blocks_bg.wasm ]; then
  echo "WASM artifacts missing under web/src/lib/wasm/" >&2
  echo "run ./build.sh first" >&2
  exit 1
fi

prefix() {
  local tag="$1"
  while IFS= read -r line; do
    printf '%s %s\n' "$tag" "$line"
  done
}

echo "[run] ferrited → http://0.0.0.0:8088/  (REST + WS)"
echo "[run] vite     → http://0.0.0.0:5173/  (UI; proxies /api and /ws → ferrited)"
echo "[run] open the vite URL in a browser; pick preset + source from the UI"
echo

# ferrited on 0.0.0.0:8088, logs prefixed via process substitution
RUST_LOG="${RUST_LOG:-info}" \
  ./target/release/ferrited \
    --bind 0.0.0.0:8088 \
    --flowgraph "$PLACEHOLDER_FLOWGRAPH" \
    > >(prefix '[ferrited]') 2> >(prefix '[ferrited]' >&2) &
FERRITED_PID=$!

# vite dev on 0.0.0.0:5173 (proxies /api and /ws to ferrited)
( cd web && pnpm dev --host 0.0.0.0 --port 5173 ) \
    > >(prefix '[web     ]') 2> >(prefix '[web     ]' >&2) &
VITE_PID=$!

cleanup() {
  trap - INT TERM EXIT
  echo
  echo "[run] shutting down…"
  kill "$FERRITED_PID" "$VITE_PID" 2>/dev/null || true
  wait "$FERRITED_PID" "$VITE_PID" 2>/dev/null || true
}
trap cleanup INT TERM EXIT

# Block until either dies; cleanup then takes the other down.
wait -n "$FERRITED_PID" "$VITE_PID" 2>/dev/null || true
