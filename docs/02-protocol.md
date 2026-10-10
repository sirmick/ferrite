# 02 — Control API and WS frame format

Three surfaces:

- **REST under `/api/*`** — JSON over HTTP. Enumeration, control, lifecycle.
  The only control plane.
- **WebSocket `/ws/preset`** — binary postcard sample frames.
- **MQTT (the event plane)** — every observable change as a record on a
  topic, from an embedded broker (`/mqtt` over WebSocket for browsers, a
  plain TCP port for everything else). See [Event plane](#event-plane-mqtt).

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
client ── POST  /api/devices/reload                     ─► soapy module reload (in-proc recovery)
client ── GET   /api/presets                            ─► [PresetEntry]
client ── POST  /api/preset {name}                      ─► swaps active preset
client ── GET   /api/flowgraph                          ─► FlowgraphDoc
client ── GET   /api/source                             ─► SourceConfig
client ── GET   /api/source/capabilities                ─► hw|sw|unavailable
client ── GET   /api/pipeline/blocks                    ─► [PipelineBlock]
client ── GET   /api/ui-sinks                           ─► [UiSink]
client ── WS    /ws/preset                              ─► binary Frame stream
client ── POST  /api/pipeline/start                     ─► {status:"running"}
client ── POST  /api/tune  {freq_hz, span_hz?, offset_ratio} ─► ReconfigureResponse
client ── PATCH /api/source       {SourceConfig}        ─► ReconfigureResponse
client ── PATCH /api/flowgraph    {FlowgraphDoc}        ─► ReconfigureResponse
client ── POST  /api/pipeline/blocks/{id}/params {...}  ─► ReconfigureResponse
client ── POST  /api/pipeline/stop                      ─► {status:"stopped"}
client ── MQTT  ferrite/state/#                         ─► every record (graph, status, decodes, logs…)
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

### `POST /api/devices/reload`

In-process recovery when a SoapySDR driver wedges inside `ferrited`
itself — external `SoapySDRUtil --find` works, but our enumerate hangs
or misses devices. The handler calls `SoapySDR_unloadModules` +
`SoapySDR_loadModules` via raw FFI (the safe `soapysdr` crate doesn't
expose it; we already pull `soapysdr-sys` for capability probing). The
device-capability cache is cleared first so the next `/api/devices` re-
probes fresh.

`200 { "status": "ok" }` on success.

`409 RELOAD_REFUSED_RUNNING` when the pipeline is up — a live
`SoapySDRDevice` handle would dangle past `unloadModules`. Stop the
pipeline (`POST /api/pipeline/stop`) and try again.

Service-process drivers (notably SDRplay's `sdrplay_apiService`) don't
benefit from this — the wedge is usually on the service side, not in
the driver module. `sudo systemctl restart sdrplay` stays the right
hammer there. Pure-library drivers (HackRF, RTL-SDR, Airspy, …) tear
down + reload cleanly.

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
the right surface for "swap to a different SDR" or for a raw source-LO
write that intentionally bypasses the DC-spike dodge (the "low-level
escape hatch" half of D26 below). The high-level "listen at this
frequency" call is `POST /api/tune`.

### `POST /api/tune`

Single tuning intent — "listen at `freq_hz`". Encapsulates the DC-spike
dodge for zero-IF radios (HackRF leaks the LO straight into the ADC;
the only fix is to park the source off-target and pull the listened
freq back to baseband via the channelizer's `freq_shift_hz`).

```json
{ "freq_hz": 162400000.0, "span_hz": null, "offset_ratio": 0.7 }
```

- `freq_hz` (required) — target listen frequency in Hz.
- `span_hz` (optional) — desired sample-rate / passband floor. When
  given and larger than the current source rate, the source rate is
  raised (and the driver's rate ladder clamps it).
- `offset_ratio` (default 0) — per-driver DC-dodge ratio of the
  channelizer's `output_rate_hz`. The server's snap math places the
  source LO at `target − offset_ratio × output_rate_hz` and the
  channelizer's `freq_shift_hz` at `+offset_ratio × output_rate_hz`,
  so the DC spike lands outside the channel passband. **Must exceed
  0.5** (the channelizer's complex-baseband LPF cutoff is at
  `±0.5 × output_rate_hz`) or the spike sits inside the demodulated
  channel; HackRF's preset uses `0.7`. SDRplay / RTL-SDR / Airspy
  leave it unset → `0.0` → no dodge (their drivers track DC).

Server behaviour:
1. If `span_hz` raises the rate, the source-side delta carries
   `sample_rate_hz`.
2. Find the first `Channelizer` in the composed preset and read its
   `output_rate_hz`. If absent, fall back to a direct
   `src.center_freq_hz = freq_hz` write (no dodge to apply).
3. Keepout = `0.4 × output_rate_hz`. Sticky path when the target is
   within keepout of the current source centre — keep `src_center`
   parked, only retune `chan.freq_shift_hz`. Snap path otherwise —
   re-place the source LO + reset `chan.freq_shift_hz` per the dodge
   math above.

Response is a `ReconfigureResponse` (same shape as `PATCH /api/source`),
plus the latest `source_readback`.

The UI's `tuneVfoTo` (Nixie commit, double-click on a spectrum/waterfall
pane, band-preset RX/Tune buttons, `ferrite-ctl tune`, AI tune) all
route here. Single-click + drag bypass this — they only adjust the
channelizer's `freq_shift_hz` (VFO-only), via
`POST /api/pipeline/blocks/<chan>/params`. See D26 in
[docs/09-decisions.md](09-decisions.md).

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

## Event plane (MQTT)

ferrited embeds an MQTT 3.1.1 broker ([`server/src/event_plane.rs`](../server/src/event_plane.rs),
`rumqttd`). It listens on two ports — TCP on `<http port>+1`, WebSocket
on `<http port>+2` (`--mqtt-port` / `--mqtt-ws-port`, `0` = any free
loopback port, `--no-mqtt` to disable) — and `GET /api/hello` reports both
under `mqtt`. Browsers never see the ports: `GET /mqtt` on the HTTP
port reverse-proxies the WebSocket (subprotocol `mqtt`) to the broker,
and vite proxies `/mqtt` the same way in dev. No auth, no TLS (LAN
deployment, same posture as the rest of the API).

**State is a set of kinds** in the server store (`/api/state` is the
whole thing); **every change is one record** and every record is one
message:

```
ferrite/state/<kind>          every delta of the kind (QoS 0).
                              Replace kinds (graph, blocks, status, health,
                              readback, rssi, signals, flowdiag,
                              params/<block>): the latest record, retained —
                              a new subscriber gets it at once.
ferrite/state/<kind>/<key>    keyed kinds (adsb, aprs, ais, jobs, ai_live):
                              the entity, retained; an empty retained
                              payload = expired.
ferrite/state/log/<target>    tracing lines, `::` → `/` (decoder::pocsag is
                              ferrite/state/log/decoder/pocsag) as
                              {target, level, message, at_ms}.
ferrite/state/activity        one record per mutating ferrite-ctl / agent
                              request (X-Ferrite-Command / -Note).
ferrite/ingest/<kind>         publish a raw event here and the server folds
                              it into <kind>; an `epoch` field that doesn't
                              match the current graph epoch is dropped.
```

Payloads are the store's delta JSON — the same shape the REST snapshot
is built from:

```json
{"kind":"adsb","seq":4821,"epoch":7,"op":"upsert",
 "record":{"seq":4821,"epoch":7,"at_ms":1791650000000,"key":"a1b2c3","data":{…}}}
```

Decoder records also carry `ctx: {listen_hz, lo_hz, chan_shift_hz,
preset}` — where the radio was listening when the decode was folded —
so an RF frequency needs no join on `graph`. Pager, EAS and AIS records
carry parsed fields next to the raw `text` (address/capcode + message;
SAME originator/event/locations; MMSI, position, name, callsign — `ais`
is keyed by `mmsi` and position + static reports merge).

`op` is `add` (append kinds), `upsert`, `expire` (`key` instead of
`record`) or `reset`. `seq` is the store-wide cursor; dedup **per kind**
(the server coalesces replace-kind bursts to ≤10 Hz, so a coalesced
record can arrive after a higher-seq append). `epoch` is the graph
epoch — bumped once per topology rebuild and stamped on every record and
every `/ws/preset` frame, so a consumer can tell which graph a record (or
a sample) belongs to. The `graph` record carries the composed doc, the
browser half and its hash: a browser reloads its half when the hash
changes and moves its frame gate when the epoch changes, with no REST
round trip.

The `graph` record also carries the UI's view — `flowgraph` (the preset
with live params), `profile`, `ui_sinks`, `source_caps` — and `blocks`
carries the composed block list; with `status` and `params/<block>`
that is everything a tab needs to follow a mutation made anywhere (CLI,
MCP, another tab) without a REST round trip.

**`health`** (Replace, ~1 Hz and right after every tune / source patch /
start) answers "am I getting what I asked for?": `ok` + `issues` (plain
words), `radio` (`state` idle/ok/lost/recovering, `attempts`,
`last_error`, `recoveries`, `remote`, reader `stream` counters),
`tuning` (`lo_requested_hz`, `lo_actual_hz`, `chan_shift_hz`,
`listen_hz`), `rate` (`declared_hz` by the preset, `configured_hz`,
`actual_hz`), and `warnings` — WARN/ERROR lines from decoders, the flow
diagnostics and the driver since the current graph loaded (rate
mismatches, snapped rates, stalls).

**Radio supervisor.** A `SoapySource` whose reader died, or that hasn't
delivered for 5 s, is a lost radio; while the operator still wants the
pipeline running, ferrited cycles it (stop — which releases a SoapyRemote
handle — back off 1/2/4/8/10 s, start) until samples flow, with each step
on `activity`. A start/tune/source/preset request that fails because a
remote radio host isn't answering returns `503 RADIO_UNREACHABLE` (and a
failed start keeps the run intent, so the pipeline comes up when the host
does). `POST /api/devices/reload` refuses (`409 RELOAD_UNSAFE`) while a
leaked hung handle or an abandoned probe thread could be left pointing at
unloaded driver code.

**AI session bus.** Every AI conversation — the ferrite-ai sidecar's,
and a Claude Code session mirrored by its hooks (`ferrite-ctl
chat-post`) — is the `ai` append kind, published through
`ferrite/ingest/ai`:

```
{type:"user"|"text"|"tool_use"|"tool_result"|"turn_end"|"reset",
 session, source, mode?, model?, t, text?, tool_id?, name?, input?,
 is_error?, images?, status?, error?, reason?}
```

`ai_live` (keyed by `session`) carries the reply forming in real time.
The browser's AI panel renders all sessions from these records; `/ws/chat`
is only the control channel (prompt, stop, reset). Only the newest few
`ai` records keep inline tool-result images.

**Persistence.** With `--state-dir` / `FERRITE_STATE_DIR` (run.sh:
`.ferrite-state/`), every non-live kind — decodes, `ai`, `activity`,
`jobs`, keyed tables — is saved (`store.json`, atomically, ≤5 s after a
change and on SIGINT/SIGTERM) and restored at boot with `seq` and `epoch`
continuing. Live kinds (graph, blocks, status, health, readback,
flowdiag, signals, rssi, params/*, ai_live) are rebuilt, never restored.
Decoder history is no longer wiped when a preset starts; clear a kind
with `POST /api/state/:kind/reset`.

Append kinds are stream-only; history is the snapshot: subscribe first,
`GET /api/state`, then apply anything with a higher seq. The browser
mirror ([`web/src/lib/decoders/store.svelte.ts`](../web/src/lib/decoders/store.svelte.ts))
does exactly this on every (re)connect through one shared client
([`web/src/lib/events/bus.ts`](../web/src/lib/events/bus.ts)); the log
sidebar is a subscriber of `ferrite/state/log/#` on the same connection.

By hand:

```bash
mosquitto_sub -p 12002 -t 'ferrite/state/log/decoder/#' -v     # every decoder log line
mosquitto_sub -p 12002 -t 'ferrite/state/graph' -v              # retained: what's running
mosquitto_pub -p 12002 -t ferrite/ingest/pager -m '{"msg":"hi"}'
```

## Cross-origin isolation

Every response from the static-asset layer carries

```
Cross-Origin-Opener-Policy:   same-origin
Cross-Origin-Embedder-Policy: require-corp
```

Set in `server/src/main.rs:313-314`. The dev-server equivalent is in
[`web/src/lib/vite/coop-coep.ts`](../web/src/lib/vite/coop-coep.ts).
`SharedArrayBuffer` (the audio ring) silently degrades without these.

## MCP server (`ferrite-ctl mcp`)

The control surface for AI clients — and the single operations layer the
human `ferrite-ctl` CLI drives too. The REST API documented above is
wrapped as [Model Context Protocol](https://modelcontextprotocol.io)
tools, dispatched over **stdio JSON-RPC**. Any MCP-enabled client —
Claude Desktop, Claude Code CLI, the bundled `ferrite-ai` sidecar via
the Agent SDK's `mcpServers` config — can drive a running `ferrited`
with no shell-exec and no per-tool prompt engineering.

Each verb is implemented exactly once, as a `FerriteServer::*_op` method
that hits the REST API. The MCP `#[tool]` handlers are thin wrappers
over those ops, and every human CLI subcommand calls the same `*_op`
method — so the two front-ends **cannot drift** (a CLI verb *is* its MCP
tool). Source:
[`tools/ferrite-ctl/src/mcp.rs`](../tools/ferrite-ctl/src/mcp.rs);
underlying SDK is the canonical
[`rmcp`](https://crates.io/crates/rmcp).

### Wire transport

`stdio`. Each line on stdin is a JSON-RPC 2.0 request; replies land on
stdout. Logs (including any error context) are routed to **stderr** —
a stray `println!` from a tool handler would corrupt the protocol
stream, so handlers return text content blocks instead. Set
`RUST_LOG=rmcp=debug` for per-frame protocol detail.

### Tools

Each tool corresponds to one or more REST endpoints from this document.
Argument schemas are emitted automatically from `#[derive(JsonSchema)]`
request structs; the descriptions below are what `tools/list` returns.

| Tool | Wraps | Purpose |
|---|---|---|
| `status` | `GET /api/pipeline` + `/source` + `/flowgraph` + `/ui-sinks` | Cheap "what's the world look like" snapshot (incl. a `ready` boolean + the full flowgraph). Call first. |
| `list_devices` | `GET /api/devices` | Enumerate Soapy devices + capability schemas. |
| `source_capabilities` | `GET /api/source/capabilities` | Antennas, gain ladder, rate/bw/freq ranges, and the driver `settings` map for the *bound* source. |
| `source_readback` | `GET /api/state` → `kinds.readback` | Live driver readback (actual gain/AGC/antenna/bw) — confirms a gain/AGC change landed. Folded into the store by the diag tick. |
| `flowdiag` | `GET /api/state` → `kinds.flowdiag` | Per-block throughput / process-time / ring fill — "are samples actually moving?". Folded into the store by the diag tick. |
| `list_captures` | `GET /api/captures` | The curated `samples/` fixtures (path, kind, rate, centre, modulation). |
| `replay_capture` | `PATCH /api/source` (`ModulatedFileSource`) | Replay a fixture through the full RX chain — deterministic decode/transcribe tests, no SDR. |
| `list_blocks` | `GET /api/pipeline/blocks` | Every block in the active preset with its full spec (param keys, kinds, ai_notes) + current values. |
| `list_block_types` | `GET /api/blocks` | Static schema of every registered block type, with `reconfig_scope`. |
| `reload_drivers` | `POST /api/devices/reload` | In-process Soapy module unload + reload. Pipeline must be stopped. |
| `select_device` | `PATCH /api/source` | Bind the source to a different SDR (Soapy args string). Resets params to driver defaults. |
| `list_presets` | `GET /api/presets` | Available flowgraph presets (name + label + description). |
| `preset_detail` | `GET /api/presets/{name}` | Full JSON of one preset (ai_notes, block graph, sigwiki refs, sample path). |
| `band_at` | `GET /api/band-plan/at?hz=` | US allocation band(s) containing a frequency. |
| `load_preset` | `POST /api/preset` | Swap to a different preset (preserves centre freq). |
| `tune` | `POST /api/tune` (+ `PATCH /api/source`) | Listen-frequency change with the per-driver DC-spike dodge; optionally folds in `gain_db` (+`agc=false`), `bandwidth_hz`, `sample_rate_hz`. |
| `set_block_param` | `POST /api/pipeline/blocks/{id}/params` | Patch one block's params. `src` accepts gain/agc/antenna/bandwidth_hz/dc_offset_correction/`settings`. |
| `start` | `POST /api/pipeline/start` | Start the pipeline. |
| `stop` | `POST /api/pipeline/stop` | Stop the pipeline. |
| `transcribe` | `PATCH /api/profile` | Toggle the speech-to-text tap (sherpa-onnx: in-browser WASM under `balanced`, the node sidecar under `server`). |
| `decodes` | `GET /api/state` | Structured store state: one kind (`{recent, current}`) or the whole store — decodes, `jobs`, `activity`, `ai`, `health`. |
| `await_events` | MQTT `ferrite/state/<kind>/#` | Block until the next `max` records of a kind arrive (or `timeout_s`); `since_seq` skips known records, `include_retained` returns the current value first, `filter` keeps only records whose data matches (scalar = equal, array = any-of). The subscription form of `decodes` — no polling. CLI: `ferrite-ctl await <kind> [--follow] [--where field=v1,v2]`. |
| `start_capture_iq` | `chan` tee / `src` tee | Async IQ capture — live tee of the channelized slice (default) or, with `wideband`, of the raw full-span stream off the SDR. Neither swaps presets. Returns a `job_id`. |
| `start_capture_fft` | `logmag` tee | Async FFT-byte-stream capture (live tee). Returns a `job_id`. |
| `start_capture_audio` | `WsBridgeTxF32` tee | Async 48 kHz WAV of what the operator hears (live tee of the node→browser audio bridge; `freq_hz` optional). Returns a `job_id`. |
| `chat_post` | MQTT `ferrite/ingest/ai` | Publish a headless driver's turn (user / assistant / tool) onto the AI session bus, grouped by `session`. |
| `view_snapshot` | `GET /api/ui-views/{pane}` | PNG of a live FFT/waterfall canvas. |
| `view_state` | `GET /api/view` | Browser-authored UI chrome state. |
| `set_view_state` | `POST /api/ui-view/set` | Push a chrome-state patch (Main pane, channel-detail visibility) to the browser. |

Mutating tools accept an optional `note` (the "why") that the daemon
surfaces under `ai::activity` for the UI's transcript panel.

Captures are async: `start_capture_*` returns a `job_id`; wait with
`await_events(kind="jobs/<job_id>", include_retained=true,
filter={"status":["done","failed","interrupted"]})` — the job record
carries `output_path` and the sidecar. `status` includes `health`. The
CLI's `tail <target>` follows `ferrite/state/log/<target>` live
(`decoder recent` is the one-shot log recall for humans).

### Claude Desktop / Claude Code config

Drop this into `~/.config/Claude/claude_desktop_config.json` (Linux) or
the Claude Code MCP config (`~/.config/claude/mcp.json` or per-project
`.mcp.json`) and the tools surface automatically:

```jsonc
{
  "mcpServers": {
    "ferrite": {
      "command": "/usr/bin/ferrite-ctl",
      "args": ["mcp"],
      "env": { "FERRITE_CTL_CONNECT": "http://127.0.0.1:10001" }
    }
  }
}
```

Override the target daemon with `--connect <BASE>` (CLI flag, surfaces
in MCP mode too) when driving a remote rig over an SSH tunnel.

### Smoke test by hand

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"status","arguments":{}}}' \
  | ferrite-ctl mcp
```

Each `tools/call` reply's `result.content[0].text` is the wrapped REST
response body as a JSON-encoded string.

## Security

LAN-trust, no auth on any endpoint. Anything beyond the LAN is the user's
tunnel problem. See [09-decisions.md D07](09-decisions.md).
