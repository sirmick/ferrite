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

# `-e` is intentionally off: each background launch's exit propagates
# via `wait -n` into the cleanup trap, where one failed child triggers
# orderly teardown of the others. With `-e` a transient setup error
# would abort before the trap installed.
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

# Ports — all env-overridable. Defaults are the 12000 family so they line
# up with the ferrite MCP server, which dials
# `ferrite-ctl --connect http://127.0.0.1:12001`.
FERRITE_UI_PORT="${FERRITE_UI_PORT:-12000}"   # vite dev server (browser UI)
FERRITED_PORT="${FERRITED_PORT:-12001}"        # ferrited REST + WS
FERRITE_AI_PORT="${FERRITE_AI_PORT:-10002}"    # ferrite-ai sidecar (internal)

# vite proxies /api + /ws here; the sidecar's internal ferrite-ctl drives
# the same daemon (without this it defaults to 10001 → wrong/dead daemon).
FERRITED_URL="${FERRITED_URL:-http://127.0.0.1:${FERRITED_PORT}}"

# Plain HTTP by default — localhost is a secure context, so AudioWorklet +
# SharedArrayBuffer work without TLS. Set FERRITE_HTTPS=1 for LAN / mobile
# browsers, which gate those APIs behind a secure (https) context.
FERRITE_HTTPS="${FERRITE_HTTPS:-0}"
UI_SCHEME=$([ "$FERRITE_HTTPS" = 1 ] && echo https || echo http)

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

echo "[run] ferrited → http://0.0.0.0:${FERRITED_PORT}/   (REST + WS, plain HTTP on loopback)"
echo "[run] ai       → ws://0.0.0.0:${FERRITE_AI_PORT}/ws/chat  (Claude Code sidecar; ferrited reverse-proxies /ws/chat here)"
echo "[run] vite     → ${UI_SCHEME}://0.0.0.0:${FERRITE_UI_PORT}/  (UI; proxies /api and /ws → ${FERRITED_URL})"
echo "[run] open the vite URL in a browser; pick preset + source from the UI"
[ "$FERRITE_HTTPS" = 1 ] && echo "[run] first visit will show a self-signed cert warning — bypass once per browser profile"
echo

# Persist Screenshot-button PNGs (POST /api/screenshot) into a
# repo-local dir (gitignored) so they survive reboots instead of
# vanishing from /tmp. Overridable; ferrited's CWD is the repo root
# (cd above). NB: `ferrite-ctl view` writes client-side to its own
# --out (default /tmp/ferrite-views) — that path is unaffected.
FERRITE_SCREENSHOTS_DIR="${FERRITE_SCREENSHOTS_DIR:-$(pwd)/screenshots}"
mkdir -p "$FERRITE_SCREENSHOTS_DIR"
echo "[run] shots    → $FERRITE_SCREENSHOTS_DIR  (Screenshot button)"

# Persist the AI sidecar's conversation state — per-session raw event
# transcripts + the readable per-turn log — into a repo-local dir
# (gitignored) so the chat panel survives a sidecar restart / browser
# reload instead of vanishing from /tmp. Same convention as
# FERRITE_SCREENSHOTS_DIR. The sidecar keys files by SDK session id, so
# the visible transcript and the LLM context reset/resume together.
FERRITE_AI_STATE_DIR="${FERRITE_AI_STATE_DIR:-$(pwd)/.ferrite-ai-state}"
mkdir -p "$FERRITE_AI_STATE_DIR"
echo "[run] ai state → $FERRITE_AI_STATE_DIR  (conversation transcripts)"

# ferrited on 0.0.0.0:10001, logs prefixed via process substitution
RUST_LOG="${RUST_LOG:-info}" \
  FERRITE_SCREENSHOTS_DIR="$FERRITE_SCREENSHOTS_DIR" \
  ./target/release/ferrited \
    --bind 0.0.0.0:${FERRITED_PORT} \
    --flowgraph "$PLACEHOLDER_FLOWGRAPH" \
    > >(prefix '[ferrited]') 2> >(prefix '[ferrited]' >&2) &
FERRITED_PID=$!

# ferrite-ai sidecar on FERRITE_AI_PORT (default 10002). ferrited's
# /ws/chat handler reverse-proxies to this; the UI never connects
# directly so single-port architecture holds end-to-end.
( cd tools/ferrite-ai && FERRITE_AI_PORT="$FERRITE_AI_PORT" \
    FERRITE_CTL_CONNECT="$FERRITED_URL" \
    FERRITE_AI_STATE_DIR="$FERRITE_AI_STATE_DIR" npm start --silent ) \
    > >(prefix '[ai      ]') 2> >(prefix '[ai      ]' >&2) &
AI_PID=$!

# vite dev on 0.0.0.0:$FERRITE_UI_PORT (proxies /api and /ws to ferrited via
# FERRITED_URL). FERRITE_HTTPS=1 toggles the basicSsl plugin in
# web/vite.config.ts — needed for LAN / mobile browsers that gate
# AudioWorklet + SharedArrayBuffer behind a secure context.
( cd web && FERRITED_URL="$FERRITED_URL" FERRITE_HTTPS="$FERRITE_HTTPS" \
    pnpm dev --host 0.0.0.0 --port "$FERRITE_UI_PORT" ) \
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
