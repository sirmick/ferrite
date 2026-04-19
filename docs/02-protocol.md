# 02 — Control API and WS frame format

Two surfaces:

- **REST (`/api/…`)** — JSON over HTTP. State changes, enumeration, identify.
  Everything mutating goes here.
- **WebSocket (`/ws/…`)** — binary frames, multiplexed. Streaming data and
  server-pushed events.

A client (browser or `ferrite-headless`) always does REST to set up state,
then opens a WS to receive streams.

## Session lifecycle

```
client ──► GET  /api/devices                 ─► [ { device... } ]
client ──► POST /api/device/open  { args,     ─► { session_id, ws_url, streams }
                                    stream }
client ──► WebSocket connect ws_url           ─► binary frames start flowing
client ──► POST /api/device/{id}/vfo { ... }  ─► { vfo_id, stream_id }
client ──► PATCH /api/device/{id}/vfo/{vfo_id} { ... }   (retune mid-stream)
client ──► PATCH /api/device/{id}/settings { ... }       (AGC, gain, etc.)
client ──► POST /api/device/{id}/close        ─► {}
```

Single-listener posture: there is at most one active session on `ferrited`.
A second `POST /api/device/open` while a session is active closes the first
session (last-connect wins) and returns a new `session_id`.

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

### `POST /api/device/open`

Take ownership of a device.

Request:

```json
{
  "args": { "driver": "rtlsdr", "serial": "0000001" },
  "stream": {
    "sample_rate": 2048000,
    "center_freq": 100000000,
    "antenna": "RX",
    "agc": false,
    "gain": { "TUNER": 30.0 },
    "settings": { "biastee": false, "offset_tune": false }
  },
  "fft": {
    "size": 8192,
    "rate_hz": 30,
    "window": "hann"
  }
}
```

Response:

```json
{
  "session_id": "8a3d4b2f-...",
  "ws_url": "/ws/8a3d4b2f-...",
  "streams": {
    "fft": { "stream_id": 0, "payload_type": "fft_u8", "size": 8192 }
  }
}
```

Failure (device busy, driver error, invalid settings): standard error response
(see below). If another session was active, it is closed atomically and its WS
receives a `session_closed` event immediately before disconnect.

### `GET /api/device/{session_id}/state`

Current settings + FFT config + list of open VFOs.

Response:

```json
{
  "session_id": "8a3d4b2f-...",
  "device": { "id": "rtlsdr://0000001" },
  "stream": {
    "sample_rate": 2048000,
    "center_freq": 100000000,
    "agc": false,
    "gain": { "TUNER": 30.0 },
    "antenna": "RX",
    "settings": { "biastee": false, "offset_tune": false, "direct_samp": "off" }
  },
  "fft": { "size": 8192, "rate_hz": 30, "window": "hann" },
  "vfos": [
    { "vfo_id": "...", "stream_id": 1, "offset_hz": 100000, "rate": 48000,
      "payload_type": "iq_f32" }
  ]
}
```

### `PATCH /api/device/{session_id}/settings`

Mutate device settings while streaming. Body is a partial — only include keys
that change.

Request:

```json
{ "center_freq": 101100000, "gain": { "TUNER": 34.0 }, "agc": true }
```

Response: the updated `state` object.

Failure if any requested change targets a setting with
`mutableWhileStreaming: false` — the response identifies the offending keys
and no changes are applied. Clients should detect this and offer the user an
"Apply & restart stream" affordance that first closes the session and reopens
with the new settings.

### `POST /api/device/{session_id}/vfo`

Add a VFO (a channelized narrowband slice).

Request:

```json
{
  "offset_hz": 100000,
  "rate": 48000,
  "filter_bw": 200000,
  "payload_type": "iq_f32"
}
```

`offset_hz` is relative to the current device `center_freq`; the VFO auto-
updates when the device retunes, so relative-frequency semantics survive
retuning naturally. Absolute mode available by passing `center_freq_abs`
instead of `offset_hz`.

Response:

```json
{ "vfo_id": "...", "stream_id": 2, "payload_type": "iq_f32", "rate": 48000 }
```

### `PATCH /api/device/{session_id}/vfo/{vfo_id}`

Move or reconfigure a VFO mid-stream (drag its cursor on the waterfall).

Request:

```json
{ "offset_hz": 225000, "filter_bw": 12000 }
```

Response: the updated VFO descriptor.

### `DELETE /api/device/{session_id}/vfo/{vfo_id}`

Tear down a VFO. The corresponding `stream_id` is released; any further frames
bearing it would be a bug.

### `POST /api/device/{session_id}/close`

Release the device. Disconnects the WS. Returns `{}` on success.

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

Codes (non-exhaustive): `DEVICE_NOT_FOUND`, `DEVICE_BUSY`, `DEVICE_FAILED`,
`INVALID_SETTINGS`, `SETTING_IMMUTABLE_WHILE_STREAMING`, `SESSION_NOT_FOUND`,
`VFO_NOT_FOUND`, `STREAM_LIMIT_REACHED`, `INTERNAL`.

## WebSocket frame format

One WS connection per session. All frames are **binary**. A common fixed
header precedes every payload:

```
Offset  Size   Field          Notes
------  -----  -------------  -------------------------------------------
  0     1      version        0x01 for this spec version
  1     1      payload_type   enum (see below)
  2     2      stream_id      big-endian u16 (0 = server control stream)
  4     4      seq            big-endian u32, per-stream, wraps
  8     8      timestamp_ns   big-endian u64, device clock
 16     ...    payload        payload-type-specific
```

Total fixed header: 16 bytes.

### `payload_type` values

| value | name            | payload                                             |
|-------|-----------------|-----------------------------------------------------|
| 0x01  | `fft_u8`        | `size` bytes of unsigned magnitude, 0..255           |
| 0x02  | `fft_f16`       | `2 * size` bytes of half-float magnitudes (dBFS)     |
| 0x10  | `iq_s16`        | interleaved I,Q signed 16-bit samples                |
| 0x11  | `iq_f32`        | interleaved I,Q 32-bit floats (native-endian f32)    |
| 0x20  | `audio_f32`     | mono 32-bit float PCM (server-side demod path; rare) |
| 0x80  | `json_event`    | UTF-8 JSON — see event schema below                  |
| 0x81  | `json_control`  | UTF-8 JSON — reserved for future client→server       |
| 0xFF  | `error`         | UTF-8 JSON error object, same shape as REST errors   |

`iq_f32` notes: payload byte length is `8 * num_samples` (4B I + 4B Q per
sample). Endian is native to the server; both server and client architectures
we target are little-endian, and the wire protocol asserts LE in the FFT
frames' header timestamps but leaves IQ payload as-is to avoid per-sample
byte-swap overhead. Non-LE clients must swap.

### Stream IDs

- `stream_id = 0` is the **control stream**: server pushes `json_event`
  frames here (state changes, warnings, errors).
- `stream_id = 1` (convention) is the **waterfall FFT** stream (allocated
  implicitly at `POST /api/device/open`).
- `stream_id` 2..999 are assigned by the server when VFOs are created via
  the REST API. Clients receive the mapping via the `POST /api/device/.../vfo`
  response.
- `stream_id >= 1000` (`CROSS_ENV_STREAM_BASE`) are allocated automatically
  by the runtime's `env_split` pass when a flowgraph crosses the node/browser
  boundary. Each cross-env wire gets its own id, assigned in wire-declaration
  order, carried on an auto-inserted `WsBridgeTx`/`WsBridgeRx` pair. The
  allocation is deterministic for a given doc, so the browser side and the
  node side resolve to the same numbers without any negotiation round-trip.

### JSON event schema (stream 0)

All events are `{ "type": "...", ... }`. Known types:

| type              | fields                                                   |
|-------------------|----------------------------------------------------------|
| `state_changed`   | `{ stream: {...}, fft: {...}, vfos: [...] }` — full or partial echo |
| `vfo_added`       | `{ vfo_id, stream_id, offset_hz, rate, payload_type }`   |
| `vfo_removed`     | `{ vfo_id, stream_id }`                                  |
| `warning`         | `{ code, message }` — e.g. buffer underrun               |
| `session_closed`  | `{ reason }` — server is about to disconnect              |

## Subscription semantics

The waterfall FFT stream and all VFO streams are **push** — the server sends
frames as they are produced, a client cannot throttle individual streams.
Clients that cannot keep up will see the server's outbound buffer saturate;
backpressure is handled at the WS layer (TCP's flow control). Any dropped
frames surface as a gap in `seq` numbers — clients should detect this and
either reset their local state or ignore the gap depending on what the stream
represents.

For power saving or tab-visibility throttling, clients should simply close
streams they are not rendering (`DELETE` the VFO, unsubscribe from FFT) rather
than try to rate-limit. The server side is where the FFT cost lives.

## Protocol versioning

The `version` byte in the frame header and a `X-Ferrite-Protocol-Version`
header on REST responses identify the protocol version. Breaking changes
bump this byte. Minor additions (new `payload_type` values, new event types)
do not; clients that see an unknown type should log and ignore.

## Security

See `09-decisions.md`: v0.1 assumes LAN-trust, no auth on any endpoint.
Any exposure to untrusted networks must happen behind a user-operated tunnel
(Tailscale, WireGuard, reverse proxy with its own auth).
