# Resume prompt — Ferrite MCP single-layer refactor

Paste this into a fresh Claude Code session in `/home/mick/ferrite` to pick up where we left off.

---

## Context

We collapsed Ferrite's two duplicate control surfaces (the `ferrite-ctl` human CLI and the
`ferrite-ctl mcp` MCP server) into **one operations layer**. Every verb now lives once as a
`FerriteServer::*_op` method in `tools/ferrite-ctl/src/mcp.rs` that hits ferrited's REST API.
The MCP `#[tool]` handlers are thin `ok_json` wrappers over those ops; each human CLI
subcommand in `tools/ferrite-ctl/src/main.rs` calls the same op. **A CLI verb is its MCP
tool — they cannot drift.** The REST boundary to `ferrited` is unchanged (that split is
deliberate; it forces clean design).

Goal behind the work: a **server-side AI driving Ferrite entirely over MCP** — e.g. "find and
transcribe weather": tune/replay to NOAA, confirm signal, transcribe, summarize, while driving
the UI and taking snapshots.

## Branch / state

- Branch: `refactor/mcp-single-layer` (off `main`), **2 commits, not pushed**:
  - `bd9c454` refactor(ferrite-ctl): MCP becomes the single ops layer; CLI is a shim
  - `9fccfdc` feat(mcp): headless-test tools — list_captures/replay_capture + flowdiag/source_readback
- MCP now serves **28 tools**. Full CI matrix was run green locally (cargo clippy/test, pnpm
  wasm:build incl. fldigi+whisper, lint, svelte-check, vitest) and the loop was verified against
  a live headless `ferrited`.

## What was built (audit findings, all fixed)

1. **SoapySource schema completeness** (`blocks/src/soapy_source.rs`): `spec()` now advertises
   `bandwidth_hz`, `agc`, `antenna`, `dc_offset_correction` (with ai_notes); the dynamic
   `settings` map is documented in the block-level notes. These SDR knobs were *accepted* but
   *hidden* from the AI's `list_blocks`/`list_block_types` introspection. Regression test in
   `server/src/block_schema.rs`.
2. **`tune` is the superset**: dodge-aware `/api/tune` + optional `gain_db` (+`agc=false`),
   `bandwidth_hz`, `sample_rate_hz` (driver rate→BW ladder). CLI `tune` now genuinely routes
   through `/api/tune` (healed the false `post_tune` docstring).
3. **New capture coverage**: `start_capture_audio` tool; `start_capture_iq` gained `wideband`;
   `status` regained `ready` + full `flowgraph`.
4. **Per-call activity notes**: every mutating op stamps `x-ferrite-command` + optional `note`,
   so AI actions surface individually in the UI's `ai::activity` transcript. **Verified live.**
5. **Headless-test tools** (the E2E enablers): `list_captures`, `replay_capture` (replay a
   `samples/` fixture through `ModulatedFileSource` — full RX chain, no SDR), `flowdiag`
   (are samples moving?), `source_readback` (did a gain/AGC change land?).
6. New CLI introspection verbs (`caps`, `blocks`, `block-types`, `band-at`, `preset detail`,
   `device reload`, `view-set`, `captures`, `replay`, `flowdiag`, `readback`). CLI output is
   JSON (pretty default, compact `--json`). `device select` resets source params to driver
   defaults.

Docs updated: `docs/02-protocol.md` (single-layer framing + full tool table), `explorer.md`
prompt (captures are MCP-native).

## What's verified

Drove a headless `ferrited` (SineSource default, no hardware) via both the CLI and raw MCP
`tools/call`: `status`/`ready`, Phase-0 src knobs in `block-types`, `list_captures` (37
fixtures), `replay_capture` → source becomes `ModulatedFileSource(nwr_5min.wav, fm, 162.55M)`,
`start` → `flowdiag` shows millions of samples flowing, `source_readback` null on non-Soapy,
and the `ai::activity` note logged verbatim. **Both CLI and MCP drive the live daemon
identically.**

NOT yet exercised headlessly (need a connected browser tab — whisper + canvas live there):
`transcribe`, `view_snapshot`. They work when the AI UI tab is open.

## How to test

**A. Backend loop (no browser, no SDR, ~1 min):**
```bash
RUST_LOG="warn,ai::activity=info" target/debug/ferrited \
  --bind 127.0.0.1:10001 --flowgraph flowgraphs/nwr.json --presets-dir flowgraphs &
sleep 2
C="target/debug/ferrite-ctl --connect http://127.0.0.1:10001"
$C --note "test: NWR replay" replay samples/nwr_5min.wav --kind audio --modulation fm --center 162.55M
$C preset load nwr
$C start
$C flowdiag      # samples_cum climbing = signal moving
$C status        # ready: true, source: ModulatedFileSource
kill %1
```

**B. Full live loop (server-side AI, browser):**
```bash
./build.sh   # only if release binary / wasm artifacts are stale
./run.sh     # ferrited :10001, web UI :10000, ferrite-ai :10002
```
Open http://localhost:10000, leave the tab open, go to the AI tab, type:
**"find and transcribe weather"** (or "replay the NWR sample and transcribe it" with no antenna).
`ferrite-ai` uses the Claude Code SDK / subscription — check `[ai]` log lines if it complains
about auth. First `transcribe` pauses while the browser loads the whisper model.

## Next steps (pick up here)

- [ ] Push `refactor/mcp-single-layer` and open a PR against `main`.
- [ ] (Optional) `scripts/test-weather-loop.sh` — package the Option-A sequence with pass/fail
      assertions as a headless E2E regression check; wire into CI.
- [ ] Browser-half verification: run Option B and confirm `transcribe` → `recent_decodes`
      (`decoder::transcribe`) → summary, plus `view_snapshot` of `wide-waterfall`.
- [ ] Deferred / out of scope so far: whole-UI (DOM) screenshot as an MCP tool — left to the
      AI UI's toolbar/getDisplayMedia path (push-only, needs a human gesture; not MCP-triggerable).
      A pull-based DOM snapshot via the view-bridge would need new browser-side plumbing.

## Key files

- `tools/ferrite-ctl/src/mcp.rs` — the ops layer + 28 MCP tool wrappers + `serve()`.
- `tools/ferrite-ctl/src/main.rs` — thin CLI shim over the ops.
- `blocks/src/soapy_source.rs` — SoapySource `spec()` (the SDR knob schema).
- `server/src/routes.rs` / `server/src/main.rs` — REST endpoints + `ai_activity_layer`.
- `docs/02-protocol.md` — protocol + MCP tool table.
- CI gate: `cargo clippy --workspace --all-targets -- -D warnings -A clippy::pedantic`,
  `cargo test --workspace`, and the web `pnpm wasm:build`/`-r lint`/`-r check`/`test`
  (emsdk at `/home/mick/emsdk`; `source emsdk_env.sh` before `wasm:build:fldigi`).
