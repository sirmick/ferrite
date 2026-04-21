# 04 — Flowgraphs

## What is a flowgraph

A **flowgraph** is a JSON document describing a graph of block instances
and their wiring. The `ferrite-runtime` reads this document (`FlowgraphDoc`),
instantiates blocks from the registry, validates ports and params, wires
them, and runs them. Today the runtime lives server-side in `ferrited`;
post-M5 the same runtime (compiled to WASM) also drives the browser-side
half of cross-env presets.

Flowgraphs are the unit of "here is a new decoder" — adding ADS-B will be
a `flowgraphs/adsb.json` file plus whatever blocks it needs. No UI code,
no server code.

## Top-level schema

```json
{
  "$schema": "https://ferrite.example/flowgraph-v1.json",
  "name": "wbfm",
  "label": "FM broadcast receiver",
  "description": "Wideband FM demod to audio",
  "environments": ["node", "browser"],
  "blocks": {
    "<instance_id>": {
      "type": "<BlockTypeName>",
      "placement": "node" | "browser",
      "params": { ... }
    }
  },
  "wires": [
    ["<instance_id>.<out_port>", "<instance_id>.<in_port>"]
  ]
}
```

### Fields

| field          | type              | purpose                                           |
|----------------|-------------------|---------------------------------------------------|
| `name`         | string (slug)     | machine identifier; filename matches              |
| `label`        | string            | human-readable name for UI                        |
| `description`  | string            | one-sentence summary                              |
| `environments` | string[]          | which runtimes this graph lives on                |
| `blocks`       | object            | instance_id → block declaration                   |
| `wires`        | array of [a, b]   | connections, `a` is output, `b` is input          |

`environments` values: `"browser"`, `"node"`. A single-env graph declares
one side; a cross-env graph declares both. Block types with
`Placement::NativeOnly` (e.g. `SoapySource`) or `Placement::WasmOnly` (e.g.
`AudioSink`) pin themselves to the matching environment; blocks with
`Placement::Either` (most DSP — `Decimator`, `FmDemod`, `Channelizer`)
accept a per-instance `placement` field in the doc that pins them to one
side. On load, `split_for_environment` carves the doc into an env-local
subgraph and auto-inserts a `WsBridgeTx`/`WsBridgeRx` pair on every wire
that crossed the boundary. Authors do **not** hand-wire those bridges.

### Instance IDs

Instance IDs are local to a flowgraph. They must be unique within the
flowgraph, URL-safe, and human-meaningful (you will read them in error
messages). Convention: lower_snake_case nouns.

## Sources and sinks

Sources and sinks are **blocks** from the runtime's perspective (they implement
the same trait). Placement is declared by the block's `BlockSpec::placement`:

### The `Source` placeholder

Every cross-env preset names its source as `"type": "Source"`. This is
a **placeholder** — at load time, `compose_source` reads the
`SourceConfig` subresource from `AppState` (updated via `PATCH
/api/source`) and substitutes the real source block (`SoapySource`,
`SineSource`, `FileIqSource`) into the graph before validation. That
keeps preset files stable across source swaps — changing device is a
`PATCH /api/source` call, not a preset rewrite. The placeholder carries
tuning hints (`center_freq_hz`, `sample_rate_hz`, `bandwidth_hz`) which
the real source inherits unless the `SourceConfig` overrides them.

### Node-side (`Placement::NativeOnly`)

| type              | direction | notes                                                   |
|-------------------|-----------|---------------------------------------------------------|
| `SoapySource`     | source    | RTL-SDR / SDRPlay / any Soapy device                    |
| `FileIqSource`    | source    | reads IQ from a local file                              |

### Browser-side (`Placement::WasmOnly`)

| type              | direction | notes                                                   |
|-------------------|-----------|---------------------------------------------------------|
| `AudioSink`       | sink      | feeds the AudioWorklet ring buffer (SAB)                |
| `WsBridgeRx`      | source    | subscribes to a `stream_id` on `/ws/preset` (also auto-inserted by `env_split`; see below) |

### Either (DSP; pin with per-instance `placement`)

DSP blocks (`Channelizer`, `Decimator`, `FmDemod`, `AmDemod`, `FFT`,
`LogMagU8`, `TeeIqF32`, `SineSource`) default to `Placement::Either`.
In a cross-env doc they carry a `placement: "node" | "browser"` field
that chooses which side they land on. Leaving `placement` unset lets
`env_split` infer the side from the neighbourhood (a block fed by a
`node`-pinned producer lands on the node half).

### Cross-env bridges are automatic

`WsBridgeTx` and `WsBridgeRx` exist in the registry but are **not**
hand-authored. The loader inspects each wire; wires that cross the
node/browser boundary are rewritten to insert a `WsBridgeTx` on the node
side and a `WsBridgeRx` on the browser side, sharing a freshly allocated
`stream_id` starting at `CROSS_ENV_STREAM_BASE` (1000). Allocation is
deterministic: wires that cross the boundary are enumerated in
declaration order. See [02-protocol.md](02-protocol.md#stream-ids) for
the full id allocation story.

### `ui:<name>` sinks

A wire whose RHS is the sentinel `"ui:<name>"` (rather than a real
block port) declares a **UI-bound** output. `env_split` rewrites it
into a `WsBridgeTx` on the server side only — the browser discovers
the allocated `stream_id` and payload type via
`GET /api/ui-sinks?name=<name>` rather than by declaring a matching
`WsBridgeRx` in the preset. This is how the waterfall's FFT stream
flows from a server-side `FFT → LogMagU8` chain without the browser
having to know which preset block emitted it.

## Wires

A wire connects one output port to one input port. Form: `"block.port"` on
both sides.

```json
"wires": [
  ["src.out",   "demod.in"],
  ["demod.out", "audio.in"]
]
```

Rules:

- **One input, one output, one wire.** A port may not connect to multiple
  counterparts. Broadcasting is a job for a `Tee` block — explicit, not
  implicit.
- **Port types must match.** `iq_f32 → iq_f32`, `real_f32 → real_f32`, etc.
  The validator rejects type mismatches before `init()` runs.
- **Sample rate compatibility** is checked at `init()` time after rate
  negotiation. A sink that demands 48 kS/s fed by a decimator producing
  48 kS/s is fine; feeding it with 192 kS/s is an error the runtime surfaces.

## Validation

A flowgraph is validated in this order. The first failure aborts with a
structured error.

1. **Shape**: JSON conforms to the top-level schema.
2. **Env match**: all block types in `blocks` are registered and available
   in the declared environment.
3. **Params**: each block's params pass the block's param schema.
4. **Wire endpoints**: `a` is a real output port, `b` is a real input port.
5. **Port-type match** on each wire.
6. **Fan-in/fan-out**: no port has more than one wire.
7. **DAG check**: no cycles. Feedback loops are not supported in v0.1.
8. **Connectivity**: every declared block has at least one wire (isolated
   nodes are a warning, not an error — tolerated for in-progress work).
9. **Rate negotiation**: `init()` runs, blocks declare their rate
   requirements; the runtime resolves or rejects.

Errors look like:

```json
{
  "phase": "wire_type_match",
  "wire": ["demod.out", "audio.in"],
  "error": "Port 'demod.out' is real_f32; 'audio.in' expects real_i16"
}
```

The browser UI renders these inline next to the offending flowgraph file
in the Signal Catalog panel when a preset fails to instantiate. When
`ferrited` is launched headless (`--flowgraph <path>`) and the preset
fails to instantiate, it logs the error and exits non-zero so
`systemctl` restart policy behaves sensibly.

## Runtime parameter updates

Blocks declare which params are `runtimeUpdatable`. The runtime exposes an
API for the environment to push updates:

```ts
runtime.update("demod", { bandwidth: 15000 });
```

- The UI calls this when the user drags a filter edge or adjusts a
  slider (routed to the server as `PATCH /api/flowgraph` against the
  single block; the runtime applies it without stopping the graph if
  the param's `reconfig_scope` is `SelfBlock`).
- Headless `ferrited` with file-watch enabled applies diffs the same
  way when the preset file changes on disk.

Non-updatable param changes require restarting the flowgraph.

## Example: WBFM (cross-env)

The shipped `flowgraphs/wbfm.json`: the node half reads 2.4 MS/s IQ
from the `Source` placeholder (resolved to whatever device
`SourceConfig` currently names), fans the raw source through a
`TeeIqF32` so **one** leg drives a server-side `FFT → LogMagU8` chain
into the `ui:fft` sentinel (feeding the waterfall at full 2.4 MHz
span), and **the other** leg runs a `Channelizer` to 240 kS/s before
crossing into the browser for `Decimator → FmDemod → AudioSink`.

```json
{
  "name": "wbfm",
  "environments": ["node", "browser"],
  "blocks": {
    "src":    { "type": "Source", "placement": "node",
                "params": { "center_freq_hz": 100100000,
                            "sample_rate_hz": 2400000,
                            "bandwidth_hz": 2000000 } },
    "tee":    { "type": "TeeIqF32", "placement": "node" },
    "fft":    { "type": "FFT", "placement": "node",
                "params": { "size": 16384, "window": "hann" } },
    "logmag": { "type": "LogMagU8", "placement": "node",
                "params": { "size": 16384, "floor_dbfs": -100.0,
                            "ceil_dbfs": 0.0, "alpha": 0.3 } },
    "chan":   { "type": "Channelizer", "placement": "node",
                "params": { "input_rate_hz": 2400000, "freq_shift_hz": 0.0,
                            "factor": 10, "num_taps": 81,
                            "cutoff_normalized": 0.03125 } },
    "decim":  { "type": "Decimator", "placement": "browser",
                "params": { "factor": 5, "num_taps": 41,
                            "cutoff_normalized": 0.08 } },
    "demod":  { "type": "FmDemod", "placement": "browser",
                "params": { "sample_rate_hz": 48000,
                            "max_deviation_hz": 75000 } },
    "audio":  { "type": "AudioSink",
                "params": { "buffer_samples": 8192 } }
  },
  "wires": [
    ["src.out",    "tee.in"],
    ["tee.out0",   "chan.in"],
    ["tee.out1",   "fft.in"],
    ["fft.out",    "logmag.in"],
    ["logmag.out", "ui:fft"],
    ["chan.out",   "decim.in"],
    ["decim.out",  "demod.in"],
    ["demod.out",  "audio.in"]
  ]
}
```

`audio` omits `placement` (its `BlockSpec` is `WasmOnly`); everything
else pins explicitly. The `chan.out → decim.in` wire crosses the env
boundary — `env_split` inserts a `WsBridgeTx`/`WsBridgeRx` pair on a
freshly allocated `stream_id` from the `CROSS_ENV_STREAM_BASE` (1000)
range. The `logmag.out → ui:fft` wire is rewritten to a
`WsBridgeTxFftU8` on the server side only; the browser learns the
allocated id via `GET /api/ui-sinks`.

## Shipped presets

| file                          | purpose                                                                 |
|-------------------------------|-------------------------------------------------------------------------|
| `flowgraphs/wbfm.json`        | WBFM listening, Phase D smoke target                                    |
| `flowgraphs/wbam.json`        | AM listening (AM variant of wbfm)                                       |
| `flowgraphs/dtmf-e2e.json`    | `DtmfAudioSource → AmModulator → AmDemod → DtmfDecoder → EventsSink` end-to-end smoke |
| `flowgraphs/capture_fm.json`  | all-native capture to `/tmp/ferrite-fm-*.wav` + sidecar (no browser)    |

NBFM, SSB/CW, APRS, ADS-B, FT8, M17 presets land alongside their
respective blocks — see `docs/decoder-roadmap/` for the sequence.

## Schema authoring

Block param schemas come from the `#[ferrite_block]` attribute-derived
`BlockSpec` in the `blocks/` crate — exposed to clients via
`GET /api/blocks`. There is no separate on-disk schema file; the
source tree is the schema. Adding a new block gives the dialog UI
automatic rendering of its params through the same `optionsModel`
pipeline the source and flowgraph dialogs use.

## Hot reload (dev only)

`ferrited --flowgraph <path>` watches the file (opt-in) and applies the
new preset through `PATCH /api/flowgraph`'s reconfigure plan — hot
where the diff allows, cold-restart otherwise. The browser side picks
up the new stream_ids via `GET /api/ui-sinks` after any cold restart
without reloading the page.
