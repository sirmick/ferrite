#!/usr/bin/env bash
# Make a tree build out-of-the-box: provision the two vendored native deps
# `cargo build` needs but git doesn't track.
#
#   - whisper.cpp source @ a pinned commit — `ferrited` *won't link*
#     without it (undefined `wsp_*`; the build.rs stub only keeps the lib
#     green, not the binary).
#   - the local SoapySDR prefix (ABI-matched core + HackRF/RTL/SDRplay
#     driver modules) — without it the daemon enumerates no hardware.
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

# --- The two native build deps -----------------------------------------
provide "$WHISPER_DIR"                     "$WHISPER_DIR/include/whisper.h" clone_whisper
provide soapysdr                           soapysdr/lib                     build_soapy
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
echo "[bootstrap] (in-browser transcription also needs emsdk + ggml models;"
echo "[bootstrap]  see blocks/native/whisper/README.md)"
