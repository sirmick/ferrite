# 02 — Control API and WS frame format

Two surfaces:

- **REST under `/api/*`** — JSON over HTTP. Enumeration, control, lifecycle.
- **WebSocket under `/ws/*`** — `/ws/preset` (binary postcard frames),
  `/ws/logs` (text log lines).

All routes are wired in
[`server/src/main.rs:269-297`](../server/src/main.rs); handlers in
[`server/src/routes.rs`](../server/src/routes.rs).

The server holds **one** preset and **one** source config (no per-session
state). Clients GET the current state, PATCH/POST to change it, then connect
to `/ws/preset` to receive sample frames. There is no auth on any endpoint
(D07).

## Lifecycle

```
client ── GET   /api/devices                            ─► [DeviceEntry]
client ── GET   /api/presets                            ─► [PresetEntry]
client ── POST  /api/preset {name}                      ─► swaps active preset
client ── GET   /api/flowgraph                          ─► FlowgraphDoc
client ── GET   /api/source                             ─► SourceConfig
client ── GET   /api/source/capabilities                ─► hw|sw|unavailable
client ── GET   /api/pipeline/blocks                    ─► [PipelineBlock]
client ── GET   /api/ui-sinks                           ─► [UiSink]
client ── WS    /ws/preset                              ─► binary Frame stream
client ── POST  /api/pipeline/start                     ─► {status:"running"}
client ── PATCH /api/source       {SourceConfig}        ─► ReconfigureResponse
client ── PATCH /api/flowgraph    {FlowgraphDoc}        ─► ReconfigureResponse
client ── POST  /api/pipeline/blocks/{id}/params {...}  ─► ReconfigureResponse
client ── POST  /api/pipeline/stop                      ─► {status:"stopped"}
client ── WS    /ws/logs                                ─► text log lines
```

A patch made while the pipeline is stopped is stored against `AppState` and
takes effect on the next `start` (the response carries `applied: false`).

## REST endpoints

### `GET /api/hello`

`{ app: "ferrited", version, status: "ok" }`. Liveness probe.

### `GET /api/devices`

Enumerate every attached SoapySDR device and probe each for capabilities.
Probes go through the per-process `DeviceCache`
([`server/src/device_cache.rs`](../server/src/device_cache.rs)) — only the
first hit per device touches the driver; entries for devices that disappear
are pruned.

Response: `Vec<DeviceEntry>`, where each is one of:

```json
{ "status": "available",
  "driver": "rtlsdr", "label": "...", "serial": "...",
  "args": { "driver":"rtlsdr", "serial":"..." },
  "driver_key": "...", "hardware_key": "...", "hardware_info": { ... },
  "rx_channels": [
    { "index": 0,
      "antennas": ["RX"],
      "sample_rate_ranges_hz": [{ "min": 250000, "max": 3200000 }],
      "bandwidth_ranges_hz":   [...],
      "frequency_ranges_hz":   [{ "min": 24e6, "max": 1.766e9 }],
      "frequency_components":  [{ "name": "RF", "ranges_hz": [...] }],
      "gains":                 [{ "name": "TUNER", "range_db": { "min": 0, "max": 49.6, "step": 0.1 } }],
      "overall_gain_range_db": { "min": 0, "max": 49.6, "step": 0.1 },
      "has_agc": true }
  ],
  "settings": [
    { "key": "biastee", "label": "Bias-T", "data_type": "bool",
      "default": "false", "options": ["false","true"] }
  ]
}
```

```json
{ "status": "unavailable",
  "info": { "driver": "...", "label": "...", "serial": "...", "args": {...} },
  "error": "..." }
```

The exact struct is `crate::device::DeviceCapabilities`
([`server/src/device.rs`](../server/src/device.rs)).

### `GET /api/source/capabilities`

Probe the *currently-configured* source rather than enumerate everything.
Tagged response (`SourceCapabilitiesResponse` in `routes.rs:340`):

- `{ kind: "hardware",   type_name: "SoapySource",   capabilities: {...} }`
- `{ kind: "software",   type_name: "SineSource"    }`
- `{ kind: "unavailable",type_name: "...", error: "..." }`

Used by the source dialog to hide hardware-only controls when a software
source (`SineSource`, `FileIqSource`) is active.

### `GET /api/flowgraph` · `PATCH /api/flowgraph`

GET returns the current `FlowgraphDoc` verbatim. PATCH replaces it; the body
is a complete `FlowgraphDoc`. The response is a `ReconfigureResponse`:

```json
{ "applied": true,
  "overall": "self" | "downstream" | "sourceRestart",
  "changes": [
    { "block_id": "demod", "param_key": "deemphasis_us",
      "old_value": 75, "new_value": 50, "scope": "downstream" }
  ],
  "structural_count": 0,
  "noop": false }
```

`applied: false` means the patch was stored but no pipeline was running.
On apply failure the previous doc is retained and an error returned.

### `GET /api/source` · `PATCH /api/source`

Just the `src` placeholder's resolved config:

```json
{ "type": "SoapySource",
  "params": { "args": "driver=rtlsdr", "sample_rate_hz": 2400000,
              "center_freq_hz": 100100000, "bandwidth_hz": 2000000,
              "gain_db": 30.0, "agc": false } }
```

PATCH body shape mirrors GET. Same `ReconfigureResponse` as above. This is
the right surface for "swap to a different SDR" or "tune the centre"; it
doesn't re-serialise the whole flowgraph.

### `GET /api/pipeline` · `POST /api/pipeline/{start,stop}`

GET returns `{ status: "running" | "stopped" }`. POSTs are idempotent and
return the new status.

### `GET /api/pipeline/blocks`

Every block in the currently-composed preset, including the resolved source
and any auto-inserted bridges. Each entry carries the block's `BlockSpec`
plus its current param values:

```json
[
  { "id": "src", "type_name": "SoapySource", "placement": "native",
    "spec":   { ... },
    "values": { "args": "driver=rtlsdr", "center_freq_hz": 100100000, ... } }
]
```

This is what feeds the generic `<BlockParams>` Svelte component — adding a
block makes its params editable in the UI without per-block frontend code.

### `POST /api/pipeline/blocks/{id}/params`

Apply a delta to one block. Body is a JSON object of `{ key: new_value }`
pairs. Writes to `id == "src"` route to the source patch path; everything
else routes through `PATCH /api/flowgraph` internally. Same
`ReconfigureResponse` as above.

### `GET /api/blocks`

Every *registered* block type's schema (sorted by `type_name`). Static —
doesn't depend on the active preset. The dialog's source-type and
flowgraph-type pickers render from this. See
[`server/src/block_schema.rs`](../server/src/block_schema.rs) for the DTO
shape; `reconfig_scope` is wire-encoded as `"self"`, `"downstream"`, or
`"sourceRestart"`.

### `GET /api/ui-sinks`

Every `ui:<name>` sink in the currently-composed preset, with the
`stream_id` `env_split` allocated:

```json
[ { "name": "fft", "stream_id": 1001, "payload_type": "fft_u8" } ]
```

The browser uses this to subscribe to the right stream without hard-coding
ids.

### `GET /api/presets` · `POST /api/preset`

GET enumerates `*.json` files in the configured presets directory
(`--presets-dir`, auto-detected when `--flowgraph` is inside a `flowgraphs/`
sibling). POST takes `{ name: "wbfm" }` and atomically swaps the active
preset; the running pipeline is hot-reconfigured if possible. Names are
restricted to `[A-Za-z0-9_-]+` so lookups can't escape the presets dir.

Response: `{ name: "wbfm", reconfigure: ReconfigureResponse }`.

## Error model

Failures return non-2xx + a JSON body:

```json
{ "error": { "code": "RECONFIGURE_FAILED", "message": "..." } }
```

Codes seen in `routes.rs`: `DEVICE_PROBE_FAILED`,
`LIST_PIPELINE_BLOCKS_FAILED`, `RECONFIGURE_FAILED`, `LIST_PRESETS_FAILED`,
`LOAD_PRESET_FAILED`. The free-form message is the formatted error chain.

## WebSocket: `/ws/preset`

One connection per client; binary messages only. Each message is a
postcard-encoded `Frame` from
[`blocks/src/frame.rs`](../blocks/src/frame.rs):

```rust
pub enum Frame {
    IqF32     { stream_id: u16, seq: u32, timestamp_ns: u64, payload: Vec<u8> },
    FftU8     { stream_id: u16, seq: u32, timestamp_ns: u64, payload: Vec<u8> },
    JsonEvent { stream_id: u16, seq: u32, timestamp_ns: u64, payload: Vec<u8> },
}
```

| variant     | `payload`                                                    |
|-------------|--------------------------------------------------------------|
| `IqF32`     | interleaved `f32` I,Q little-endian (8 bytes/sample)         |
| `FftU8`     | log-magnitude bins as `u8` 0..=255                           |
| `JsonEvent` | UTF-8 JSON                                                   |

The browser decodes via `decodeFrame` exported from `ferrite-blocks`
(`web/src/lib/ws/frame.ts`). Server and browser use the same crate, so
schema changes are build breaks rather than wire ambiguities.

The connection survives pipeline start/stop. Subscribe before starting and
frames arrive as soon as the pipeline spins up.

### Stream ids

Constants in `frame.rs`:

```
CONTROL_STREAM   = 0   // reserved for future JsonEvent control traffic
FFT_STREAM       = 1   // legacy direct-FFT path
VFO_STREAM_BASE  = 2
```

In practice today, allocation is driven by `env_split` and starts at
`CROSS_ENV_STREAM_BASE = 1000`
([`runtime/src/env_split.rs`](../runtime/src/env_split.rs)). Both `ui:<name>`
sinks and node→browser cross-env wires draw from this counter, in
declaration order. Allocation is deterministic — server and browser arrive
at the same numbers without negotiation.

The browser learns ids two ways:

- **For UI sinks:** `GET /api/ui-sinks` (above).
- **For cross-env wires:** by calling `RuntimeHandle::split_doc_for_environment`
  on the same preset doc in WASM — the browser's `WsBridgeRx` instances land
  with the matching `stream_id` param.

### Stamping & ordering

The block-side producer builds a `Frame` with `seq = 0` and
`timestamp_ns = 0`. `BroadcastSink`
([`server/src/bridge_sink.rs`](../server/src/bridge_sink.rs)) overwrites
both from per-`(variant, stream_id)` counters before encoding. `seq`
increments by 1 per frame and wraps at `u32::MAX`; gaps signal drops.

### Backpressure

`FrameBus` ([`server/src/frame_bus.rs`](../server/src/frame_bus.rs)) fans
each frame to every `/ws/preset` subscriber through a per-subscriber
bounded `mpsc::channel(1024)`. `send` is non-async and uses `try_send` —
the scheduler thread never blocks. A full subscriber queue drops *only that
subscriber's* copy and logs `frame bus: subscriber full`; other subscribers
keep receiving uninterrupted. From the affected subscriber's view this
shows up as `seq` gaps.

## WebSocket: `/ws/logs`

Text-only stream of `tracing` lines from the server, in the format

```
[INFO] preset_pipeline: starting preset name=wbfm
```

Used by the LogPanel ([`web/src/lib/layout/LogPanel.svelte`](../web/src/lib/layout/LogPanel.svelte))
and `web/src/lib/logs/`. Lagged subscribers see synthetic
`[WARN] log stream lagged by N lines`. Returns 503 if the log broadcaster
isn't installed (it always is in `main`, so 503 only surfaces in stripped
test harnesses).

## Cross-origin isolation

Every response from the static-asset layer carries

```
Cross-Origin-Opener-Policy:   same-origin
Cross-Origin-Embedder-Policy: require-corp
```

Set in `server/src/main.rs:313-314`. The dev-server equivalent is in
[`web/src/lib/vite/coop-coep.ts`](../web/src/lib/vite/coop-coep.ts).
`SharedArrayBuffer` (the audio ring) silently degrades without these.

## Security

LAN-trust, no auth on any endpoint. Anything beyond the LAN is the user's
tunnel problem. See [09-decisions.md D07](09-decisions.md).
