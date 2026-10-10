#!/usr/bin/env bash
# Make a tree build out-of-the-box: provision the vendored native deps
# the build needs but git doesn't track.
#
#   - whisper.cpp source @ a pinned tag — `ferrited` *won't link*
#     without it (undefined `wsp_*`; the build.rs stub only keeps the lib
#     green, not the binary).
#   - the local SoapySDR prefix (ABI-matched core + HackRF/RTL/SDRplay
#     driver modules) — without it the daemon enumerates no hardware.
#   - sherpa-onnx source @ a pinned tag + the streaming-zipformer EN-20M
#     model — the in-browser speech-to-text engine. Both are inputs to
#     `pnpm wasm:build:sherpa` (which also needs emsdk on PATH); without
#     them that build step is a no-op and the transcript panel shows
#     "engine not built". The model is shared with the node sidecar
#     (tools/sherpa-asr), so the sherpa golden e2e runs off the same copy.
#
# Worktree-aware: a *linked* worktree borrows the primary worktree's
# already-built deps via symlink (instant, no reclone/rebuild) — the
# workflow this repo uses for feature branches. A fresh clone (or a
# primary tree) clones whisper and builds SoapySDR from scratch.
#
# Idempotent — re-running is a no-op once everything is present. Run it
# yourself, or let ./build.sh call it.

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# Pinned to a release tag (not a bare master commit): the glue in
# shim/whisper_glue.c only needs the whisper_full_params VAD fields +
# no_speech_prob getter, stable across the v1.7→v1.8 line, so the latest
# release is safe and link-tested. Keep in sync with README.md (PINNED:).
WHISPER_PIN="v1.8.6"
WHISPER_URL="https://github.com/ggml-org/whisper.cpp"
WHISPER_DIR="blocks/native/whisper/vendor/whisper.cpp"

# sherpa-onnx: pinned to the release tag the browser engine was brought up
# on (blocks/native/sherpa/emscripten/build.sh drives its CMake; the JS
# API shim it patches is stable across the v1.1x line). Keep in sync with
# blocks/native/sherpa/README.md.
SHERPA_PIN="v1.13.2"
SHERPA_URL="https://github.com/k2-fsa/sherpa-onnx"
SHERPA_DIR="blocks/native/sherpa/vendor/sherpa-onnx"
# The streaming Zipformer EN-20M model (~42 MB int8). Same default URL as
# tools/sherpa-asr/setup.sh — one copy serves the browser build, the node
# sidecar and the golden e2e. Override with SHERPA_ASR_MODEL_URL.
SHERPA_MODEL_URL="${SHERPA_ASR_MODEL_URL:-https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-20M-2023-02-17.tar.bz2}"
SHERPA_MODEL_DIR="tools/sherpa-asr/models/$(basename "$SHERPA_MODEL_URL" .tar.bz2)"

# --- Locate the primary worktree, if we're a linked one ----------------
# In a linked worktree `--git-dir` (this tree's .git/worktrees/<n>) differs
# from `--git-common-dir` (the shared repo). The primary worktree is the
# first entry of `git worktree list`.
PRIMARY=""
if git rev-parse --git-dir >/dev/null 2>&1; then
  gd="$(git rev-parse --absolute-git-dir 2>/dev/null || echo)"
  cd_="$(cd "$(git rev-parse --git-common-dir 2>/dev/null)" && pwd)"
  if [ "$gd" != "$cd_" ]; then
    PRIMARY="$(git worktree list --porcelain | awk '/^worktree /{print $2; exit}')"
    [ "$PRIMARY" = "$ROOT" ] && PRIMARY=""
  fi
fi
[ -n "$PRIMARY" ] && echo "[bootstrap] linked worktree — primary: $PRIMARY"

# borrow PATH from primary as a symlink, else run the builder.
#   $1 path (rel to ROOT)   $2 present-marker (rel)   $3.. builder cmd
provide() {
  local path="$1" marker="$2"; shift 2
  if [ -e "$ROOT/$marker" ]; then
    echo "[bootstrap] ok: $path"
    return
  fi
  if [ -n "$PRIMARY" ] && [ -e "$PRIMARY/$marker" ]; then
    echo "[bootstrap] symlink $path  <-  primary"
    mkdir -p "$(dirname "$ROOT/$path")"
    ln -sfn "$PRIMARY/$path" "$ROOT/$path"
    return
  fi
  echo "[bootstrap] build  $path"
  "$@"
}

clone_whisper() {
  echo "[bootstrap] vendoring whisper.cpp @ ${WHISPER_PIN:0:12} (depth 1)"
  rm -rf "$WHISPER_DIR"
  mkdir -p "$WHISPER_DIR"
  git -C "$WHISPER_DIR" init -q
  git -C "$WHISPER_DIR" remote add origin "$WHISPER_URL"
  git -C "$WHISPER_DIR" fetch -q --depth 1 origin "$WHISPER_PIN"
  git -C "$WHISPER_DIR" checkout -q --detach FETCH_HEAD
}

build_soapy() { ./scripts/build-soapysdr.sh; }

clone_sherpa() {
  echo "[bootstrap] vendoring sherpa-onnx @ $SHERPA_PIN (depth 1)"
  rm -rf "$SHERPA_DIR"
  mkdir -p "$SHERPA_DIR"
  git -C "$SHERPA_DIR" init -q
  git -C "$SHERPA_DIR" remote add origin "$SHERPA_URL"
  git -C "$SHERPA_DIR" fetch -q --depth 1 origin "refs/tags/$SHERPA_PIN:refs/tags/$SHERPA_PIN"
  git -C "$SHERPA_DIR" checkout -q --detach "$SHERPA_PIN"
}

fetch_sherpa_model() {
  echo "[bootstrap] fetching sherpa model: $(basename "$SHERPA_MODEL_DIR")"
  local models tmp
  models="$(dirname "$SHERPA_MODEL_DIR")"
  mkdir -p "$models"
  tmp="$models/.dl.tar.bz2"
  # The GitHub release CDN drops connections now and then; retry hard.
  curl -fL --retry 5 --retry-delay 5 --retry-all-errors -sS -o "$tmp" "$SHERPA_MODEL_URL"
  tar xjf "$tmp" -C "$models"
  rm -f "$tmp"
}

# --- Native build deps ---------------------------------------------------
provide "$WHISPER_DIR"                     "$WHISPER_DIR/include/whisper.h" clone_whisper
provide soapysdr                           soapysdr/lib                     build_soapy
provide "$SHERPA_DIR"                      "$SHERPA_DIR/build-wasm-simd-asr.sh" clone_sherpa
provide "$SHERPA_MODEL_DIR"                "$SHERPA_MODEL_DIR/tokens.txt"   fetch_sherpa_model
# soapysdr-src rides along when borrowed from primary (build_soapy makes its own).
if [ -n "$PRIMARY" ] && [ ! -e "$ROOT/soapysdr-src" ] && [ -e "$PRIMARY/soapysdr-src" ]; then
  ln -sfn "$PRIMARY/soapysdr-src" "$ROOT/soapysdr-src"
fi

# --- Web deps: a REAL install, not a symlink. ---------------------------
# Symlinking the primary's node_modules breaks `vite dev`: it serves deps
# via `/@fs/<realpath>`, and the symlink resolves *outside* this worktree's
# `server.fs.allow` root, so vite blocks them (empty MIME → corrupted
# module). A real `pnpm install` is sub-second on a warm store anyway, and
# build.sh's later install is then a no-op. (Covers every workspace package,
# so the lefthook `pnpm -r check/lint` doesn't ENOENT on tools/*.)
if [ ! -e "$ROOT/node_modules" ]; then
  echo "[bootstrap] pnpm install (real node_modules — a symlink breaks vite)"
  pnpm install --frozen-lockfile
fi
# wasm bindings (worktree convenience): copy from the primary so you don't
# need emsdk just to run the UI. A symlink confuses tsc's module resolution,
# so copy. A fresh clone builds them via `pnpm wasm:build` (build.sh).
if [ -n "$PRIMARY" ] && [ ! -e "$ROOT/web/src/lib/wasm" ] && [ -e "$PRIMARY/web/src/lib/wasm" ]; then
  echo "[bootstrap] copy web/src/lib/wasm  <-  primary"
  cp -r "$PRIMARY/web/src/lib/wasm" "$ROOT/web/src/lib/wasm"
fi

echo "[bootstrap] done — \`cargo build\` / \`./build.sh\` should now work."
echo "[bootstrap] (in-browser transcription + fldigi also need emsdk on PATH"
echo "[bootstrap]  before \`pnpm wasm:build\`; see blocks/native/sherpa/README.md)"
