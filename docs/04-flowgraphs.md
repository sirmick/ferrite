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
  "environments": ["browser"],
  "blocks": {
    "<instance_id>": {
      "type": "<BlockTypeName>",
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
| `environments` | string[]          | which runtimes can host this graph                |
| `blocks`       | object            | instance_id → block declaration                   |
| `wires`        | array of [a, b]   | connections, `a` is output, `b` is input          |

`environments` values: `"browser"`, `"node"`. A graph that only makes sense
in one environment (e.g. needs an `AudioSink`, only available in `browser`)
declares that here. The runtime refuses to instantiate in a mismatched env.

### Instance IDs

Instance IDs are local to a flowgraph. They must be unique within the
flowgraph, URL-safe, and human-meaningful (you will read them in error
messages). Convention: lower_snake_case nouns.

## Sources and sinks

Sources and sinks are **blocks** from the runtime's perspective (they implement
the same trait), but they are environment-specific:

### Browser sources/sinks

| type              | direction | notes                                                   |
|-------------------|-----------|---------------------------------------------------------|
| `WsIqSource`      | source    | subscribes to a VFO stream on the ferrited WS           |
| `WsFftSource`     | source    | subscribes to the waterfall FFT stream                  |
| `AudioSink`       | sink      | feeds the AudioWorklet ring buffer                      |
| `OpfsFileSink`    | sink      | writes to Origin Private File System                    |
| `EventBusSink`    | sink      | publishes `events` payload to a Svelte store / bus      |

### Node sources/sinks

| type              | direction | notes                                                   |
|-------------------|-----------|---------------------------------------------------------|
| `WsIqSource`      | source    | same WS subscription, loopback                          |
| `WsFftSource`     | source    | same                                                    |
| `FsFileSink`      | sink      | `fs.createWriteStream` to a path                        |
| `MqttSink`        | sink      | publishes to an MQTT topic                              |
| `SyslogSink`      | sink      | RFC 5424 syslog                                         |
| `SqliteSink`      | sink      | insert into a SQLite table                              |

### Sinks/sources are listed per-environment

The runtime's block registry is keyed not only by type name but by the
environment it supports. A flowgraph that references `AudioSink` in its
blocks with `"environments": ["node"]` fails validation.

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

## Example: WBFM

```json
{
  "name": "wbfm",
  "label": "WBFM broadcast",
  "description": "Wideband FM broadcast demodulator with de-emphasis.",
  "environments": ["browser"],
  "blocks": {
    "src":   { "type": "WsIqSource", "params": { "stream": "vfo.primary" } },
    "demod": { "type": "FmDemod",    "params": { "bandwidth": 200000,
                                                  "deemphasis_us": 75 } },
    "decim": { "type": "Decimator",  "params": { "out_rate": 48000 } },
    "audio": { "type": "AudioSink",  "params": { "rate": 48000 } }
  },
  "wires": [
    ["src.out",   "demod.in"],
    ["demod.out", "decim.in"],
    ["decim.out", "audio.in"]
  ]
}
```

## Example: ADS-B

```json
{
  "name": "adsb",
  "label": "ADS-B (Mode S)",
  "description": "Aircraft position/velocity decoding on 1090 MHz.",
  "environments": ["browser", "node"],
  "blocks": {
    "src":     { "type": "WsIqSource", "params": { "stream": "vfo.adsb" } },
    "decoder": { "type": "AdsbDecoder", "params": {} },
    "out":     { "type": "EventBusSink", "params": { "topic": "adsb" } }
  },
  "wires": [
    ["src.out",     "decoder.in"],
    ["decoder.frames", "out.in"]
  ]
}
```

Note `environments` includes `node` — this flowgraph runs equally well in
the browser (EventBusSink → map panel) and in the headless sidecar
(EventBusSink → MqttSink swap, via a deployment-specific override layer).

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
