#!/usr/bin/env bash
# Dev run: ferrited + vite dev + ferrite-ai sidecar, all bound to 0.0.0.0.
# Logs from all three processes are combined onto this script's
# stdout/stderr, prefixed with [ferrited] / [web] / [ai]. Ctrl-C stops
# everything.
#
# No flowgraph or source is started — pick those from the UI:
#   1. open http://<host>:10000/
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
FERRITE_AI_PORT="${FERRITE_AI_PORT:-10002}"

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

if [ ! -d tools/ferrite-ai/node_modules ]; then
  echo "ferrite-ai deps missing — running npm install" >&2
  ( cd tools/ferrite-ai && npm install --silent ) || {
    echo "npm install failed in tools/ferrite-ai" >&2
    exit 1
  }
fi

prefix() {
  local tag="$1"
  while IFS= read -r line; do
    printf '%s %s\n' "$tag" "$line"
  done
}

echo "[run] ferrited → http://0.0.0.0:10001/   (REST + WS, plain HTTP on loopback)"
echo "[run] ai       → ws://0.0.0.0:${FERRITE_AI_PORT}/ws/chat  (Claude Code sidecar; ferrited reverse-proxies /ws/chat here)"
echo "[run] vite     → https://0.0.0.0:10000/  (UI; self-signed cert, proxies /api and /ws → ferrited)"
echo "[run] open the vite URL in a browser; pick preset + source from the UI"
echo "[run] first visit will show a self-signed cert warning — bypass once per browser profile"
echo

# ferrited on 0.0.0.0:10001, logs prefixed via process substitution
RUST_LOG="${RUST_LOG:-info}" \
  ./target/release/ferrited \
    --bind 0.0.0.0:10001 \
    --flowgraph "$PLACEHOLDER_FLOWGRAPH" \
    > >(prefix '[ferrited]') 2> >(prefix '[ferrited]' >&2) &
FERRITED_PID=$!

# ferrite-ai sidecar on FERRITE_AI_PORT (default 10002). ferrited's
# /ws/chat handler reverse-proxies to this; the UI never connects
# directly so single-port architecture holds end-to-end.
( cd tools/ferrite-ai && FERRITE_AI_PORT="$FERRITE_AI_PORT" npm start --silent ) \
    > >(prefix '[ai      ]') 2> >(prefix '[ai      ]' >&2) &
AI_PID=$!

# vite dev on 0.0.0.0:10000 with self-signed TLS (proxies /api and /ws to ferrited).
# FERRITE_HTTPS=1 toggles the basicSsl plugin in web/vite.config.ts — needed for
# LAN / mobile browsers that gate AudioWorklet + SharedArrayBuffer behind a
# secure context.
( cd web && FERRITE_HTTPS=1 pnpm dev --host 0.0.0.0 --port 10000 ) \
    > >(prefix '[web     ]') 2> >(prefix '[web     ]' >&2) &
VITE_PID=$!

cleanup() {
  trap - INT TERM EXIT
  echo
  echo "[run] shutting down…"
  kill "$FERRITED_PID" "$AI_PID" "$VITE_PID" 2>/dev/null || true
  # ferrite-ai is `npm start` → `sh -c` → `node`; killing the npm wrapper
  # may leave the node grandchild. Sweep anything still on the AI port.
  if command -v lsof >/dev/null 2>&1; then
    local ai_left
    ai_left=$(lsof -t -i :"$FERRITE_AI_PORT" 2>/dev/null || true)
    [ -n "$ai_left" ] && kill $ai_left 2>/dev/null || true
  fi
  wait "$FERRITED_PID" "$AI_PID" "$VITE_PID" 2>/dev/null || true
}
trap cleanup INT TERM EXIT

# Block until any one dies; cleanup then takes the others down.
wait -n "$FERRITED_PID" "$AI_PID" "$VITE_PID" 2>/dev/null || true
