# 04 — Flowgraphs

## What is a flowgraph

A **flowgraph** is a JSON document describing a graph of block instances and
their wiring. The flowgraph runtime (browser Worker or Node sidecar) reads
this document, instantiates blocks from the registry, validates ports and
params, wires them, and runs them.

Flowgraphs are the unit of "here is a new decoder" — adding ADS-B is a
`flowgraphs/adsb.json` file plus whatever blocks it needs. No UI code, no
server code.

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

### Node-side (`Placement::NativeOnly`)

| type              | direction | notes                                                   |
|-------------------|-----------|---------------------------------------------------------|
| `SoapySource`     | source    | RTL-SDR / SDRPlay / any Soapy device                    |
| `FileIqSource`    | source    | reads IQ from a local file                              |

### Browser-side (`Placement::WasmOnly`)

| type              | direction | notes                                                   |
|-------------------|-----------|---------------------------------------------------------|
| `AudioSink`       | sink      | feeds the AudioWorklet ring buffer (SAB)                |

### Either (DSP; pin with per-instance `placement`)

DSP blocks (`Channelizer`, `Decimator`, `FmDemod`, `FFT`, `LogMagU8`,
`SineSource`) default to `Placement::Either`. In a cross-env doc they
carry a `placement: "node" | "browser"` field that chooses which side they
land on.

### Cross-env bridges are automatic

`WsBridgeTx` and `WsBridgeRx` exist in the registry but are **not**
hand-authored. The loader inspects each wire; wires that cross the
node/browser boundary are rewritten to insert a `WsBridgeTx` on the node
side and a `WsBridgeRx` on the browser side, sharing a freshly allocated
`stream_id` starting at `CROSS_ENV_STREAM_BASE` (1000). See
[02-protocol.md](02-protocol.md#stream-ids) for the id allocation story.

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

The browser UI renders these inline next to the offending flowgraph file in
the Signal Catalog panel when a preset fails to instantiate. The Node sidecar
logs them and exits non-zero (so `systemctl` restart policy behaves
sensibly).

## Runtime parameter updates

Blocks declare which params are `runtimeUpdatable`. The runtime exposes an
API for the environment to push updates:

```ts
runtime.update("demod", { bandwidth: 15000 });
```

- In the browser, the UI calls this when the user drags a filter edge or
  adjusts a slider.
- In Node, a sidecar configuration reloader can call it when the flowgraph
  file changes (optional feature, off by default).

Non-updatable param changes require restarting the flowgraph.

## Example: WBFM (cross-env)

The shipped `flowgraphs/wbfm.json`: node half reads 2.4 MS/s IQ off the
RTL-SDR and channelizes to 240 kS/s; the `chan.out → decim.in` wire crosses
the boundary and `env_split` inserts a bridge pair carrying stream_id 1000.
The browser half decimates 5× to 48 kHz, demodulates, and feeds the audio
ring.

```json
{
  "name": "wbfm",
  "environments": ["node", "browser"],
  "blocks": {
    "src":   { "type": "SoapySource",
               "params": { "args": "driver=rtlsdr",
                           "sample_rate_hz": 2400000,
                           "center_freq_hz": 100100000,
                           "bandwidth_hz": 2000000 } },
    "chan":  { "type": "Channelizer", "placement": "node",
               "params": { "input_rate_hz": 2400000, "factor": 10,
                           "num_taps": 81, "cutoff_normalized": 0.03125 } },
    "decim": { "type": "Decimator",   "placement": "browser",
               "params": { "factor": 5, "num_taps": 41,
                           "cutoff_normalized": 0.08 } },
    "demod": { "type": "FmDemod",     "placement": "browser",
               "params": { "sample_rate_hz": 48000,
                           "max_deviation_hz": 75000 } },
    "audio": { "type": "AudioSink",
               "params": { "buffer_samples": 8192 } }
  },
  "wires": [
    ["src.out",   "chan.in"],
    ["chan.out",  "decim.in"],
    ["decim.out", "demod.in"],
    ["demod.out", "audio.in"]
  ]
}
```

`src` and `audio` omit `placement` because their `BlockSpec` pins them
(NativeOnly / WasmOnly). `chan`, `decim`, `demod` are `Placement::Either`
and must declare a side.

### Deployment overrides

The sidecar supports per-deployment override files that swap sinks without
editing the preset:

```json
{
  "name": "adsb-headless",
  "extends": "adsb",
  "overrides": {
    "blocks": {
      "out": { "type": "MqttSink", "params": { "topic": "ferrite/adsb" } }
    }
  }
}
```

Overrides are resolved client-side (or sidecar-side) before validation.

## Shipped presets (v0.1)

| file                      | purpose                                |
|---------------------------|----------------------------------------|
| `flowgraphs/wbfm.json`    | WBFM listening, Phase D smoke target   |
| `flowgraphs/nbfm.json`    | narrow FM listening                    |
| `flowgraphs/am.json`      | AM listening                           |
| `flowgraphs/adsb.json`    | ADS-B, Phase E target                  |

SSB/CW, APRS, FT8, M17 flowgraphs land alongside their respective blocks
post-v0.1.

## Schema authoring

The flowgraph JSON schema lives at `packages/flowgraph-runtime/schema/v1.json`
(JSON Schema draft 2020-12). The `#[ferrite_block]` macro emits a per-block
params sub-schema into `packages/flowgraph-blocks/schemas/`, which the
runtime resolves when validating. End result: a contributor who adds a new
block gets schema validation for that block's params automatically.

## Hot reload (dev only)

In the browser, Vite HMR reloads a flowgraph JSON file when edited — the
runtime stops the old graph and starts the new one, transferring subscribed
VFO state where compatible. In Node, the sidecar watches its configured
flowgraph directory and reloads on file change (opt-in via config).
