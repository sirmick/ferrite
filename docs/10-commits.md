# 10 — Commit-level implementation plan

This is the authoritative commit list for v0.1 (phases 0–E), plus sketches
for F and G. Each item is one conceptual change and should land as one green
commit (see `09-decisions.md` D16).

Numbering is cumulative across phases. When a phase ships, its items are
crossed off in PRs; this file is updated to reflect reality.

## Phase 0 — Documentation

1. `docs: LICENSE header note, README stub, project tagline`
2. `docs: project context and goals (docs/00-context.md)`
3. `docs: architecture overview (docs/01-architecture.md)`
4. `docs: REST API and WS frame format (docs/02-protocol.md)`
5. `docs: block trait and WASM port strategy (docs/03-blocks.md)`
6. `docs: JSON flowgraph schema (docs/04-flowgraphs.md)`
7. `docs: testing strategy and replay mode (docs/05-testing.md)`
8. `docs: build and dev setup (docs/06-build.md)`
9. `docs: SBC deployment guide (docs/07-deploy.md)`
10. `docs: roadmap and phases (docs/08-roadmap.md)`
11. `docs: decision log (docs/09-decisions.md)`
12. `docs: commit-level implementation plan (docs/10-commits.md)` ← this file

## Phase A — Scaffolding

Get the real stack shape in place. No features; everything here unlocks
later phases.

13. `chore: initialize cargo workspace with server/ and blocks/ members`
14. `feat(server): tokio + axum skeleton on 8088, GET /api/hello`
15. `feat(server): /ws echo endpoint (text frames only, placeholder)`
16. `chore: initialize pnpm workspace with web/ and packages/*`
17. `chore(web): SvelteKit + adapter-static, empty app shell`
18. `chore(web): Tailwind v4 via Vite plugin, base theme tokens`
19. `chore(web): Bits UI baseline, Dockview layout with a single panel`
20. `chore(web): Vite dev proxy for /api and /ws → ferrited:8088`
21. `chore(web): COOP/COEP Vite plugin for dev; confirm SAB works`
22. `feat(server): static file serving + COOP/COEP headers for prod`
23. `chore: packages/flowgraph-runtime TS skeleton (exports, types only)`
24. `chore: packages/flowgraph-blocks TS skeleton (WASM-wrapper shape)`
25. `chore(web): vite-plugin-wasm + top-level-await`
26. `feat(blocks): trivial Rust WASM demo block (adds two numbers)`
27. `feat(web): load demo WASM block in a Worker and log output`
28. `ci: GitHub Actions — cargo check/test/clippy/fmt on ubuntu-latest`
29. `ci: GitHub Actions — pnpm install/test/lint/check`
30. `chore: lefthook (or husky) pre-commit hooks — fmt, prettier, check`
31. `docs: contributing guide (CONTRIBUTING.md) — commit style, CI gates`

**Phase A done when:** CI is green, `pnpm dev` loads the shell, a Worker
invokes the WASM demo block, headers make `crossOriginIsolated === true`.

## Phase B — Synthetic data path E2E

Full data path, zero hardware. Replay mode lands here so CI uses the same
binary shipped to users.

32. `feat(blocks): Block trait + BlockSpec + port types enum`
33. `feat(blocks): Param schema types + #[ferrite_block] proc macro`
34. `feat(blocks): SignalSource (sine, chirp, WGN) + unit tests`
35. `feat(blocks): FFT block (rustfft) + Parseval property test`
36. `feat(blocks): Decimator + rate-preservation property test`
37. `build(blocks): wasm-pack target + wasm-bindgen-test CI job`
38. `test(blocks): identical fixtures pass native and WASM`
39. `feat(server): WS binary frame codec (16-byte header + payload)`
40. `test(server): frame codec round-trip + property test`
41. `feat(server): in-process scheduler running SignalSource → FFT`
42. `feat(server): FFT stream publisher on stream_id 1`
43. `feat(web): WebGL waterfall renderer (imperative, Svelte wrapper)`
44. `feat(web): WebGL spectrum-line renderer sharing the same canvas`
45. `feat(web): WS client with auto-reconnect + frame demux`
46. `feat(web): wire WS → waterfall on stream 1`
47. `feat(server): --source file://path.iq replay mode (--loop flag)`
48. `feat(server): synthetic source fallback if no --source given`
49. `test(server): wire-protocol conformance harness (replay + test client)`
50. `test(web): AudioWorklet process() unit tests (Vitest, synthetic input)`
51. `test(web): SAB ring-buffer stress test in headless Chromium`
52. `feat(web): dev debug panel — block counters (samples_in/out)`
53. `fixtures: add wbfm_1khz_pilot.iq + sidecar + LICENSE entry`
54. `ci: fixture provenance check`

**Phase B done when:** `pnpm dev` + `ferrited --source file://...` shows
a moving WBFM waterfall in the browser. Conformance test passes in CI.

## Phase C — First real device

SoapySDR integration, REST lifecycle, auto-generated options dialog.

55. `feat(server): soapysdr-rs binding + enumerate devices`
56. `feat(server): capability probe (rates, freq range, gains, settings)`
57. `feat(server): GET /api/devices returning full capability schema`
58. `feat(server): POST /api/device/open — session_id, device lifecycle`
59. `feat(server): GET /api/device/{id}/state`
60. `feat(server): PATCH /api/device/{id}/settings — mutable-while-streaming`
61. `feat(server): POST /api/device/{id}/close`
62. `feat(server): session eviction — last-connect wins + WS session_closed`
63. `feat(web): device picker panel — lists /api/devices output`
64. `feat(web): generic options dialog from capability schema (Bits UI)`
65. `feat(web): open flow — POST /api/device/open + WS connect + state store`
66. `feat(web): PATCH settings wiring for every knob on the dialog`
67. `feat(web): "apply & restart stream" for mutableWhileStreaming=false`
68. `test(server): capability-schema validation tests`
69. `test(web): options-dialog rendering tests from fixture schemas`
70. `docs: RTL-SDR and SDRPlay RSPduo tested-config notes`

**Phase C done when:** plug in RTL-SDR (then SDRPlay), page shows device,
user opens it, configures gain/bias-tee, sees live FFT.

## Phase D — First listening experience

Channelizer, flowgraph runtime, FmDemod, audio out.

71. `feat(blocks): Channelizer block (native only; serverside)`
72. `feat(server): POST /api/device/{id}/vfo — allocate stream, channelize`
73. `feat(server): PATCH /api/device/{id}/vfo/{vfo_id} — retune mid-stream`
74. `feat(server): DELETE /api/device/{id}/vfo/{vfo_id}`
75. `feat(server): VFO streams on stream_id >= 2, iq_f32 payload`
76. `feat(blocks): FmDemod block (Rust, dual-built) + unit test`
77. `feat(runtime): flowgraph JSON parser + schema validation` *(shipped in TS; superseded by M1 — see "Runtime pivot" section below)*
78. `feat(runtime): block registry + instantiation` *(shipped in TS; superseded by M1)*
79. `feat(runtime): wire-up + topological scheduler` *(shipped in TS; superseded by M1)*
80. `feat(runtime): init / start / stop lifecycle + runtime.update()` *(shipped in TS; superseded by M1)*
81. `feat(web): SAB audio ring buffer (producer side)`
82. `feat(web): AudioWorklet consumer (128-frame process loop)`
83. `feat(web): AudioSink block using the ring buffer`
84. `feat(web): WsIqSource block (subscribes to VFO stream)`
85. `feat(web): flowgraph runner Worker + lifecycle messaging`
86. `flowgraphs: add flowgraphs/wbfm.json`
87. `feat(web): per-digit frequency dial widget`
88. `feat(web): drag VFO cursor on waterfall to retune`
89. `feat(web): "Signal Catalog" panel shell (presets list)`
90. `test: golden-fixture WBFM end-to-end (replay → audio RMS + pilot tone)`

**Phase D done when:** WBFM smoke works on RTL-SDR and SDRPlay, golden-
fixture test is green, VFO drag retunes without reconnecting.

## Phase E — First decoder (likely ADS-B)

Port a C decoder, wrap as a block, wire through a flowgraph.

92. `chore: vendor dump1090 DSP core under decoders/dump1090 (strip I/O)`
93. `build(decoders): cc-based native build of dump1090 core`
94. `build(decoders): wasm32 build via clang + wasi-libc subset`
95. `feat(blocks): AdsbDecoder block wrapping the C ABI (Rust wrapper)`
96. `test(blocks): AdsbDecoder unit test on a known DF17 fixture`
97. `test(blocks): native/WASM parity — same fixture, identical hex`
98. `flowgraphs: add flowgraphs/adsb.json`
99. `feat(web): EventBusSink block routing to a Svelte store`
100. `feat(web): ADS-B message list panel (virtualized, Bits UI)`
101. `feat(web): MapLibre panel plotting aircraft positions`
102. `test: golden-fixture ADS-B (replay → assert known hex decoded)`
103. `docs: decoder contributor guide — how to port a new C decoder`

**Phase E done when:** tuned to 1090 MHz, aircraft populate the map live,
ADS-B golden-fixture CI test is green.

## Phase F — LLM identify (post-v0.1)

Backend proxy + RAG index; drag-to-identify UX.

F01. `feat(tools): scrape-sigidwiki — MediaWiki API client`
F02. `data: generate data/sigidwiki.json (CC-BY-SA attributed)`
F03. `feat(server): sigidwiki index loader + freq/BW lookup`
F04. `feat(server): LLM client (Anthropic API) with config in ferrited.toml`
F05. `feat(server): POST /api/identify — compose vision prompt, call LLM`
F06. `feat(server): request cache (hash of image+metadata)`
F07. `feat(web): drag-to-select region on waterfall`
F08. `feat(web): capture PNG + metadata, POST /api/identify`
F09. `feat(web): results card — best guess + candidates, sigidwiki links`
F10. `feat(web): "Signal Catalog" panel — search over sigidwiki.json`

## Phase G — Spectrum allocation explorer (post-v0.1)

Full-spectrum-at-a-glance Dockview panel.

G01. `data: spectrum-allocations.json from ITU/FCC/ARRL public sources`
G02. `feat(web): band-allocation tooltip on tuned frequency (lands earlier)`
G03. `feat(web): spectrum-explorer Svelte component (SVG + virtualization)`
G04. `feat(web): log-scale freq axis 0 Hz → 300 GHz with wheel zoom`
G05. `feat(web): click-to-retune (with out-of-device-range state)`
G06. `feat(web): WebGL fallback renderer if SVG density is a problem`

## Runtime pivot — M1–M5 (supersedes Phase D commits #77–80)

Phase D shipped a TS flowgraph runtime (`packages/flowgraph-runtime/`)
plus TS block wrappers (`packages/flowgraph-blocks/`). Once WBFM ran
end-to-end it was clear the server needed to run the *same* graph the
browser did — not just agree on its JSON shape. See **D19** for the
context and reasoning. M1–M5 replace the TS runtime with a single Rust
runtime (`runtime/` crate, dual-compile native + WASM) and delete the
TS runtime/blocks packages at M4.

These milestones are sequential. Each is a set of commits; the commit
list for the later milestones will be expanded as they are planned in
detail. The headings below are load-bearing; the sub-items are a
working outline.

### M1 — Rust `runtime/` crate (library only)

- [x] `feat(runtime): Rust runtime crate skeleton + FlowgraphDoc serde` — 01fef45
- [x] `feat(runtime): graph validator + topo scheduler` — 4bbe89e
- [x] `feat(runtime): registry-dependent validation + populated port types` — 7046992
- [ ] `feat(runtime): block construction — factory registry producing Box<dyn Block>`
- [ ] `feat(runtime): tick pump — buffer allocation, per-block process() loop, back-pressure`
- [ ] `feat(runtime): lifecycle state machine — Init / Start / Stop / Reconfigure`
- [ ] `feat(runtime): WsBridge block pair — placeholder; wires land in M2`

**M1 done when:** `runtime/` builds as rlib + cdylib, runs a trivial
SignalSource → Sink graph natively and under `wasm-pack test`, and
exposes the API that M2 will call.

### M2 — `ferrited` loads presets and runs the server-half

- [ ] `feat(server): link ferrite-runtime, load preset from disk`
- [ ] `feat(server): SoapySource Rust block (moves from hardcoded pipeline)`
- [ ] `feat(server): scheduler splits preset into server-half + browser-half, auto-inserts WsBridge`
- [ ] `feat(server): WsBridge server-side half — encode IQ to existing WS frame format`
- [ ] `flowgraphs: wbfm.json updated with per-block placement (source=server, demod=browser)`
- [ ] `test(server): end-to-end against the existing TS browser runtime — same waterfall + audio as before`

**M2 done when:** browser is unchanged, `ferrited` runs WBFM from a
preset file, and the golden-fixture audio test still passes.

### M3 — Reconfigure event + `reconfigScope`

- [ ] `feat(runtime): ReconfigureScope enum { Self, Downstream, SourceRestart }`
- [ ] `feat(runtime): preset diff → minimal reconfigure plan`
- [ ] `feat(runtime): rollback on apply failure (retain previous JSON)`
- [ ] `feat(blocks): annotate existing block params with reconfigScope`
- [ ] `feat(server): PATCH /api/device/{id}/flowgraph — apply new preset, return plan`
- [ ] `chore: remove mutable_while_streaming — fully replaced by reconfigScope`

**M3 done when:** changing a WBFM param (volume → Self, decim → Downstream,
centre freq → SourceRestart) triggers only the matching restart scope,
with rollback if the new preset fails to instantiate.

### M4 — Browser loads the Rust runtime as WASM

- [ ] `build(runtime): wasm-pack + vite-plugin-wasm integration in web/`
- [ ] `feat(web): browser-only blocks registered into the Rust runtime (AudioSink, WsIqSource)`
- [ ] `feat(web): flowgraph runner Worker — drives the Rust runtime via bindings`
- [ ] `chore: delete packages/flowgraph-runtime`
- [ ] `chore: delete packages/flowgraph-blocks`
- [ ] `test(web): golden-fixture WBFM — same audio result under the Rust runtime`

**M4 done when:** browser runs the same preset as M2 but via the Rust
runtime in a Worker; the TS packages are gone; CI is green.

### M5 — Config dialogs + receivers pane

- [x] `feat(blocks): AmDemod Rust block (dual-built)` — decda9b
- [x] `flowgraphs: add flowgraphs/wbam.json (AM variant of wbfm)` — 6cd791a
- [x] `feat(web): source options dialog — schema-driven, one section per source param group` — 3551740
- [x] `feat(web): flowgraph options dialog — schema-driven, one section per non-source block` — 3501ea4
- [x] `feat(web): dialogs gain a read-only JSON tab mirroring live preset` — 38410a7
- [x] `feat(web): receivers pane with AM/FM dropdown — full chain swap, source stable` — a2c6dda
- [x] `test(web): dialog reconfigure paths map to correct reconfigScope` — 1ab711f

**M5 done when:** user picks AM or FM from the receivers pane and the
flowgraph reconfigures without touching the source; source and
flowgraph dialogs render correctly for both presets; VFO0 is the only
wired VFO (with explicit room in the preset schema for VFO1+).

## Post-M5 — preset-first server + FFT tap

After M5 closed, a cluster of refactors landed to finish wiring the
shipped pipeline end-to-end: the server went preset-first (one
`AppState`, no `SessionState`), the FFT waterfall started flowing via
a server-side `ui:fft` tap, and the WS transport moved to a single
postcard `Frame` enum shared by both sides. This section tracks what
shipped.

- [x] `feat(blocks): TeeIqF32 block — 1→2 IQ fan-out` — 61b706a
- [x] `feat(blocks+server): FftU8 bridge transport across /ws/preset` — 2d85126
- [x] `refactor(blocks+server): unify bridge sinks behind one BridgeSink trait` — 2910d0f
- [x] `refactor(blocks+server+web): postcard Frame transport, drop 16-byte header` — 1a14232
- [x] `refactor(runtime): Wire struct, ui:<name> sinks, Placement::Either inference` — a74cbc7
- [x] `feat(flowgraphs,runtime): server-side FFT tap via ui:fft in wbfm+wbam` — edf95d4
- [x] `feat(runtime,flowgraphs): Source placeholder + compose_source` — 0f197f6
- [x] `refactor(server): preset-first AppState, REST, and CLI; drop SessionState` — 597fd9d
- [x] `refactor(web): preset-first pipeline store, SourceDialog, header Start/Stop` — 25792d4
- [x] `feat(server,web): GET /api/ui-sinks for preset-allocated stream_ids` — c81f0a5
- [x] `feat(runtime,blocks): output capacity hints + FFT input accumulation` — c786a24

**E2E coverage landed alongside:**

- [x] `test(server): wbfm_preset_e2e_emits_iq_and_fft_streams` — full
      preset → pipeline → `/ws/preset` → both crossings light up
- [x] `test(web): runnerDsp — browser runtime tested in Node against
      FakeFrameClient, recovers a 1 kHz FM tone from fixture IQ`
- [x] `test(blocks): wbfm_e2e — synthetic WBFM with mono + pilot`

**Post-M5 done when:** a real session works end-to-end from the shipped
UI — device enumerated, preset started, waterfall flowing from
`ui:fft`, WBFM audible in the browser — through the postcard-framed
`/ws/preset` transport, with the server holding exactly one preset-
backed pipeline and the source independently patchable.

### Follow-ups

- [ ] `feat(web): waterfall tuning — drag = source centre, right-click
      = channelizer offset` — wires both frequency knobs into the
      spectrum picture per D21. Drag debounces at mouse-up and
      `PATCH /api/source`; right-click places the channelizer at the
      clicked absolute freq via `PATCH /api/flowgraph` (`Self`-scope
      `freq_shift_hz`). First real exercise of the M3 `reconfigScope`
      machinery. Multi-VFO extension: right-click menu grows a "set
      VFO<n>" submenu; drag behaviour unchanged.

## Pre-Phase-1 — DTMF events-bridge E2E canary

A single hardcore end-to-end test that exercises every Post-M5 layer
(preset compose + env-split, `WsBridge`, postcard `Frame` transport,
browser runtime in Node, cross-env events delivery) before Phase 1
starts building the full analog-listening block set. DTMF is the
simplest real digital mode and has no external fixture dependency —
test audio is generated in-graph.

Architecture (locked):

- Single-client. No multi-client decimator compromise.
- Server-half: `DtmfAudioSource(8 kS/s real) → AmModulator(offset=+50 kHz,
  out=2.4 MS/s) → TeeIqF32 → { Channelizer(factor=10, shift=+50 kHz) →
  Decimator(factor=30) → WsBridgeTx, FFT(4096) → LogMagU8 → ui:fft }`.
- Browser-half: `WsBridgeRx(8 kS/s IQ) → AmDemod → DtmfDecoder →
  EventsSink`.
- Server-side decimation — wire carries 8 kS/s IQ, not 240 kS/s.

Commits:

- [ ] `feat(blocks): DtmfAudioSource — programmatic DTMF tone generator (digits, hold_ms, gap_ms)`
- [ ] `feat(blocks): AmModulator — real_f32 → iq_f32 with offset_hz, mod_depth, out_rate_hz`
- [ ] `feat(blocks): DtmfDecoder — 8-Goertzel digit detector, real_f32 → events`
- [ ] `feat(blocks): EventsSink — native mpsc + browser postMessage + test drain API`
- [ ] `feat(server): ferrited prints actual listen addr on stdout for ephemeral-port harnesses`
- [ ] `flowgraphs: presets/dtmf-e2e.json — AM-upmixed DTMF source + split chain`
- [ ] `test(blocks): cross-env preset runs both halves in one Runtime, events arrive`
- [ ] `test(web): dtmfE2E — spawn ferrited, drive blocks.wasm in Node, assert "1234"`

**Done when:** `pnpm -C web test dtmfE2E` boots `ferrited`, runs the
browser half via the Rust runtime WASM in Node, and drains the four
expected digit events off the browser-side `EventsSink` queue. The
native integration test covers the same chain without the WS hop, as
a faster regression signal.

## Post-canary — Cross-env transport round-trip + scheduler rewrite

Two pieces of work fell out of the DTMF canary effort. The canary
shook out a transport gap (browser ingress was never wired) and a
scheduler limitation (rate-expansion chains silently drop samples).
Rationale in D22 (WsBridgeRx unification) and D23 (rate-aware
scheduler).

### Track A — Unify `WsIqSource` into `WsBridgeRx`

Close the browser ingress half of `env_split`. Every `node → browser`
wire carrying `IqF32` should actually deliver samples. Unblocks any
future preset with an `IqF32` crossing. Scope is ~1-2 days; see D22.

- [x] `refactor(blocks): merge WsIqSource body into WsBridgeRx; delete WsIqSource`
- [x] `refactor(runtime): pushIq wasm method dispatches to WsBridgeRx instead of WsIqSource`
- [x] `feat(web): runner routes FrameClient IqF32 frames to WsBridgeRx instances by stream_id`

**Done when:** native + wasm32 both build with the unified `WsBridgeRx`
block, every workspace test passes, and `pnpm test` stays green after
the type-string rename.

The cross-env `dtmfE2E` vitest was originally planned as a fourth
Track A commit against a reduced DTMF preset (matched-rate AM to
sidestep the scheduler gap). Deferred into Track B's final item
instead — Track B's rate-aware scheduler lets the canary run the real
authored preset, so we write the vitest once against the full chain
rather than shipping a reduced variant first.

### Track B — Rate-aware scheduler

Replace the one-tick-one-process scheduler with accumulating ring
buffers + demand-driven re-run. See D23 for the design. Scope is
~1-2 weeks.

- [ ] `feat(runtime): TypedRing — per-wire circular buffer with multi-reader pointers`
- [ ] `feat(blocks): Block::relative_rate + Block::forecast trait additions with sync defaults`
- [ ] `refactor(runtime): scheduler work loop — demand-driven re-run until quiescent or budget exhausted`
- [ ] `refactor(blocks): migrate rate-changing blocks (Channelizer, Decimator, AmModulator, FFT, LogMagU8, DtmfAudioSource, DtmfDecoder)`
- [ ] `refactor(blocks): retire output_capacity_hints — ring sizing derives from rate declarations`
- [ ] `test(runtime): rate-expansion chains (AM upsample, resampler) don't drop samples`
- [ ] `test(web): dtmfE2E vitest — spawn ferrited serving the full dtmf-e2e.json, drive the browser half via the runner, assert "1234"`

**Done when:** the DTMF canary runs the full authored preset (no
rate-sidestepping) both natively and cross-env, and a synthetic test
confirms arbitrary rate topologies produce correct sample counts.

## UX-1 — WBFM end-to-end with real controls

Push the WBFM preset from "pipeline runs, FFT draws" to "a real SDR
app": click-to-tune, preset switching, receiver panel, working
controls end-to-end. Built on the generic block-params pipe from D24,
the spectrum click inversion from D25, and the preset-first UX from
D26. Scope is ~1–2 weeks of web-heavy work; backend additions are
concentrated in the first three commits, the rest is Svelte.

The foundation is everything in D24 — without it, each subsequent
item needs bespoke plumbing. Ship the foundation first; every
follow-up is a spec-declaration-plus-glue commit.

- [ ] `feat(blocks): ParamSpec extension — min, max, step, unit`
- [ ] `feat(server): GET /api/pipeline/blocks + POST /api/pipeline/blocks/{id}/params — generic reconfigure dispatch`
- [ ] `feat(runtime): RuntimeHandle.reconfigureBlock — WASM mirror of the REST params endpoint`
- [ ] `feat(web): setBlockParam dispatcher + pipelineStore.blocks reshape`
- [ ] `feat(web): <BlockParams> component driven by ParamSpec[]`
- [ ] `feat(server): GET /api/presets + POST /api/preset — dir-based preset registry with tuning-retention swap`
- [ ] `feat(server): GET /api/source/capabilities — SoapySDR-reported rates/antennas/bandwidths`
- [ ] `feat(web): bands.json gains preset field; fill WBFM FM-broadcast group (88.1–107.9 step 0.2); click-to-tune routes through setBlockParam`
- [ ] `refactor(web): fixed spectrum-over-waterfall layout with resize handle; delete movable-pane machinery`
- [ ] `feat(web): green SDR-centre Nixie + orange VFO Nixie — bound to source and channelizer[0] via setBlockParam`
- [ ] `feat(web): spectrum click handlers — left=VFO, right=SDR centre (D25)`
- [ ] `feat(web): sample-rate dropdown populated from /api/source/capabilities`
- [ ] `feat(web): receiver panel renders <BlockParams> for every non-source, non-channelizer, non-FFT block in current preset`
- [ ] `feat(web): FFT controls strip — floor/ceil (LogMagU8 params), max-hold/fade/auto-range (UI-only state)`
- [ ] `feat(web): waterfall controls — sensitivity/contrast/scroll-speed (all UI-only)`
- [ ] `test(web): wbfmE2E vitest — boot ferrited with wbfm, click band entry, verify SDR + VFO retuned via API, audio ring drains, FFT frames arrive`

**Done when:** a user loads the UI, picks "100.1" from the FM
broadcast group, hears FM audio, sees the green Nixie on 100.1 MHz
and orange Nixie on 100.1 MHz, can right-click to move the SDR
centre elsewhere, can swap to another preset (e.g. `wbam`) via the
header dropdown without losing the tuning frequency, and a vitest
covers the click-through end to end.

## How this file stays honest

- **PRs update it.** Each PR that lands a commit here crosses that item off
  (or rewrites it if reality differed).
- **No premature promises.** Items in later phases are expected to shift;
  if one moves up, note it in the decision log.
- **Phases 0–E are v0.1.** Phases F and G are the shape of v0.2. Treat the
  latter's items as sketches.
- **One conceptual change per commit.** If an item as written can't be one
  commit, break it into sub-items in the PR that lands it.
