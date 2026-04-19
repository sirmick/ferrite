# 01 — Architecture

## One-line shape

**A thin realtime Rust daemon handles the radio and the wire; a fat browser
frontend handles everything downstream; the same DSP blocks compile to both
sides. Headless decoders run as a second `ferrited` instance with
`--flowgraph <path>` — no browser, no Node sidecar.**

## Diagram

```
           ┌───────────────────────────────────┐
           │  Browser                          │
           │  ┌───────────────────────────┐    │
           │  │ Svelte UI (Bits UI,       │    │
           │  │ Tailwind, Dockview)       │    │
           │  └─────────────┬─────────────┘    │
           │                │                  │
           │  ┌─────────────▼─────────────┐    │
           │  │ Flowgraph runtime         │    │
           │  │  + block instances        │    │
           │  │  (Workers, WASM blocks,   │    │
           │  │   SAB ring buffers)       │    │
           │  └─────────────┬─────────────┘    │
           │                │                  │
           │  ┌─────────────▼─────────────┐    │
           │  │ AudioWorklet  + WebGL     │    │
           │  │ waterfall/spectrum        │    │
           │  └───────────────────────────┘    │
           └──────────────────▲────────────────┘
                              │ WS (binary, multiplexed)
                              │ REST /api  (JSON control)
                              │
                ┌─────────────▼────────────────┐
                │  ferrited (Rust)             │
                │  ┌────────────────────────┐  │
                │  │ Soapy device I/O       │  │
                │  └───────────┬────────────┘  │
                │              ▼                │
                │  ┌────────────────────────┐  │
                │  │ Wideband FFT           │  │
                │  └───────────┬────────────┘  │
                │              ▼                │
                │  ┌────────────────────────┐  │
                │  │ Channelizer pool       │  │
                │  └───────────┬────────────┘  │
                │              ▼                │
                │  ┌────────────────────────┐  │
                │  │ WS transport +         │  │
                │  │ REST control           │  │
                │  └────────────────────────┘  │
                └───────────────────────────────┘
```

A second `ferrited` with `--flowgraph <preset.json>` covers the
headless case: same runtime, same blocks, node-side sinks (fs, mqtt)
wired into the preset.

## Backend: `ferrited` (Rust + SoapySDR)

One binary. Intended to run on an ARM SBC (Pi-class is the floor; larger boards
just run faster). Responsibilities, end to end:

- **Device lifecycle.** Open a Soapy device, apply sample rate / center freq /
  gain elements / AGC / antenna / driver-specific settings. Close cleanly.
- **Capability introspection.** Probe each device's gain elements, antennas,
  `getSettingInfo` knobs, sample-rate range, frequency range. Expose this as a
  typed JSON schema the frontend renders into an options dialog. No driver-
  specific UI code anywhere.
- **Wideband FFT.** Compute the spectrum the waterfall draws. One FFT on the
  server is cheaper than every tab doing its own. Typically 4k–32k bins at
  20–60 Hz refresh, emitted as compact magnitude frames.
- **Channelizer pool.** For each active VFO, extract and decimate a narrowband
  IQ slice (48–192 kS/s). Ship those slices to the client. This is the bandwidth
  win — the client gets kilobytes per second per VFO instead of megabytes.
- **Transport.**
  - **WebSocket** for streaming (binary frames, multiplexed: waterfall FFT,
    per-VFO IQ, JSON state events).
  - **REST** for control (`/api/devices`, `/api/device/open`, `/patch
    settings`, `/close`, later `/api/identify`).
- **Static asset serving.** The compiled SvelteKit bundle ships inside the
  Rust binary (`rust-embed` or `tower-http`'s `ServeDir`). No Node in prod.
- **Replay mode.** `ferrited --source file://path.iq --rate ... --freq ...
  --loop` replaces device I/O with a file reader. First-class feature, not a
  test-only hack. Enables hardware-free development, deterministic CI, and
  gives users "replay last night's band opening."

Backend explicitly **does not** demodulate or decode. That lives in the
flowgraph runtime.

### Concurrency model

Typical Rust realtime-streaming shape:

- A dedicated thread owns the Soapy read callback and pushes IQ into a
  lock-free SPSC ring buffer.
- A worker thread consumes from that ring, runs the FFT + channelizer stage,
  and publishes fan-out to N per-client ring buffers.
- The tokio runtime handles HTTP + WS; WS writer tasks pull from their
  per-client rings.

Rust's ownership + `Send`/`Sync` make the hand-offs data-race-safe by
construction. `tokio` for async I/O, `crossbeam`/`rtrb` for the rings,
`axum` for HTTP/WS.

### SDRPlay note

The SDRPlay SoapySDR plugin depends on SDRPlay's own `sdrplay_apiService`
daemon. That is a pre-existing, vendor-owned process installed by SDRPlay's
own tooling. Ferrite inherits it when an RSPduo is attached; we do not
architect around it. On non-SDRPlay deployments it simply isn't there.

## Frontend: SvelteKit + Bits UI + Tailwind + Dockview

Built as a static SvelteKit app (`adapter-static`). The output is a `build/`
folder of HTML/JS/CSS/WASM that `ferrited` serves. No Node is running in
production.

### UI layer

- **Svelte 5** for reactivity. Runes (`$state`, `$derived`) are fine-grained
  and match streaming data well (tune cursor, S-meter, frame counters update
  constantly; no VDOM diffs to fight).
- **Bits UI** for headless primitives: select, switch, slider, combobox,
  popover, dialog, tabs, tooltip, toggle-group.
- **Tailwind v4** via its Vite plugin for styling.
- **Dockview** (vanilla TS API) for the multi-panel layout — waterfall,
  controls, audio, decoder output, identify results.
- Custom widgets we own: per-digit frequency dial, S-meter, gain knob,
  passband-drag overlay on the waterfall, interactive spectrum explorer.

### DSP / data path in the browser

- **WebGL** canvas renders the waterfall and spectrum. Driven imperatively
  from a Svelte component (the reactivity model is for state around the canvas,
  not for every frame).
- **Web Workers** host flowgraph pipelines. The main thread never blocks on
  DSP.
- **WebAssembly** hosts block implementations — Rust blocks compiled to WASM,
  ported C decoders compiled via `clang --target=wasm32`.
- **SharedArrayBuffer** ring buffers carry samples between Workers and the
  **AudioWorklet**, avoiding per-frame copies. Requires
  `Cross-Origin-Opener-Policy: same-origin` +
  `Cross-Origin-Embedder-Policy: require-corp` in both dev and prod.
- **OPFS** (Origin Private File System) for short IQ recordings and any local
  bulk storage.
- **localStorage** for prefs and bookmarks.

### Frontend dev stack

- Vite 6 (via SvelteKit), pnpm workspace, TypeScript strict, `svelte-check` in CI.
- `vite-plugin-wasm` + `vite-plugin-top-level-await` for ergonomic WASM imports.
- Vite dev server proxies `/api` and `/ws` to `ferrited` on a separate port, so
  URLs are identical in dev and prod.
- Vitest for unit, Playwright for browser-level tests when needed.

## Blocks: one implementation, two compile targets

Everything in the signal path is a **block**: a typed-port, typed-param unit
with a clear lifecycle. A block:

- Declares its inputs, outputs, and parameters (schemas the flowgraph validator
  can check).
- Has `start`, `process`, `stop` semantics.
- Cannot assume it runs in any particular environment — no file I/O, no
  network, no `console`, no `fs`.

Blocks live in a Rust crate that compiles two ways:

- **Native** (`cargo build`) — objects linked into `ferrited`.
- **WASM** (`wasm-pack build --target web`) — modules loaded by the flowgraph
  runtime in browser Workers and in the Node sidecar.

The same `cargo test` exercises both. Identical fixtures, identical results.
This is why Rust beat C++ for this project: the dual-build story is painless
and the type system protects the concurrent handoffs.

### C/C++ decoder ports

Projects we want to reuse (dump1090 for ADS-B, codec2 for M17 audio, `ft8_lib`
for FT8, etc.) are C. Strategy:

- Vendor the pure-DSP core only. Strip each project's stdio / network / audio
  glue and replace with our block port interface.
- Compile twice: `clang --target=wasm32-unknown-unknown` for the browser/Node;
  `cc` crate (Rust build script) for native linking into `ferrited`.
- Prefer `clang --target=wasm32` over Emscripten — avoids JS-side fs/stdlib
  glue that adds weight and is irrelevant to us.

Per-decoder porting is typically a day's focused work, after which it runs
identically on both sides.

## Flowgraph runtime: shared TS, environment-agnostic

The **flowgraph runtime** is the thing that reads a JSON flowgraph file,
instantiates block instances, wires their ports together, and drives them.

It lives in its own workspace package and depends only on `WebAssembly` and
`Worker` — both available in modern browsers and Node ≥14 (Node 20+ for our
feature floor). Nothing environment-specific is in the core.

Environment-specific bits are **sources** (where samples come from) and
**sinks** (where block output goes):

- **Browser sources:** WS binary stream from `ferrited`.
- **Browser sinks:** AudioWorklet, OPFS file writer, Svelte store update,
  UI event (map marker for ADS-B, message list append, identify-card render).
- **Node sources:** WS binary stream from `ferrited` (loopback).
- **Node sinks:** filesystem, MQTT, syslog, SQLite.

Flowgraphs are JSON:

```json
{
  "name": "wbfm",
  "environments": ["node", "browser"],
  "blocks": {
    "src":   { "type": "SoapySource", "params": { "args": "driver=rtlsdr" } },
    "chan":  { "type": "Channelizer", "placement": "node",    "params": { ... } },
    "demod": { "type": "FmDemod",     "placement": "browser", "params": { ... } },
    "audio": { "type": "AudioSink",   "params": { "buffer_samples": 8192 } }
  },
  "wires": [
    ["src.out",   "chan.in"],
    ["chan.out",  "demod.in"],
    ["demod.out", "audio.in"]
  ]
}
```

Cross-env wires are rewritten at load time: `env_split` inserts a
`WsBridgeTx` / `WsBridgeRx` pair and allocates a `stream_id` from the
`CROSS_ENV_STREAM_BASE` (1000) range. Authors never hand-wire bridges or
pick ids. See `04-flowgraphs.md` for the full schema.

## Process model

### `ferrited` (Rust) — always runs

Single binary. Owns the device, FFT, channelizer pool, WS transport, REST
control, and static file serving. Stateless aside from the current device
session. Configured via a small TOML file (Soapy prefs, LLM API key for the
identify feature).

### Headless flowgraph runs

Launch `ferrited --flowgraph <preset.json>` and the daemon skips the
interactive per-session REST path, loads the preset through the Rust
runtime, and publishes the bridged IQ stream on `/ws/preset`. A
second instance on a different port covers "run ADS-B to MQTT with no
browser" — same binary, same blocks, same wire format.

## Transport

### WebSocket

One connection per browser tab (or second `ferrited --flowgraph`
instance acting as a headless consumer). Binary frames, multiplexed.
Header:

```
 0               1               2               3
 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7 0 1 2 3 4 5 6 7
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
| version       | payload_type  | stream_id                     |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
| seq                                                           |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
| timestamp_ns (64 bits)                                        |
+                                                               +
|                                                               |
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
| payload ...                                                   |
```

See `02-protocol.md` for full details, including `payload_type` values
(`fft_u8`, `fft_f16`, `iq_s16`, `iq_f32`, `json_event`, etc.).

### REST

JSON over HTTP. Small surface:

- `GET  /api/devices` — enumerate with full capability schema.
- `POST /api/device/open` — take ownership; returns `{session_id, ws_url}`.
- `GET  /api/device/{id}/state` — current values.
- `PATCH /api/device/{id}/settings` — mutate while streaming.
- `POST /api/device/{id}/close` — release.
- `POST /api/identify` — (Phase F) vision + RAG signal identification.

## Cross-origin isolation

`SharedArrayBuffer` is how the AudioWorklet consumes DSP output without
per-frame copies. It requires cross-origin isolation, which means both the
dev server and production server must send:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

In dev, a tiny Vite plugin sets these. In prod, `ferrited` sets them in its
static-asset response headers. Forgetting one of these turns SAB into a silent
no-op and the audio path just doesn't work — worth calling out.

## Repository layout

```
ferrite/
  server/                      # Rust backend (cargo workspace member)
  blocks/                      # Rust DSP blocks (cargo + wasm-pack)
  decoders/                    # vendored C sources (dump1090, codec2, ft8_lib)
  packages/
    flowgraph-runtime/         # env-agnostic TS runtime
    flowgraph-blocks/          # TS wrappers around WASM blocks
  web/                         # SvelteKit app (pnpm workspace member)
  flowgraphs/                  # shipped preset flowgraph JSON
  data/                        # generated: sigidwiki.json, band plans
  tools/                       # scrapers, codegen
  docs/                        # this tree
  Cargo.toml                   # workspace
  pnpm-workspace.yaml
```

One git history, one CI pipeline. Contributors need both Rust and pnpm
toolchains; that is a conscious choice — the symmetry of shared blocks is
worth it.
