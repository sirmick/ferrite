# 01 — Architecture

## One-line shape

**A thin realtime Rust daemon handles the radio and the wire; a fat browser
frontend handles everything downstream; the same DSP blocks compile to both
sides. Headless decoders run as a second `ferrited` instance with
`--flowgraph <path>` — no browser, no Node sidecar.**

## Diagram

```
           ┌──────────────────────────────────────────────┐
           │  Browser                                     │
           │  ┌────────────────────────────────────────┐  │
           │  │ Svelte UI (Bits UI, Tailwind,          │  │
           │  │ Dockview)                              │  │
           │  └──────────────────┬─────────────────────┘  │
           │                     │                        │
           │  ┌──────────────────▼─────────────────────┐  │
           │  │ FrameClient (main thread)              │  │
           │  │  • decodes postcard Frame              │  │
           │  │  • dispatches by stream_id             │  │
           │  └──────┬──────────────────────┬──────────┘  │
           │         │                      │             │
           │  ┌──────▼──────┐      ┌────────▼──────────┐  │
           │  │ Waterfall   │      │ AudioWorklet +    │  │
           │  │ (WebGL2) +  │      │ browser-side DSP  │  │
           │  │ Spectrum    │      │ (WASM blocks —    │  │
           │  │ (Canvas 2D) │      │ post M5)          │  │
           │  └─────────────┘      └───────────────────┘  │
           │  ┌────────────────────────────────────────┐  │
           │  │ Worker (control plane today;           │  │
           │  │ WASM runtime + blocks post-M5)         │  │
           │  └────────────────────────────────────────┘  │
           └──────────────────────▲───────────────────────┘
                                  │ WS (binary, multiplexed, postcard frames)
                                  │ REST /api (JSON, preset-first)
                                  │
                ┌─────────────────▼──────────────────────┐
                │  ferrited (Rust)                       │
                │  ┌──────────────────────────────────┐  │
                │  │ AppState: FlowgraphDoc +         │  │
                │  │ SourceConfig                     │  │
                │  └────────────────┬─────────────────┘  │
                │                   ▼                    │
                │  ┌──────────────────────────────────┐  │
                │  │ ferrite-runtime scheduler (sync, │  │
                │  │ single-thread, SPSC rings)       │  │
                │  │  ├─ Source (resolved)            │  │
                │  │  ├─ Channelizer / Decimator /    │  │
                │  │  │  FFT / LogMagU8 / Tee …       │  │
                │  │  └─ WsBridgeTx / FileIqSink /    │  │
                │  │     EventsSink                   │  │
                │  └────────────────┬─────────────────┘  │
                │                   ▼                    │
                │  ┌──────────────────────────────────┐  │
                │  │ axum: WS /ws/preset + REST /api  │  │
                │  │ (static-asset serving for web)   │  │
                │  └──────────────────────────────────┘  │
                │                                        │
                │  + SoapySource reader thread (only     │
                │    extra OS thread; pushes into ring)  │
                └────────────────────────────────────────┘
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

Deliberately simple: **one synchronous, single-threaded scheduler**
drives the whole block graph.

- The scheduler (`runtime::Runtime`) walks blocks in pre-computed
  topological order once per `tick`, calling `process()` on each.
  It re-walks up to 1024 passes per tick until no block makes further
  progress. Blocks signal "need more input" by returning
  `Work { consumed: 0, … }` or by overriding `forecast()`.
- Wires between blocks are **power-of-two SPSC rings** (`TypedRing`
  over `SpscRing<T>`). Unconsumed samples persist across ticks, so a
  block that needs an FFT's worth of input just buffers until there
  is enough.
- The **only other OS thread** is `SoapySource`'s reader thread, which
  owns the blocking `stream.read()` call and pushes into an
  `Arc<Mutex<IqRing>>` the scheduler pops from. Everything else runs
  on the scheduler's tick thread.
- `tokio` runs in `ferrited` **only for HTTP/WS plumbing** (`axum`) and
  the outbound WS writer tasks — never for DSP.

Why sync + single-thread: deterministic, easy to reason about,
trivially testable (feed known samples in, assert samples out), and
fast enough for the bandwidths Phase B targets. When a real
multi-channelizer workload exceeds one core, the scheduler grows
parallelism; until then, single-threaded is a feature.

Drops inside the DSP graph are not possible — rings accumulate. Drops
are confined to three explicit, counted boundaries: `SoapySource`
(driver overflow, ring full, timestamp gap), `WsBridgeTx` (lossy-latest
at the network egress — see [02-protocol.md](02-protocol.md) and
[09-decisions.md](09-decisions.md)), and `WsBridgeRx` (bounded ring
on the browser side).

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

- **WebGL2** renders the waterfall via a column-ring R8 texture and a
  `fract(head - v_uv.y)` unwrap shader. The **spectrum** is Canvas 2D
  today (line plot + optional max-hold); moving it onto the same GL
  context is a later unification, not a perf need.
- **`FrameClient`** on the main thread owns the single WebSocket to
  `ferrited`, decodes `postcard`-framed `Frame` values, and dispatches
  by `stream_id` to per-stream subscribers. FFT frames go to the
  waterfall/spectrum, audio-IQ frames go to the AudioWorklet pipeline.
- **Web Worker** exists (`FlowgraphRunner` → worker) but is currently
  **control-plane only**: it hosts the TS flowgraph runtime that takes
  `load/start/stop/state` messages via `postMessage`. Data does **not**
  flow through it — frames come from the server over WS directly to
  the main thread. This is a transition state. The M1–M5 milestone
  replaces the TS worker runtime with a WASM build of the same
  `ferrite-runtime` crate the server uses, at which point browser-side
  blocks run inside the worker and data will flow through it.
- **SharedArrayBuffer** ring buffers carry samples between the
  worker-side runtime (post-M5) and the **AudioWorklet**, avoiding
  per-frame copies. SAB requires
  `Cross-Origin-Opener-Policy: same-origin` +
  `Cross-Origin-Embedder-Policy: require-corp` in both dev and prod —
  already set on both.
- **WebAssembly** will host Rust block implementations built from the
  `ferrite-blocks` crate (same source as server-native). Ported C
  decoders compile via `clang --target=wasm32-unknown-unknown`.
- **OPFS** (planned) for short IQ recordings and local bulk storage.
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

## Flowgraph runtime

The **flowgraph runtime** reads a `FlowgraphDoc` JSON document,
instantiates blocks from the registry, wires their ports together,
validates, and drives them. It lives in the `ferrite-runtime` Rust
crate.

Today there is **one production runtime** — the Rust one, linked into
`ferrited`. The server owns the whole graph; the browser's Worker
currently runs a TS runtime as a transitional scaffold. The M1–M5
milestone (see [project memory](../)) unifies on `ferrite-runtime`
compiled for WASM so the browser runs the **same** runtime source the
server does; at that point there is one runtime, two compile targets,
and TS runtime code is deleted.

A preset names its source as `"type": "Source"` — a **placeholder**
resolved at load time by `compose_source`, which reads the current
`SourceConfig` (held on `AppState`, mutated via `PATCH /api/source`)
and substitutes the real source block. This keeps preset files stable
across device swaps: changing hardware is a `PATCH /api/source` call,
not a preset rewrite.

Environment-specific bits are **sources** (where samples come from) and
**sinks** (where block output goes). Today:

- **Native sources:** `SoapySource`, `FileIqSource`, `SineSource`,
  `DtmfAudioSource`.
- **Native sinks:** `FileIqSink`, `WsBridgeTx` / `WsBridgeTxFftU8`
  (network egress toward the browser), `EventsSink`.
- **Browser sources:** `WsBridgeRx` (subscribes to a `stream_id` on
  `/ws/preset`).
- **Browser sinks:** `AudioSink` (AudioWorklet), plus the implicit
  waterfall/spectrum renderers fed by `ui:<name>` sentinel sinks.

Cross-env wires are rewritten at load time: `env_split` inserts a
`WsBridgeTx` / `WsBridgeRx` pair (or just a `WsBridgeTx` for
`ui:<name>` UI-bound sinks) and allocates a `stream_id` from the
`CROSS_ENV_STREAM_BASE` (1000) range. Authors never hand-wire bridges
or pick ids. See `04-flowgraphs.md` for the full schema.

Example preset skeleton:

```json
{
  "name": "wbfm",
  "environments": ["node", "browser"],
  "blocks": {
    "src":   { "type": "Source",      "placement": "node",
               "params": { "center_freq_hz": 100100000,
                           "sample_rate_hz": 2400000 } },
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

## Process model

### `ferrited` (Rust) — always runs

Single binary. Owns exactly one preset-backed pipeline: the
`FlowgraphDoc` plus its resolved `SourceConfig`, held on `AppState`.
No session IDs, no device-open handle, no per-VFO REST — a VFO is just
a `Channelizer` in the preset. CLI knobs mutate `AppState` before the
scheduler starts (e.g. `--source-type`, `--source-args`,
`--source-bandwidth-hz`, `--agc`, `--start`).

### Headless flowgraph runs

Launch a second `ferrited --flowgraph <preset.json>` on a different
port for "run ADS-B to MQTT with no browser" — same binary, same
blocks, same wire format. No separate sidecar binary.

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

JSON over HTTP. Preset-first surface (full detail in `02-protocol.md`):

- `GET   /api/devices` — enumerate Soapy devices with their capability
  schemas.
- `GET   /api/flowgraph`, `PATCH /api/flowgraph` — read / replace /
  patch the preset; server computes a minimal reconfigure plan.
- `GET   /api/source`, `PATCH /api/source` — the `SourceConfig`
  subresource so device/tuning changes don't re-serialise the whole
  preset.
- `GET   /api/pipeline`, `POST /api/pipeline/{start,stop}` — idempotent
  lifecycle toggles.
- `GET   /api/ui-sinks` — resolve `ui:<name>` sentinel sinks to
  allocated `stream_id`s so the browser finds the FFT stream without
  hard-coding.
- `GET   /api/blocks` — dump registry param schemas (drives the
  generic block-config dialog).
- `POST  /api/identify` — (Phase F) vision + RAG signal identification.

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
  server/                      # `ferrited` Rust binary (cargo workspace member)
  runtime/                     # `ferrite-runtime` — scheduler, graph, loader
  blocks/                      # `ferrite-blocks` — DSP blocks (dual-compile: native + WASM)
  blocks-macros/               # `#[ferrite_block]` proc-macro crate
  flowgraphs/                  # shipped preset JSON (wbfm, wbam, dtmf-e2e, capture_fm)
  web/                         # SvelteKit app (pnpm workspace member)
  headless/                    # (transitional) Node-side workspace
  research/                    # vendored reference sources for decoder ports
  samples/                     # captured IQ + sidecars for offline dev & tests
  soapysdr/                    # user-local Soapy install (gitignored)
  scripts/                     # build-soapysdr etc.
  docs/                        # this tree
  Cargo.toml                   # workspace
  pnpm-workspace.yaml
```

One git history, one CI pipeline. Contributors need both Rust and pnpm
toolchains; that is a conscious choice — the symmetry of shared blocks
is worth it. (Dirs not yet present — `decoders/`, `data/`, `tools/` —
land as their phases do; see `08-roadmap.md`.)
