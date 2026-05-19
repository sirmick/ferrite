# 08 — Roadmap

## Where we are — 0.9.0

End-to-end, working today against live RF:

### Server / runtime
- `ferrited` boots, enumerates SoapySDR devices, caches per-device
  probes ([`server/src/device_cache.rs`](../server/src/device_cache.rs)).
- Single-flowgraph, single-source on `AppState`; `--flowgraph PATH`
  required. Hot reconfig via `POST /api/preset`.
- Rust runtime ([`runtime/`](../runtime/)) drives both ends — native
  inside `ferrited`, WASM in the browser worker
  ([`runtime/src/wasm.rs`](../runtime/src/wasm.rs)).
- Demand-driven scheduler over per-wire `TypedRing` accumulators
  ([`runtime/src/typed_ring.rs`](../runtime/src/typed_ring.rs)) honouring
  `Work.consumed`.
- **Cross-env split** — `split_for_environment` auto-inserts
  `WsBridgeTx*`/`WsBridgeRx` pairs and stamps `stream_id`s from
  `CROSS_ENV_STREAM_BASE = 1000`. `ui:<name>` sentinel sinks become
  Tx-only blocks; the browser learns the id via `GET /api/ui-sinks`.
- **`PortMeta` propagation** — `output_sample_rate_hz` *and*
  `output_center_freq_hz` propagate through the topo walk so any block
  can ask the runtime what's actually on its inputs (used by the FFT,
  the channelizer, and the recorder sidecars).
- **`force_params`** — preset-imposed hard override on
  `BlockInstanceDecl` that wins over live `SourceConfig` (see D24 in
  [`09-decisions.md`](09-decisions.md)). Used by `wbam.json` to pin
  `agc_enable=false` against AM AGC pumping.
- Postcard `Frame` transport on `/ws/preset`; encoder + decoder both
  come from `ferrite_blocks`, so wire-format drift is a build break.
- Source placeholder + `compose_source`
  ([`runtime/src/compose.rs`](../runtime/src/compose.rs)).
- Reverse-log middleware (`ai_activity_layer`) emits
  `tracing::info!(target: "ai::activity", ...)` events from the
  `X-Ferrite-Command` header; the UI activity panel surfaces them
  in-band with decoder output.
- SoapySDR C-side log handler hooked into Rust `tracing` under target
  `driver` ([`server/src/soapy_log.rs`](../server/src/soapy_log.rs)) so
  driver warnings reach the operator and the AI.
- Native vendor crates under [`blocks/native/`](../blocks/native): five
  of them — `liquid-dsp` (FIR / NCO / FEC), `multimon-ng` (POCSAG /
  FLEX / AFSK / FSK9600 / Morse / EAS / DTMF), `dump1090` (Mode S /
  ADS-B), `rtl-ais`, `ft8` (kgoba `ft8_lib`), and `libc-stubs` (WASM
  substrate). All build native into `ferrited` *and* into the
  browser WASM bundle.

### Browser
- Browser runtime in a Worker
  ([`web/src/lib/runner/`](../web/src/lib/runner/)); SAB ring +
  AudioWorklet for audio.
- WebGL waterfall with **auto-contrast** (P5/P98 of recent rows,
  EMA-smoothed) plus a manual contrast slider for the colormap window.
- **Auto-scale spectrum** trace separate from waterfall contrast.
- Click-to-tune spectrum (left = retune source, double-click =
  re-centre, right = cancel); orange VFO line + bandwidth band overlay
  on the waterfall draggable for per-VFO offset inside the wideband
  window.
- Generic block-params pipe (D24): `GET /api/pipeline/blocks` +
  `POST /api/pipeline/blocks/:id/params`, mirrored on the WASM side by
  `RuntimeHandle::reconfigure_block`. One `<BlockParams>` Svelte
  component renders every block's params from its `BlockSpec`.
- Catalog vs. bands separation (D27): catalog entries
  (`flowgraphs/*.json`) carry demod topology only; bands entries
  (`web/src/lib/presets/bands.json`) carry frequency + optional VFO
  offset. Searchable Signal Catalog panel with sigidwiki-derived
  thumbnails inlined.
- US band-allocation ribbon under the spectrum (toggleable).
- HTTPS dev server (`run.sh` flips `FERRITE_HTTPS=1`) for LAN/mobile
  testing where AudioWorklet + SharedArrayBuffer require a secure
  context.

### AI operator (optional)
- `tools/ferrite-ai/` — Node sidecar wrapping
  `@anthropic-ai/claude-agent-sdk`, exposes `/ws/chat` (reverse-proxied
  by `ferrited`). Auth via the local `claude` CLI subscription login;
  no API key in config.
- `tools/ferrite-ctl/` — thin Rust CLI driving `ferrited` via REST,
  used by the AI (and by humans). Atomic `--gain` implies
  `agc_enable=false` so the SDR side doesn't reject a manual gain.
- `tools/fft_to_png.py`, `tools/fft_peaks.py` — capture analysis
  tools. AI reads the same PNGs the human does.
- Per-driver operator notes (`web/src/lib/controls/sdr-presets/*.json`
  → `ai_operator_notes` field) merged into the AI system prompt so it
  knows e.g. RSPdx antenna A is HF-and-VHF SMA, antenna C is BNC
  HF-optimised, RSPduo gain is two-stage IFGR + LNAstate, etc.
- Operator-supplied "describe your radio setup" textbox merged into
  every prompt so the AI routes around physical layout.
- Reverse-log surfaces every AI command in the UI activity panel.
- Conversation state is single-authority: the sidecar persists the
  per-session raw transcript next to its SDK session and replays a
  `conversation_snapshot` (or honest `session_reset`) on connect, so
  the browser view can no longer silently diverge from the LLM
  context. See *Structural simplifications* below.

### Shipped flowgraph presets — 21 in [`flowgraphs/`](../flowgraphs/)

Voice / music: `wbfm`, `wbfm_stereo`, `wbam`, `nbfm`, `lsb`, `usb`,
`cw`.
Digital: `adsb`, `ais`, `packet` (AX.25; APRS is the most common payload), `packet-debug`, `pager`, `ft8`.
Tones / canaries: `dtmf-e2e`, `morse-e2e`.
Weather / hazard: `nwr`.
Capture / record: `capture_fm`, `capture-aprs`, `capture-pager`,
`fm-audio-record`, `am-audio-record`.

### Audio NR (5-stage chain)

In [`blocks/src/audio_nr/`](../blocks/src/audio_nr/) — de-emphasis →
impulse blanker → adaptive notch → spectral subtraction (MMSE-LSA /
Wiener) → DFN3 neural denoiser. Per-preset tuned (AM doesn't get the
WBFM stack; SSB uses a different neural threshold). Selectable NR
presets (auto/off/voice/ssb/am/fm) ride the receiver's Off|On|
Transcribe Audio chip; "transcribe" auto-selects voice; live picks
loop back into `<BlockParams>` via the node+browser `pipeline.blocks`
merge (shipped 2026-05-18).

**Follow-up (deferred): live AGC-gain readout.** The AGC stage's
instantaneous gain drifts continuously but isn't observable —
`audio_nr/agc.rs` computes `gain = min(target/env, max_gain)`
per-sample and discards it (no field, no accessor). A UI readout needs
the block to expose current gain + a per-tick drain channel (mirror
the AudioSink level-meter / VoiceTranscribe-tap telemetry pattern) →
reactive store → Transcript/NR panel. ~2 h; blocks+runtime wasm
rebuild + ferrited restart.

### Packaging

`packaging/run_matrix.sh` — Docker-driven matrix builds `.deb` / `.rpm`
for Ubuntu 24.04, Debian 12, Debian 13, Fedora 40 across amd64 / arm64
/ riscv64 where the base image manifest supports it. Per-row failures
don't halt the matrix; PASS/FAIL summary at the end. Output lands in
`dist/packages/<tag>/`. The web bundle is built once on the host
(arch-independent) and copied into each container's source tarball.

### Tracing & UI logs

Every tracing target is visible in stdout *and* in the UI logs panel:
`decoder::pocsag`, `decoder::flex`, `decoder::packet`, `decoder::adsb`,
`decoder::ais`, `decoder::cw`, `decoder::eas`, `decoder::ft8`,
`decoder::rds`, `ai::activity`, `driver`, `flowdiag::node`,
`flowdiag::browser`, …

The historical record of how we got here is in
[`09-decisions.md`](09-decisions.md) (D01–D27) and the milestone tally
in [`10-commits.md`](10-commits.md); plan→ship deltas live in
[`12-shipped-vs-planned.md`](12-shipped-vs-planned.md).

## Forward work — 1.0

Concrete tracks remaining before we cut 1.0:

- **Broader `rtl_433` ISM-device coverage** — 200+ device flavours. See
  [`docs/decoder-roadmap/03-phase-3-aviation-aprs-ism.md`](decoder-roadmap/03-phase-3-aviation-aprs-ism.md).
- **`Mode A/C` follow-up to dump1090** — adds non-Mode-S transponder
  support.
- **sigidwiki sample/thumbnail backfill** for the newest fldigi
  presets (per the per-mode ship gate).

**Shipped** (was on this list): the decoder UI panels — ADS-B aircraft
map, APRS station map + packet console, FT8/FT4/WSPR decode table +
station map, and the fldigi text console — plus the fldigi keyboard
modes + RSID, FT8/FT4/WSPR, and browser-side decode with the live
node↔browser swap (D28).

Other UX leftovers tracked in [`10-commits.md`](10-commits.md):

- Sample-rate dropdown driven by `/api/source/capabilities` (the
  endpoint exists; the web side still hard-codes choices).

### Structural simplifications (from the 2026-05-17 cleanup audit) — shipped 2026-05-18

Three refactors that each removed a recurring *category* of desync bug
rather than an instance. Shipped as one conventional commit each:

- **Single Rust source of truth for per-driver tables.** The IF-filter
  ladder lived in the Rust CLI (self-flagged `DUPLICATE-OF`) *and* the
  web `sdr-presets` JSON — and the CLI copy only knew `sdrplay`, so a
  HackRF rate change via `ferrite-ctl` silently skipped the bandwidth
  the web UI set. Now defined once in
  [`tools/ferrite-ctl/src/sdr_tables.rs`](../tools/ferrite-ctl/src/sdr_tables.rs);
  the web copy is generated by `cargo run -p ferrite-ctl -- gen-tables`
  and a workspace `cargo test` fails if it drifts. No `DUPLICATE-OF`
  markers remain. (The separate **sample-slug** duplication —
  receivers ↔ bands ↔ flowgraph filenames — stays deferred under the
  sample-consolidation note; it was never a `DUPLICATE-OF` marker.)
- **Running pipeline is the single doc authority.** The per-edit
  `apply_block_params` → `preset_doc` mirror-back (a write nested under
  the pipeline lock — the audit's desync window) is gone. While live,
  the runtime's `applied_doc` is the sole writer; `list_blocks` /
  `GET /api/flowgraph` read *through* it (falling back to `preset_doc`
  only when stopped); `stop()` folds the final live state back once, in
  canonical lock order. Browser blocks keep authored params, so the
  node+browser `pipeline.blocks` merge is unchanged.
- **Single-authority AI conversation state.** The sidecar persists the
  raw browser-bound event stream per session to
  `${FERRITE_AI_STATE_DIR}/<session_id>.jsonl` (co-located with the SDK
  session; env mirrors `FERRITE_SCREENSHOTS_DIR`, run.sh + gitignored).
  ferrited stays a transparent `/ws/chat` proxy. On WS connect the
  browser sends `request_snapshot` with the id it thinks it's
  continuing; the sidecar replays the complete transcript as one
  `conversation_snapshot` (the store folds it through its existing
  reducer and *replaces* local turns; localStorage demoted to a
  first-paint cache) or emits `session_reset` with an honest banner
  when that session is unresumable. UI `/clear` routes through a
  unified `reset_session`. Because staleness is now reconciled at
  connect *before* any turn, the per-turn `resume_session_id` is no
  longer a desync source — it's intentionally retained as a harmless
  fallback for the connect→snapshot race window, not removed.

## Parked feature ideas

Captured in [`11-browsdr-inspired-plan.md`](11-browsdr-inspired-plan.md):

- Audio transcription panel (Whisper-tiny WASM on the post-demod PCM
  stream).
- Frequency bookmarks with categories.
- Multi-VFO (static N=2 first, then dynamic spawn).

## Deferred / out of scope (by design)

Captured in [`09-decisions.md`](09-decisions.md):

- **Multi-listener / multi-device per server** — D06 (single listener,
  last-connect wins).
- **Server-side recordings** — D08 (browser owns user state for IQ
  clips; server-side recording exists for *captures*, not for
  user-private archives).
- **Authentication / remote access** — D07 (LAN-trust; user's tunnel
  problem).
- **Mobile UI** — desktop-first.
- **DMR / DSD** — D15 (AMBE patent encumbrance).
- **Transmit** — receive only.
- **Whole-spectrum explorer (0 Hz → 300 GHz log axis)** — sketched
  in the original plan, parked behind 1.0 priorities.
