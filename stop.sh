#!/usr/bin/env bash
# Stop ferrited + ferrite-ai sidecar and recycle the SDRplay API service.
# Run this before ./build.sh or ./run.sh when a previous ferrited may
# still be holding the SDR — the most common reason SoapySDRUtil --find
# returns "No devices found".
#
# Usage: ./stop.sh [--no-sdr]
#   --no-sdr   don't restart sdrplay_apiService (skip if you're not on SDRplay)

# `-e` left off so a missing ferrited / ferrite-ai (or a sudo-needed
# sdrplay path) doesn't abort the rest of the cleanup.
set -uo pipefail
cd "$(dirname "$0")"

restart_sdr=1
for arg in "$@"; do
  case "$arg" in
    --no-sdr) restart_sdr=0 ;;
    -h|--help) sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

FERRITE_AI_PORT="${FERRITE_AI_PORT:-10002}"

pids=$(pgrep -x ferrited || true)
if [ -n "$pids" ]; then
  echo "[stop] terminating ferrited (pids: $pids)"
  # shellcheck disable=SC2086
  kill $pids 2>/dev/null || true
  for _ in 1 2 3; do
    sleep 1
    pgrep -x ferrited >/dev/null || break
  done
  remaining=$(pgrep -x ferrited || true)
  if [ -n "$remaining" ]; then
    echo "[stop] force-killing ferrited (pids: $remaining)"
    # shellcheck disable=SC2086
    kill -9 $remaining 2>/dev/null || true
  fi
else
  echo "[stop] no ferrited running"
fi

# ferrite-ai runs as `node ... index.ts` under an `npm start` wrapper.
# Identify by the port it owns rather than by argv — `pgrep -f
# tools/ferrite-ai` would also match an editor or shell pwd'd in that
# directory. Requires lsof; if it's not installed, the user gets a
# "not running" report and can kill manually.
ai_pids=""
if command -v lsof >/dev/null 2>&1; then
  ai_pids=$(lsof -t -i :"$FERRITE_AI_PORT" 2>/dev/null || true)
fi
if [ -n "$ai_pids" ]; then
  echo "[stop] terminating ferrite-ai (pids: $ai_pids)"
  # shellcheck disable=SC2086
  kill $ai_pids 2>/dev/null || true
  for _ in 1 2 3; do
    sleep 1
    still=""
    if command -v lsof >/dev/null 2>&1; then
      still=$(lsof -t -i :"$FERRITE_AI_PORT" 2>/dev/null || true)
    fi
    [ -z "$still" ] && break
  done
  if command -v lsof >/dev/null 2>&1; then
    remaining=$(lsof -t -i :"$FERRITE_AI_PORT" 2>/dev/null || true)
    if [ -n "$remaining" ]; then
      echo "[stop] force-killing ferrite-ai (pids: $remaining)"
      # shellcheck disable=SC2086
      kill -9 $remaining 2>/dev/null || true
    fi
  fi
else
  echo "[stop] no ferrite-ai running"
fi

if [ "$restart_sdr" = 1 ] && systemctl list-unit-files sdrplay.service >/dev/null 2>&1; then
  echo "[stop] restarting sdrplay_apiService"
  if sudo -n systemctl restart sdrplay 2>/dev/null; then
    :
  elif [ -t 0 ]; then
    sudo systemctl restart sdrplay
  else
    echo "[stop] sudo needs a password and stdin isn't a tty — run this yourself:" >&2
    echo "         sudo systemctl restart sdrplay" >&2
    exit 3
  fi
fi

echo "[stop] done"
