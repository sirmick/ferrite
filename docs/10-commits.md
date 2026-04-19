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

- [ ] `feat(blocks): AmDemod Rust block (dual-built)`
- [ ] `flowgraphs: add flowgraphs/wbam.json (AM variant of wbfm)`
- [ ] `feat(web): source options dialog — schema-driven, one section per source param group`
- [ ] `feat(web): flowgraph options dialog — schema-driven, one section per non-source block`
- [ ] `feat(web): dialogs gain a read-only JSON tab mirroring live preset`
- [ ] `feat(web): receivers pane with AM/FM dropdown — full chain swap, source stable`
- [ ] `test(web): dialog reconfigure paths map to correct reconfigScope`

**M5 done when:** user picks AM or FM from the receivers pane and the
flowgraph reconfigures without touching the source; source and
flowgraph dialogs render correctly for both presets; VFO0 is the only
wired VFO (with explicit room in the preset schema for VFO1+).

## How this file stays honest

- **PRs update it.** Each PR that lands a commit here crosses that item off
  (or rewrites it if reality differed).
- **No premature promises.** Items in later phases are expected to shift;
  if one moves up, note it in the decision log.
- **Phases 0–E are v0.1.** Phases F and G are the shape of v0.2. Treat the
  latter's items as sketches.
- **One conceptual change per commit.** If an item as written can't be one
  commit, break it into sub-items in the PR that lands it.
