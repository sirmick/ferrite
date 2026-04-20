# 02 — Control API and WS frame format

Two surfaces:

- **REST (`/api/…`)** — JSON over HTTP. State changes, enumeration, identify.
  Everything mutating goes here.
- **WebSocket (`/ws/…`)** — binary frames, multiplexed. Streaming data and
  server-pushed events.

A client (browser, or a second `ferrited --flowgraph` consumer) always
does REST to set up state, then opens a WS to receive streams.

## Pipeline lifecycle

`ferrited` holds exactly **one preset-backed pipeline**. The preset is
authored as a `FlowgraphDoc` JSON document (see `04-flowgraphs.md`); its
source block is the one subresource that accepts independent patching so
device/tuning changes don't re-serialise the whole doc. Post-M5 the
surface is preset-first — there is no session-id, no device-open handle,
no per-VFO REST; a VFO is just a channelizer in the preset.

```
client ──► GET  /api/devices             ─► [ { device... } ]
client ──► GET  /api/flowgraph           ─► FlowgraphDoc
client ──► GET  /api/source              ─► SourceConfig
client ──► GET  /api/ui-sinks            ─► [ { name, stream_id, payload_type } ]
client ──► WebSocket connect /ws/preset  ─► postcard-framed binary stream
client ──► PATCH /api/source  { ... }    ─► { applied: bool }        (hot reconfigure)
client ──► PATCH /api/flowgraph { ... }  ─► { plan: ReconfigurePlan } (hot or cold)
client ──► POST /api/pipeline/start      ─► { status: "running" }
client ──► POST /api/pipeline/stop       ─► { status: "stopped" }
```

Single-listener posture: there is at most one active pipeline, and it is
whatever the server currently holds. Clients subscribe to individual
streams on `/ws/preset` by their `stream_id` (discovered via
`GET /api/ui-sinks` or allocated deterministically by `env_split` — see
"Stream IDs" below).

## REST endpoints

All responses are JSON. All mutating endpoints accept JSON request bodies.
Errors follow the convention below.

### `GET /api/devices`

Enumerate attached Soapy devices with their capability schemas.

Response:

```json
{
  "devices": [
    {
      "id": "rtlsdr://0000001",
      "driver": "rtlsdr",
      "label": "RTL-SDR (serial 0000001)",
      "args": { "driver": "rtlsdr", "serial": "0000001" },
      "caps": {
        "sample_rates": {
          "kind": "discrete",
          "values": [250000, 1024000, 1536000, 1792000, 2048000,
                     2160000, 2560000, 2880000, 3200000],
          "default": 2048000,
          "mutableWhileStreaming": false
        },
        "frequency": {
          "kind": "range",
          "min": 24000000,
          "max": 1766000000,
          "default": 100000000,
          "mutableWhileStreaming": true
        },
        "gain_elements": [
          {
            "name": "TUNER",
            "kind": "range",
            "min": 0.0,
            "max": 49.6,
            "step": 0.1,
            "default": 30.0,
            "mutableWhileStreaming": true
          }
        ],
        "agc": {
          "kind": "bool",
          "default": false,
          "mutableWhileStreaming": true
        },
        "antennas": {
          "kind": "enum",
          "values": ["RX"],
          "default": "RX",
          "mutableWhileStreaming": false
        },
        "settings": [
          {
            "key": "biastee",
            "label": "Bias-T",
            "kind": "bool",
            "default": false,
            "mutableWhileStreaming": true
          },
          {
            "key": "offset_tune",
            "label": "Offset tuning",
            "kind": "bool",
            "default": false,
            "mutableWhileStreaming": false
          },
          {
            "key": "direct_samp",
            "label": "Direct sampling",
            "kind": "enum",
            "values": ["off", "i-adc", "q-adc"],
            "default": "off",
            "mutableWhileStreaming": false
          }
        ]
      }
    }
  ]
}
```

The `caps` object is the **single source of truth** for what knobs the device
exposes. The frontend renders the options dialog from this schema; adding a
new Soapy driver therefore adds new UI automatically as long as the driver
populates `getSettingInfo` properly.

### `GET /api/flowgraph`

Return the preset `FlowgraphDoc` currently bound to the server. Exact
round-trip of the JSON the user authored (plus any patches since).

### `PATCH /api/flowgraph`

Replace the preset with a new one, or patch a subset of its blocks/wires.
The server computes a minimal reconfigure plan (M3) and applies it —
hot where the diff supports it, cold-restart otherwise. Rollback on
apply failure preserves the previous preset and returns a structured
error.

Response:

```json
{
  "applied": true,
  "plan": {
    "restart": false,
    "reconfigured_blocks": ["demod"],
    "stream_ids_preserved": [1000, 1001]
  }
}
```

### `GET /api/source`

The `SourceConfig` subresource — `{ type_name, params }`. This is just
the `src` entry of the preset, exposed separately so a tuning change
doesn't require PATCHing the whole flowgraph.

### `PATCH /api/source`

Swap source type or tweak source params. Hot-applies while running
(restarts only the source subgraph). If the pipeline is stopped, the
patch is stored but not realised until the next `start`; the response's
`applied` flag distinguishes.

### `GET /api/pipeline`

```json
{ "status": "running" | "stopped" }
```

### `POST /api/pipeline/start`, `POST /api/pipeline/stop`

Idempotent lifecycle toggles. Returns the new status.

### `GET /api/ui-sinks`

Enumerate every `ui:<name>` sentinel sink in the current preset along
with the `stream_id` the runtime allocated for it. This is how the
browser finds the FFT stream without hard-coding an id.

Response:

```json
{
  "sinks": [
    { "name": "fft", "stream_id": 1001, "payload_type": "fft_u8" }
  ]
}
```

### `GET /api/blocks`

Dump the block registry's param schemas — what the dialog UI renders
from. One entry per registered block type, each with its param list,
port shapes, and `placement`.

### `GET /api/devices`

Enumerate attached Soapy devices with their capability schemas — same
shape as before. The source dialog composes this with whatever preset
hints are declared in the `Source` block.

### `POST /api/identify` (Phase F)

Body:

```json
{
  "image_png_base64": "...",
  "center_freq": 144800000,
  "span_hz": 12500,
  "resolution_bw_hz": 24,
  "timestamp_ns": 1713100000000000000,
  "iq_clip_base64": null
}
```

Response:

```json
{
  "best_guess": { "name": "APRS", "confidence": 0.84 },
  "candidates": [
    { "name": "APRS", "url": "https://www.sigidwiki.com/wiki/APRS",
      "summary": "1200 baud Bell 202 AFSK over narrow FM ..." },
    ...
  ]
}
```

## Error model (REST)

Failures return a non-2xx HTTP status plus:

```json
{
  "error": {
    "code": "DEVICE_BUSY",
    "message": "Device is already open by another session.",
    "details": { "other_session_id": "..." }
  }
}
```

Codes (non-exhaustive): `DEVICE_NOT_FOUND`, `DEVICE_FAILED`,
`INVALID_PRESET`, `RECONFIGURE_FAILED`, `PIPELINE_NOT_RUNNING`, `INTERNAL`.

## WebSocket frame format

All `/ws/preset` traffic is **binary**. Every frame is a `postcard`-
serialised value of the `Frame` enum defined in `blocks/src/frame.rs`:

```rust
pub enum Frame {
    IqF32     { stream_id: u16, seq: u32, timestamp_ns: u64, payload: Vec<u8> },
    FftU8     { stream_id: u16, seq: u32, timestamp_ns: u64, payload: Vec<u8> },
    JsonEvent { stream_id: u16, seq: u32, timestamp_ns: u64, payload: Vec<u8> },
}
```

The variant tag **is** the protocol discriminator — a schema change
(adding/removing/reshaping a variant) is a version bump. There is no
separate fixed-length header byte. The browser decoder is a WASM export
from the same crate (`decodeFrame()` in `ferrite_blocks`), so the
server and browser cannot disagree on the schema without a build break.

### Variants

| variant     | `payload` contents                                        |
|-------------|-----------------------------------------------------------|
| `IqF32`     | interleaved I,Q little-endian `f32`, 8 bytes/sample       |
| `FftU8`     | `size` bytes of log-magnitude bins, 0..255 (floor..ceil)  |
| `JsonEvent` | UTF-8 JSON — see event schema below                       |

The block-side producer builds a `Frame` with `seq = 0` and
`timestamp_ns = 0`; the `BridgeSink` trait overrides both from its own
per-stream counters before serialising, so no block needs to thread a
mutable counter through `process()`.

The `JsonEvent` variant covers what the old spec called `json_event`,
`json_control`, and `error` — all three are structurally JSON and there
was no value in three tags. The JSON body's `"type"` field does the
dispatch.

### Stream IDs

- `stream_id = 0` — the **control stream** carrying `JsonEvent` frames
  (lifecycle, warnings, errors). Clients that care subscribe to it; the
  rest ignore it.
- `stream_id = 1` — conventional "waterfall FFT" id. The current preset-
  first server allocates FFT taps via `ui:<name>` sinks (see below), so
  this id is mostly a legacy anchor in the frame module.
- `stream_id >= 1000` (`CROSS_ENV_STREAM_BASE`) — allocated
  deterministically by the runtime's `env_split` pass. Two allocation
  sources share this range:
  - Wires that cross the node/browser boundary — each such wire gets a
    `stream_id`, carried on an auto-inserted `WsBridgeTx`/`WsBridgeRx`
    pair.
  - `ui:<name>` sentinel sinks — any wire whose RHS is `ui:<name>` is
    treated as a server-to-UI crossing. `env_split` rewrites it to a
    `WsBridgeTx` on the server side and emits nothing on the browser
    side — the browser discovers the allocated id via
    `GET /api/ui-sinks` rather than through the preset.

Both allocations are deterministic for a given preset, so server and
browser resolve to the same numbers without negotiation.

### JSON event schema (stream 0)

All events are `{ "type": "...", ... }`. Known types:

| type                     | fields                                               |
|--------------------------|------------------------------------------------------|
| `pipeline_started`       | `{ preset_name }`                                    |
| `pipeline_stopped`       | `{ reason }`                                         |
| `pipeline_reconfigured`  | `{ plan }` — same shape as the PATCH response body   |
| `warning`                | `{ code, message }` — e.g. buffer underrun           |

## Subscription semantics

All `/ws/preset` streams are **push** — the server emits frames as they
are produced, and a client cannot throttle individual streams. Clients
that can't keep up will see the outbound buffer saturate; backpressure
is handled at the WS layer (TCP's flow control). Dropped frames surface
as gaps in a stream's `seq` counter (per-stream, monotonic, wraps at
`u32::MAX`) — clients should detect this and either reset their local
state or ignore the gap depending on what the stream carries.

For power saving or tab-visibility throttling, clients should just
unsubscribe from streams they aren't rendering. The server side is
where the FFT cost lives.

## Protocol versioning

The postcard `Frame` enum **is** the wire schema — a version bump is a
variant added, removed, or reshaped. The browser decoder is built from
the same crate as the server encoder (`ferrite_blocks`), so any
mismatch between what the server sends and what the browser expects is
a build break, not a runtime ambiguity. There is no separate version
byte; the variant tag covers forward-compat within a tag, and anything
bigger is a breaking change to the schema itself.

## Security

See `09-decisions.md`: v0.1 assumes LAN-trust, no auth on any endpoint.
Any exposure to untrusted networks must happen behind a user-operated tunnel
(Tailscale, WireGuard, reverse proxy with its own auth).
