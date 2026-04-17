# 08 — Roadmap

## Shape of the plan

v0.1 is the first shippable thing: **one SDR, one browser, clean WBFM
listening and one real decoder (likely ADS-B)**. Everything is organized
in phases that each end on a **demo-able** state, because a project that
can demo every few weeks stays honest about what works.

Each phase lists a crisp definition of "done" — the moment at which it is
legitimate to start planning the next phase.

| phase | name                             | status       | demo gate                                                        |
|-------|----------------------------------|--------------|------------------------------------------------------------------|
| 0     | Docs-first                       | in progress  | this docs tree exists, plan frozen                               |
| A     | Scaffolding                      | next         | "hello WS" ping-pong over the real stack; CI green               |
| B     | Synthetic data path E2E          | planned      | synthetic waterfall visible in browser, no hardware              |
| C     | First real device                | planned      | RTL-SDR + SDRPlay enumerate, open, stream; options dialog works  |
| D     | First listening experience       | planned      | tune to a local FM station, hear audio, drag VFO on waterfall    |
| E     | First decoder                    | planned      | ADS-B messages decoded live from a real receiver                 |
| F     | LLM signal identify              | post-v0.1    | drag waterfall box → "that's APRS" with sigidwiki deep-link      |
| G     | Spectrum allocation explorer     | post-v0.1    | full-spectrum zoomable chart, click-to-retune                    |

Phases A–E are **v0.1**. Phases F and G are the shape of v0.2, sketched
here so decisions in v0.1 don't paint them into a corner.

## Phase 0 — Documentation

No code. Capture the architecture in this `docs/` tree and a top-level
README before implementation starts. The goal is that a contributor
(or future-you) can orient without reading chat history.

**Done when:** every numbered doc in `docs/` exists and is internally
consistent with the others, and the README points into them.

Deliverables:
- `README.md`
- `docs/00-context.md` through `docs/10-commits.md`

## Phase A — Scaffolding

Get the real build topology in place with the smallest possible payload
flowing through it. Hello-world, but through the shape we actually ship.

**Done when:**
- Cargo workspace with `server/` and `blocks/` members compiles.
- `ferrited` serves `GET /api/hello` over HTTP and echoes on `/ws`.
- pnpm workspace with `web/` (SvelteKit + Bits UI + Tailwind + Dockview)
  builds and runs in dev against the Rust backend (Vite proxy for
  `/api` and `/ws`).
- COOP/COEP headers set in both dev (Vite plugin) and prod
  (`ferrited` static serve).
- A trivial Rust→WASM block built with `wasm-pack` is imported via
  `vite-plugin-wasm` and called from a Web Worker to prove the path.
- CI (GitHub Actions) runs `cargo check/test/clippy/fmt` and
  `pnpm test/lint/check` on every PR and merge.
- Pre-commit hooks enforce format + lint.

**Demo:** load the page, see a "connected" indicator, see the WASM
block's output logged. No signals yet. This is the hardest phase to
feel impressive; it's also the one that unlocks every future phase.

## Phase B — Synthetic data path E2E

Prove the full data path — block → WS → WebGL — with zero hardware.
Replay mode lands here so it stays first-class from day one.

**Done when:**
- `blocks/` crate has `SignalSource`, `FFT`, `Decimator` with unit tests
  passing native and in WASM. Fixtures identical in both.
- `ferrited` runs a hardcoded `SignalSource → FFT → WS` pipeline.
- Binary WS frame codec implemented per `docs/02-protocol.md`.
- Frontend WebGL waterfall + spectrum renders the FFT stream.
- `ferrited --source file://path.iq --loop` replays a recorded IQ file
  through the same pipeline.
- Wire-protocol conformance test harness running in CI.
- AudioWorklet `process()` unit-tested in Vitest.
- SAB ring-buffer stress test running in headless Chromium in CI.

**Demo:** load the page, see a moving waterfall of a synthetic chirp,
then rerun with `--source file://wbfm_fragment.iq` and see the FM
waterfall without an SDR connected.

## Phase C — First real device

Plug in hardware. SoapySDR integration, capability introspection,
REST endpoints, auto-generated options dialog.

**Done when:**
- `soapysdr` Rust bindings linked into `ferrited`.
- `GET /api/devices` enumerates with full capability schema (rates,
  freq ranges, named gain elements, antennas, all `getSettingInfo` keys).
- `POST /api/device/open`, `GET …/state`, `PATCH …/settings`,
  `POST …/close` implemented with session semantics (last-connect wins).
- Frontend options dialog auto-generated from the capability schema
  using Bits UI primitives.
- `mutableWhileStreaming: false` settings show "Apply & restart stream"
  in the UI; backend does clean close+reopen preserving `session_id`.
- WS streams real FFT from the real device.
- RTL-SDR and SDRPlay RSPduo both work. RSPduo's two tuners each appear
  as distinct devices in enumeration.

**Demo:** plug in an RTL-SDR, open the page, pick the device, configure
bias-tee and gain, see real radio noise on the waterfall. Swap in the
SDRPlay, repeat.

## Phase D — First listening experience

The first time a user says "I heard it." Channelizer, minimal flowgraph
runtime, FM demod, audio out.

**Done when:**
- `Channelizer` block runs server-side and produces per-VFO narrowband
  IQ on dedicated stream IDs.
- REST endpoints `POST/PATCH/DELETE /api/device/{id}/vfo` land.
- Flowgraph runtime in `packages/flowgraph-runtime/`: JSON parse →
  validate → instantiate blocks from registry → wire → run in a Worker.
- `FmDemod` block (Rust, dual-built).
- AudioWorklet consumes from a SAB ring filled by the Worker's
  `AudioSink`.
- `flowgraphs/wbfm.json` loads, runs, and produces clean audio.
- Per-digit frequency dial widget.
- Drag VFO cursor on the waterfall to retune mid-stream.
- Golden-fixture CI test: replay-mode `ferrited` + the WBFM flowgraph
  → audio RMS and a known pilot tone match within tolerance.
- (Optional) `headless/` skeleton that can run the same flowgraph
  headlessly — no sinks wired yet, just "it compiles and instantiates."

**Demo:** open the page, tune to a local FM broadcast, hear music. Drag
the VFO down the band, listen to other stations.

## Phase E — First decoder

Port a C decoder, wrap it as a block, wire it through a flowgraph, show
decoded output in the UI. The likely candidate is **dump1090** for ADS-B
(smallest C surface to port cleanly).

**Done when:**
- Pure-DSP core of the chosen decoder vendored under `decoders/`,
  stripped of I/O glue.
- Native build via `cc` crate; WASM build via
  `clang --target=wasm32-unknown-unknown` + wasi-libc.
- Thin Rust `Block` wrapper implements the block trait on both targets.
- `flowgraphs/adsb.json` loads and runs: WsIqSource → AdsbDecoder →
  EventBusSink → UI.
- A message list panel renders decoded frames (Bits UI, virtualized).
- Golden-fixture CI test: known ADS-B burst decodes to known hex.
- Map panel (MapLibre or similar) plots aircraft from decoded position
  reports. (Map is optional for v0.1 but on the critical path for
  "looks real.")

**Demo:** tune to 1090 MHz with the RTL-SDR, see aircraft positions
updating live.

### Why ADS-B first

- dump1090 core is a small, well-understood C codebase.
- No patent / licensing drama (compare AMBE for DMR).
- Decoded output is visually compelling (moving aircraft on a map) —
  motivates the team and earns trust with users.
- Verifies the full C→WASM port pipeline on something that matters.

FT8 and M17+codec2 follow the same port shape; they land post-v0.1.

## Phase F — LLM signal identify (post-v0.1)

User drag-selects a region on the waterfall; the app asks an LLM what
it might be, grounded by a RAG index over scraped sigidwiki content.

Scope:
- `tools/scrape-sigidwiki/` — MediaWiki API client producing
  `data/sigidwiki.json` (checked into repo, CC-BY-SA attributed).
- `GET/POST /api/identify` — frontend posts `{ png, center_freq, span,
  resolution_bw, timestamp, iq_clip? }`; backend retrieves top-N
  sigidwiki candidates by frequency+bandwidth match, composes a vision
  prompt, calls the configured LLM.
- Result card in the UI with `{ best_guess, candidates: [{name, url,
  summary}] }` and deep-links into sigidwiki.
- Signal catalog panel: searchable view over `sigidwiki.json`.

Backend proxies the LLM call (API key lives in `ferrited.toml`, not in
browser storage). Identical requests can be cached server-side.

### v0.1 carryover

- The `POST /api/identify` route and its request/response shape are
  sketched in Phase D so the frontend can stub against them early.
- No actual LLM call in v0.1; the endpoint returns a clear "not yet
  implemented" response.

## Phase G — Spectrum allocation explorer (post-v0.1)

Full-spectrum-at-a-glance Dockview panel inspired by spectrumwiki.com.
Log-scale axis from ~0 Hz to 300 GHz. Smooth wheel zoom. Click-to-retune
(with "out of range of current device" state for unreachable bands).

Scope:
- `data/spectrum-allocations.json` — static data assembled from ITU /
  FCC / ARRL public sources, checked in.
- "Band allocation" tooltip on the tuned frequency (lands in v0.1).
- Full zoomable explorer as its own Svelte component (SVG with
  virtualization; WebGL if band density demands it) — post-v0.1.

### v0.1 carryover

- The static allocation data format is chosen in v0.1 (one tooltip
  consumer exists) to avoid a later migration.

## Consciously deferred

These are not "maybe never." They are "not in v0.1" — the reasons are
logged in `docs/09-decisions.md`.

- **Multi-listener / multi-device.** Channelizer is pool-architected so
  this is an extension, not a rewrite. Still out for v0.1.
- **Server-side long-form recording.** OPFS (browser) covers short;
  replay mode covers "test against what I captured."
- **`ferrite-headless` as a v0.1 deliverable.** The architecture admits
  it from day one; whether it *ships* in v0.1 is a separate call.
  Default: follow-up. The runtime package ships in v0.1 either way.
- **Additional decoders** (APRS, FT8, M17+codec2) — land alongside
  their respective blocks post-v0.1. Candidate list is frozen; DMR is
  ruled out (AMBE licensing).
- **Remote access / auth.** v0.1 is LAN-trust. Tailscale or a
  user-operated tunnel covers remote access today; server-side auth is
  a separate design project.
- **Mobile / tablet UI.** Desktop-first. The layout engine (Dockview)
  does not claim mobile; a separate mobile view is post-v0.1 if at all.

## Release criteria for v0.1

All of the Phase A–E "done when" checklists pass, plus:

- `cargo test --workspace` and `wasm-pack test` green.
- `pnpm -r test` and `pnpm --filter web test:e2e` green.
- Wire-protocol conformance test green.
- SAB ring-buffer stress test green in CI.
- Golden-fixture tests for WBFM and ADS-B green.
- Manual smoke on a Pi 5 with both RTL-SDR and SDRPlay RSPduo:
  enumerate, open, tune FM, hear audio, tune ADS-B, see aircraft.
- Documentation reflects shipped behavior (not just the plan).
