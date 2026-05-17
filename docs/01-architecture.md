# 01 — Architecture

## One-line shape

A thin Rust daemon owns the radio and the wire. The browser owns
demodulation and audio. Both ends run the same Rust runtime
([`runtime/`](../runtime/)) over the same Rust DSP blocks
([`blocks/`](../blocks/)) — natively in `ferrited`, as WASM in a Web Worker.

## Diagram

```
   ┌──────────────────────────────────────────────────────┐
   │  Browser                                             │
   │  ┌─ src/routes/+page.svelte ──────────────────────┐  │
   │  │  Spectrum + Waterfall (WebGL2 / Canvas2D)      │  │
   │  │  Sidebar: bands · catalog · settings · log     │  │
   │  └─────────────────────┬──────────────────────────┘  │
   │  ┌─ src/lib/pipeline.svelte.ts ──────────────────┐   │
   │  │  Live preset/source/blocks store, FrameClient │   │
   │  └────────────────┬─────────────────┬────────────┘   │
   │  ┌─ src/lib/ws ──▼──┐  ┌─ src/lib/runner ────────┐   │
   │  │ FrameClient on   │  │ runnerCore in Worker:   │   │
   │  │ /ws/preset; one  │  │ Rust runtime WASM ticks │   │
   │  │ socket, postcard │  │ blocks; AudioSink fills │   │
   │  │ Frame decoded by │  │ SAB ring drained by     │   │
   │  │ ferrite-blocks   │  │ AudioWorklet            │   │
   │  └──────────────────┘  └─────────────────────────┘   │
   └──────────────────────────▲───────────────────────────┘
                              │  WS /ws/preset (binary, postcard Frame)
                              │  WS /ws/logs   (text)
                              │  REST /api/*   (JSON)
                              │
       ┌──────────────────────▼─────────────────────────┐
       │  ferrited (server/)                            │
       │  ┌─ AppState (server/src/app_state.rs) ─────┐  │
       │  │  preset_doc · source_config · pipeline   │  │
       │  │  device_cache · frames (FrameBus)        │  │
       │  └────────────────┬─────────────────────────┘  │
       │                   ▼                            │
       │  ┌─ ferrite_runtime::Runtime (native) ──────┐  │
       │  │  scheduler · TypedRing · block instances │  │
       │  │  Source-half of cross-env presets        │  │
       │  └────────────────┬─────────────────────────┘  │
       │                   ▼                            │
       │  ┌─ BroadcastSink → FrameBus ───────────────┐  │
       │  │  Stamps seq+ts; mpsc(1024)/subscriber    │  │
       │  └────────────────┬─────────────────────────┘  │
       │                   ▼                            │
       │  ┌─ axum router (server/src/main.rs) ───────┐  │
       │  │  /api/* · /ws/preset · /ws/logs · static │  │
       │  └──────────────────────────────────────────┘  │
       │                                                │
       │  + SoapySource reader thread (only extra OS    │
       │    thread; pushes IQ into a Mutex<Option<>>    │
       │    swapped by the scheduler each tick)         │
       └────────────────────────────────────────────────┘
```

## Backend: `ferrited` (Rust + axum + SoapySDR)

One binary, one process. Builds from [`server/`](../server/). Hard-required
SoapySDR (see `server/Cargo.toml` line 14–19 — no soft-degrade feature flag).

Responsibilities:

- **Hold one preset and its source config.** `AppState`
  ([`server/src/app_state.rs`](../server/src/app_state.rs)) owns
  `preset_doc: RwLock<FlowgraphDoc>`, `source_config: RwLock<SourceConfig>`,
  and `pipeline: Mutex<Option<PresetMount>>`. There is no `SessionState`,
  no per-session id, no "device open" handle — the surface is preset-first.
- **Run the node-side half of the preset.** `PresetMount`
  ([`server/src/preset_pipeline.rs`](../server/src/preset_pipeline.rs))
  composes `preset_doc` + `source_config` via `compose_source`, splits for
  `Environment::Node` via `split_for_environment`, instantiates blocks in
  `ferrite_runtime::Runtime`, and ticks them on a 400 µs default period
  (CLI: `--tick-period-us`).
- **Serve the wire.** axum router in `main.rs:269–297`. REST under `/api/*`,
  binary frame stream on `/ws/preset`, text log stream on `/ws/logs`.
- **Cache device probes.** `DeviceCache`
  ([`server/src/device_cache.rs`](../server/src/device_cache.rs)) keys probes
  by serial (or canonicalized args). Warmed at boot (`main.rs:249`); CLI
  `--probe-all` forces an enumeration + probe pass.
- **Serve the static SvelteKit bundle.** `tower_http::ServeDir` from
  `FERRITE_STATIC_ROOT` (default `./web-dist`), with COOP/COEP set on every
  response (`main.rs:313–314`).

Backend does *not* demodulate or decode. The only DSP `ferrited` runs is
whatever blocks land on the node side after `split_for_environment` — for
the shipped presets that's `SoapySource`/`Source` → `TeeIqF32` → either an
`FFT → LogMagU8 → ui:fft` tap or a `Channelizer` feeding the cross-env wire.

## Concurrency model

- **One scheduler thread per pipeline.** `Runtime::tick()`
  ([`runtime/src/runtime.rs`](../runtime/src/runtime.rs)) walks blocks in
  topological order. Each tick re-walks until no block makes progress
  (`MAX_TICK_PASSES = 1024`); sources fire at most once per tick.
  `Work.consumed[]` advances reader pointers on per-wire `TypedRing`s; a
  starved reader simply doesn't run that pass.
- **Per-wire accumulating ring buffers.** `TypedRing`
  ([`runtime/src/typed_ring.rs`](../runtime/src/typed_ring.rs)) is a
  power-of-two SPSC/SPMC buffer with one writer head and N reader heads.
  Unconsumed samples persist across ticks — this is what lets the FFT block
  accumulate a full N-sample frame across many small input batches and what
  lets the AM modulator's 300× upsample (DTMF canary) actually work
  end-to-end (D23).
- **One extra OS thread: SoapySource's reader.**
  [`blocks/src/soapy_source.rs`](../blocks/src/soapy_source.rs) owns the
  blocking `RxStream::read()` loop and overwrites a `Mutex<Option<Vec<…>>>`
  with the latest batch; the scheduler swaps it out each tick. Nothing else
  runs off the tick thread.
- **`tokio` is for HTTP/WS only.** `Runtime::tick()` is sync; the axum
  handlers and the `/ws/*` writer tasks are the only `tokio` users.

Drops inside the graph are not possible — rings accumulate. Drops happen at
two explicit edges:

- **`SoapySource`** when the reader can't keep up (latest-value slot is
  always overwritten by design — there is no buffering past `fft_size`).
- **Per WS subscriber** when the per-client `mpsc(1024)` queue fills.
  `BroadcastSink` ([`server/src/bridge_sink.rs`](../server/src/bridge_sink.rs))
  + `FrameBus` ([`server/src/frame_bus.rs`](../server/src/frame_bus.rs))
  drop only the slow subscriber's copy and log a warning; other subscribers
  keep receiving and the scheduler never blocks. Drops surface as `seq` gaps
  on the affected subscriber's stream.

## Frontend: SvelteKit + Bits UI + Tailwind

Built as a static SvelteKit app (`adapter-static`). Output lives in
`web/build/`; `ferrited` serves it. No Node runtime in production.

Single page at [`web/src/routes/+page.svelte`](../web/src/routes/+page.svelte):
fixed spectrum-over-waterfall layout (D26) with a left sidebar (Bands /
Catalog / Settings tabs + a server-log mirror panel). Header carries the
preset selector, a Source dialog, a Flowgraph dialog, and Start/Stop.

### Stores and dispatch

- [`src/lib/pipeline.svelte.ts`](../web/src/lib/pipeline.svelte.ts) is the
  central store. It holds the live `FlowgraphDoc`, `SourceConfig`, the
  composed `PipelineBlock[]` (from `GET /api/pipeline/blocks`), and UI-sink
  allocations (from `GET /api/ui-sinks`). It opens the `FrameClient` on
  init.
- `setBlockParam(id, key, value)` (D24) is the one dispatcher: branches on
  `pipeline.blocks[id].placement` and routes to either
  `POST /api/pipeline/blocks/:id/params` (server-side blocks like
  `Channelizer`, `LogMagU8`, the `Source` block) or
  `RuntimeHandle.reconfigure_block(id, deltaJson)` on the in-Worker WASM
  runtime (browser-side blocks like `AudioSink`, `FmDemod`).

### Browser runtime + audio

- [`src/lib/runner/`](../web/src/lib/runner/) hosts the Rust WASM runtime in
  a dedicated Web Worker. `runnerCore.ts` instantiates a `RuntimeHandle`
  ([`runtime/src/wasm.rs`](../runtime/src/wasm.rs)), wires `WsBridgeRx`
  blocks to incoming frames by `stream_id`, and ticks the graph on a fixed
  cadence; `AudioSink` writes into a SAB ring drained by an AudioWorklet on
  the audio thread; browser-side `__ui_*` `EventsSink`s are drained each
  tick and loopbacked to the main thread (`kind:'events'` worker message
  → `FrameClient.injectLocal`). `runnerWorker.ts` is the Worker entry;
  `runnerClient.ts` is the main-thread half.
- fldigi modems are C++/STL and don't cross-compile to
  `wasm32-unknown-unknown`. The Rust wasm leaves their `extern "C"` ABI
  as undefined imports; `initFldigiBridge()` (lazily, only when a
  fldigi block is browser-placed) instantiates a sibling Emscripten
  module and publishes it on `globalThis.__FERRITE_FLDIGI__` for the
  wasm-bindgen snippet to call into. FT8/WSPR cross-compile directly
  (their slot clock uses `web_time` so it works on wasm32).
- [`src/lib/audio/ringBuffer.ts`](../web/src/lib/audio/ringBuffer.ts) +
  [`audioRingProcessor.ts`](../web/src/lib/audio/audioRingProcessor.ts) — the
  SAB ring (Uint32 head/tail header + Float32 ring). Requires
  cross-origin-isolation; without COOP/COEP, SAB silently degrades and
  audio dies.

### WS transport

- One WebSocket to `/ws/preset`, multiplexed by `stream_id`.
  [`src/lib/ws/client.ts`](../web/src/lib/ws/client.ts) opens it;
  [`frame.ts`](../web/src/lib/ws/frame.ts) calls `decodeFrame` exported from
  the `ferrite-blocks` WASM crate, so server and browser cannot disagree on
  the schema without a build break (D02).
- A second WebSocket to `/ws/logs` (text) mirrors the server's `tracing`
  output into the LogPanel.

### Vite / dev

[`web/vite.config.ts`](../web/vite.config.ts) wires `coop-coep`, `wasm`,
`top-level-await`, `tailwindcss`, and `sveltekit` plugins. Dev server proxies
`/api` and `/ws` to `FERRITED_URL` (default `http://127.0.0.1:8088`). Worker
plugin list mirrors the main one so the runner Worker can also load WASM.

## Runtime and blocks: shared between both ends

Both ends instantiate the *same* `ferrite_runtime::Runtime` over the *same*
`ferrite_blocks::Block` instances; only the compile target differs.

- A preset declares `environments: ["node", "browser"]`. `split_for_environment`
  ([`runtime/src/env_split.rs`](../runtime/src/env_split.rs)) carves the doc
  into an env-local subgraph. Wires that crossed the boundary become a
  `WsBridgeTx`-on-node + `WsBridgeRx`-on-browser pair sharing a `stream_id`
  starting at `CROSS_ENV_STREAM_BASE = 1000`.
- A wire terminating in `ui:<name>` (a decoder's `events`/FFT/audio
  stream feeding a UI panel) is allocated the same `stream_id` in both
  halves. When the producer is **node-side**, it becomes a
  `WsBridgeTx*` and the browser learns the id via `GET /api/ui-sinks`.
  When the producer is **browser-side** (a decoder placed or swapped
  into the browser), an `Events` `ui:` wire instead terminates in a
  drainable `EventsSink` (`__ui_<name>_<sid>`); the runner drains it
  each tick and loopbacks the JSON into the same `FrameClient` the
  store subscribes to (`injectLocal`). Net: every decoder block is
  placement-`Either`, the demod-placement chip moves it node↔browser
  live with no reload, and the decode reaches the UI **identically**
  whichever side ran it — the transport divergence never leaks above
  the runner. (D28; see [09-decisions.md](09-decisions.md).)
- Browser→node crossings are rejected (`SplitError::UnsupportedCrossing`).
- The Source block is a placeholder: presets author it as `"type": "Source"`
  with tuning hints; `compose_source`
  ([`runtime/src/compose.rs`](../runtime/src/compose.rs)) overlays the
  current `SourceConfig` (e.g. `{ "type": "SoapySource", "params": { … } }`)
  before validation. Swapping device is `PATCH /api/source`, not a preset
  rewrite.

The block trait, port types, `Frame` enum, `ParamSpec`, and
`ReconfigureScope` all live in [`blocks/src/block.rs`](../blocks/src/block.rs)
and [`blocks/src/frame.rs`](../blocks/src/frame.rs). See
[03-blocks.md](03-blocks.md) for the surface and the registered block list.

## Cross-origin isolation

`SharedArrayBuffer` carries audio between the runner Worker and the
AudioWorklet. SAB requires:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

Both are set in dev by [`web/src/lib/vite/coop-coep.ts`](../web/src/lib/vite/coop-coep.ts)
and in prod by `ferrited` (`server/src/main.rs:313-314`). Forgetting either
makes SAB silently unavailable and the audio path stops working — there is
no fallback.

## Repository layout

```
ferrite/
  server/                  # ferrited — axum daemon
  runtime/                 # ferrite-runtime — scheduler, doc, env_split, wasm facade
  blocks/                  # ferrite-blocks — Block trait + DSP blocks
  blocks-macros/           # #[ferrite_block] proc-macro
  flowgraphs/              # shipped preset JSON (wbfm, wbam, dtmf-e2e, …)
  web/                     # SvelteKit app
  scripts/                 # build-soapysdr.sh
  docs/                    # this tree
  Cargo.toml               # cargo workspace (server, blocks, blocks-macros, runtime)
  pnpm-workspace.yaml      # pnpm workspace (web, tools/*)
```

`tools/*` is declared in the pnpm workspace config but currently empty.
