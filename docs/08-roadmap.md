# 08 — Roadmap

## Where we are — 0.9.0 (current state, 2026-10-14)

End-to-end, working today against live RF:

### Server / runtime
- `ferrited` boots, enumerates SoapySDR devices, caches per-device
  probes ([`server/src/device_cache.rs`](../server/src/device_cache.rs)).
  In-process recovery for a wedged driver: `POST /api/devices/reload`
  drops + re-loads every Soapy module via `soapysdr-sys`, gated on a
  stopped pipeline
  ([D31](09-decisions.md#d31--post-apidevicesreload-in-process-soapysdr-module-recovery)).
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
  `agc=false` against AM AGC pumping.
- Postcard `Frame` transport on `/ws/preset`; encoder + decoder both
  come from `ferrite_blocks`, so wire-format drift is a build break.
  Every frame carries the graph `epoch`.
- **Event plane** ([`server/src/event_plane.rs`](../server/src/event_plane.rs),
  plan: [`plans/event-plane.md`](plans/event-plane.md)) — every
  observable change is one record in a server store kind
  ([`server/src/decoder_store.rs`](../server/src/decoder_store.rs):
  `graph`, `status`, `health`, `readback`, `signals`, decodes,
  `activity`, `ai`, `jobs`, `log/<target>`, …), published by an embedded
  MQTT broker (`rumqttd`) on `ferrite/state/<kind>[/<key>]`; browsers
  reach it as `/mqtt` (WS reverse-proxy), anyone can publish raw events
  to `ferrite/ingest/<kind>`. `GET /api/state` is the snapshot;
  `await_events` (MCP) / `ferrite-ctl await` is the subscription.
  The graph `epoch` bumps once per topology rebuild. The old `/ws/state`
  and `/ws/logs` sockets are gone. Non-live kinds persist across
  daemon restarts (`--state-dir`); a radio supervisor restarts a lost
  source and reports through the `health` kind.
- **SignalList** ([`blocks/src/signal_list.rs`](../blocks/src/signal_list.rs))
  — auto-injected tap on the wideband FFT that tracks the strongest
  carriers (passband-p10 floor, DC mask) → `ui:signals` → the `signals`
  store kind; a right-pane view in the UI and the MCP `signals` tool.
- **Analysis toolkit** — `POST /api/analysis/{snapshot,spectrum,spectrogram,envelope}`
  measure live IQ (crate [`ferrite-analysis`](../analysis/); the
  offline Python `ferrite-analyze` in [`tools/analysis/`](../tools/analysis/)
  is a numpy port of the same definitions), and `spot_map` places
  FT8/FT4/WSPR decodes on a map by grid. Each measurement also folds an
  `analysis` record into the store; the UI, CLI and MCP share it.
- Source placeholder + `compose_source`
  ([`runtime/src/compose.rs`](../runtime/src/compose.rs)).
- Reverse-log middleware (`ai_activity_layer`) emits
  `tracing::info!(target: "ai::activity", ...)` events from the
  `X-Ferrite-Command` header; the UI activity panel surfaces them
  in-band with decoder output.
- SoapySDR C-side log handler hooked into Rust `tracing` under target
  `driver` ([`server/src/soapy_log.rs`](../server/src/soapy_log.rs)) so
  driver warnings reach the operator and the AI.
- Native vendor crates under [`blocks/native/`](../blocks/native):
  `liquid-dsp` (FIR / NCO / FEC), `multimon-ng` (POCSAG / FLEX / AFSK /
  FSK9600 / Morse / EAS / DTMF), `dump1090` (Mode S / ADS-B), `rtl-ais`,
  `rtl_433`, `ft8` (kgoba `ft8_lib`), `wsprd`, `fldigi` (curated v4.2.11
  modem cores), `mbelib` (AMBE+2 / IMBE voice, `vocoder` feature),
  `sherpa` (sherpa-onnx streaming ASR, browser STT), `whisper`
  (whisper.cpp + the shared transcript post-processor), plus
  `libc-stubs` (WASM substrate) and `hello` (substrate smoke test).
  The decoders build native into `ferrited` *and* into the browser
  (wasm32 directly, or — fldigi, mbelib, sherpa — a sibling Emscripten
  module).

### Browser
- Browser runtime in a Worker
  ([`web/src/lib/runner/`](../web/src/lib/runner/)); SAB ring +
  AudioWorklet for audio.
- WebGL waterfall with **auto-contrast** (P5/P98 of recent rows,
  EMA-smoothed) plus a manual contrast slider for the colormap window.
- **Auto-scale spectrum** trace separate from waterfall contrast.
- Pointer-gesture grammar across every FFT and waterfall pane
  (wide + narrow), driven by a shared `pointerTune` factory
  ([D30](09-decisions.md#d30--pointer-gesture-grammar-on-spectrum--waterfall-panes)):
  single-click = VFO-only fine-tune (`chan.freq_shift_hz` only,
  source LO parked); double-click = full re-acquire via
  `POST /api/tune` with the per-driver DC-spike dodge
  ([D29](09-decisions.md#d29--post-apitune-as-the-single-vfo-entry-point-dc-spike-dodge-lives-server-side));
  drag = live VFO-only retune, in-flight-gated against the browser's
  6-conn cap. Cross-pane hover line previews where the next click
  would tune. Orange VFO marker + channel-bandwidth band overlay on
  both wide spectrum and waterfall.
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
- Opt-in HTTPS dev server (`FERRITE_HTTPS=1 ./run.sh`) for LAN/mobile
  testing where AudioWorklet + SharedArrayBuffer require a secure
  context.

### AI operator (optional)
- `tools/ferrite-ai/` — Node sidecar that runs Claude Code as an ACP
  client of `claude-agent-acp` ([`acp.ts`](../tools/ferrite-ai/acp.ts)).
  One REST front door, `/api/ai/*` (`hello`, `prompt`, `cancel`,
  `reset`, `setup`, `sessions`), reverse-proxied by `ferrited`; there is
  no `/ws/chat`. No built-in tools (no shell, no file access): the AI's
  whole surface is the `ferrite` MCP server (`ferrite-ctl mcp`). Auth
  via the local `claude` CLI subscription login; no API key in config.
- `tools/ferrite-ctl/` — thin Rust CLI driving `ferrited` via REST;
  every CLI verb is the same op as its MCP tool (`ferrite-ctl mcp`).
  Atomic `--gain` implies `agc=false` so the SDR side doesn't reject a
  manual gain.
- `tools/analysis/` (`ferrite-analyze`) — offline capture analysis for
  operators / CI; the in-app AI measures live via the MCP `spectrum`
  tools (same definitions).
- Per-driver operator notes (`web/src/lib/controls/sdr-presets/*.json`
  → `ai_operator_notes` field) merged into the AI system prompt so it
  knows e.g. RSPdx antenna A is HF-and-VHF SMA, antenna C is BNC
  HF-optimised, RSPduo gain is two-stage IFGR + LNAstate, etc.
- Operator-supplied "describe your radio setup" textbox merged into
  every prompt so the AI routes around physical layout.
- Reverse-log surfaces every AI command in the UI activity panel.
- Conversation state lives on the event plane: the sidecar publishes
  each turn (`user`, `thought`, `text`, `tool_use`, `tool_result`,
  `plan`, `turn_end`, `reset`) as the `ai` store kind, forming text on
  `ai_live`; every AI panel (any tab) renders from there, and a
  headless driver can mirror its own turns in with `chat_post`.

### Shipped flowgraph presets — 40 files in [`flowgraphs/`](../flowgraphs/)

Voice / music: `wbfm`, `wbfm_stereo`, `wbam`, `nbfm`, `lsb`, `usb`.
Digital: `adsb`, `ais`, `packet` (AX.25; APRS is the most common
payload), `pager`, `rtl433-433`, `ft8`, `wspr`, `dmr`, `dsd-fme`;
fldigi modes `cw`, `rtty`, `psk31`, `mt63`, `olivia`, `contestia`,
`dominoex`, `throb`, `navtex`, `rsid` (auto-detect), `digital-hf-20m`.
Weather / hazard: `nwr`.
Capture / record: `capture_fm`, `capture-aprs`, `capture-pager`,
`fm-audio-record`, `am-audio-record`.
Canaries / debug (hidden): `dtmf-e2e`, `morse-e2e`, `packet-debug`.

Files with `variants` (`rtty`, `psk31`, `mt63`, `olivia`, `contestia`,
`dominoex`, `throb`, `ft8`) are families: the catalog and
`GET /api/presets` list each variant as `<slug>-<id>` (e.g.
`ft8-ft4`) and the bare base is not loadable
([14-fldigi-catalog-variants.md](14-fldigi-catalog-variants.md)), so
the 40 files list as 53 presets. Presets marked
`"catalog_visible": false` stay loadable by name but are never listed;
by convention a leading `_` (`_cw_live`, `_rtty_live`, `_nwr_record`,
`_usb_record`, `_wwv_record`) marks an ad-hoc live-RF / record graph,
always hidden.

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

Every tracing target is visible in stdout *and* in the UI logs panel
(via the event plane's `log/<target>` kinds):
`decoder::pocsag`, `decoder::flex`, `decoder::packet`, `decoder::adsb`,
`decoder::ais`, `decoder::rtl_433`, `decoder::cw`, `decoder::eas`,
`decoder::ft8`, `decoder::wspr`, `decoder::fldigi`, `decoder::rsid`,
`decoder::dmr`, `decoder::dsd`, `decoder::transcribe`, `ai::activity`,
`driver`, `flowdiag::node`, `flowdiag::browser`, …

The historical record of how we got here is in
[`09-decisions.md`](09-decisions.md) (D01–D34) and the milestone tally
in [`10-commits.md`](10-commits.md); plan→ship deltas live in
[`12-shipped-vs-planned.md`](12-shipped-vs-planned.md).

## Forward work — 1.0

Concrete tracks remaining before we cut 1.0:

- **Scan / survey jobs** — Phase 5 of the event plane
  ([`plans/event-plane.md`](plans/event-plane.md)); resilience shipped,
  survey + scan not started.
- **Packaging: a systemd unit** — the `.deb` / `.rpm` ship
  `/usr/bin/ferrited` but no unit yet, so the
  [00-context](00-context.md) "enable --now" install path isn't real.
- **Three-surface conformance harness** — one test asserting MCP, CLI
  and UI see the same state for the same action.
- **Decoder breadth** — `Mode A/C` follow-up to dump1090; broader
  `rtl_433` coverage (only the 433.92 MHz preset ships; see
  [Phase 3](decoder-roadmap/03-phase-3-aviation-aprs-ism.md)).
- **sigidwiki sample/thumbnail backfill** for the newer fldigi presets
  (per the per-mode ship gate).
- **Digital voice, next steps** — multi-channel (several channelizers
  on one wide span) → native P25 / NXDN framing → trunk following
  ([Phase 6](decoder-roadmap/06-phase-6-digital-voice.md)).
- **UI** — sherpa live-partials view in the transcript; live AGC-gain
  readout (see *Audio NR* above).
- **Runtime / DSP deferreds** — H6 per-tick `PortMeta` gap flags; RDS
  H3 symbol timing; from the July audit: decimator cutoff validation,
  stereo shared-gain AGC, FFT window-gain calibration, denormals, FIR
  tap scaling.

**Shipped** (was on this list): the decoder UI panels — ADS-B aircraft
map, APRS station map + packet console, FT8/FT4/WSPR decode table +
station map, and the fldigi text console — plus the fldigi keyboard
modes + RSID, FT8/FT4/WSPR, browser-side decode with the live
node↔browser swap (D28), DMR (D34), and the capability-driven
sample-rate dropdown (`GET /api/source/capabilities` →
`fullRateChoices` / `quickRateChoices`).

### Structural simplifications (from the 2026-05-17 cleanup audit) — shipped 2026-05-18

Three refactors that each removed a recurring *category* of desync bug
rather than an instance. Shipped as one conventional commit each:

- **Single Rust source of truth for per-driver tables.** The IF-filter
  ladder lived in the Rust CLI (self-flagged `DUPLICATE-OF`) *and* the
  web `sdr-presets` JSON — and the CLI copy only knew `sdrplay`, so a
  HackRF rate change via `ferrite-ctl` silently skipped the bandwidth
  the web UI set. Now defined once in the `ferrite-sdr-tables` leaf
  crate ([`sdr-tables/src/lib.rs`](../sdr-tables/src/lib.rs), moved out
  of `ferrite-ctl` so `ferrited`'s source policy shares it too);
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
- **Single-authority AI conversation state.** *Superseded (2026-10-11)
  by the ACP sidecar + event plane: `/ws/chat`, `conversation_snapshot`
  and `resume_session_id` are gone; the conversation is the `ai` store
  kind and `/api/ai/*` is the only front door (see* AI operator *above).
  Kept as history:* The sidecar persisted the
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

- ~~Audio transcription panel (Whisper-tiny WASM on the post-demod PCM
  stream).~~ **Shipped** — first as whisper.cpp WASM, since replaced
  in the browser by sherpa-onnx's streaming Zipformer (EN-20M, int8;
  [`blocks/native/sherpa/`](../blocks/native/sherpa/), pinned v1.13.2,
  [D33](09-decisions.md#d33--browser-speech-to-text-sherpa-onnx-replaces-whispercpp)),
  run in a Worker against the post-demod PCM. Node-side placement
  (`audio_split=server`) streams to the same model in the
  [`tools/sherpa-asr/`](../tools/sherpa-asr/) sidecar
  (`SherpaTranscribe`). Ham post-processing (call-sign cleanup,
  tail-loop scrub) is the shared Rust core in `ferrite-whisper`.
  Transcription is opt-in per receiver (Audio: Off | On | Transcribe)
  and lands in the `transcribe` store kind / `Main → Transcript`
  view; the AI reads the same store.
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
- **In-tree vocoders beyond mbelib** — D15 as revised by D34: DMR
  voice ships via vendored mbelib (separable `vocoder` feature);
  P25 / NXDN / D-STAR / YSF voice goes through the operator-installed
  `dsd-fme` (`DsdHelper`).
- **Transmit** — receive only.
- **Whole-spectrum explorer (0 Hz → 300 GHz log axis)** — sketched
  in the original plan, parked behind 1.0 priorities.
