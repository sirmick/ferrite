# 04 — Flowgraphs

A **flowgraph** is a JSON document describing a graph of block instances and
their wiring. The `ferrite-runtime` crate parses it
([`runtime/src/doc.rs`](../runtime/src/doc.rs)), validates ports/params,
splits it for the current environment
([`runtime/src/env_split.rs`](../runtime/src/env_split.rs)), instantiates
blocks from the registry, and runs them.

Shipped presets live in [`flowgraphs/`](../flowgraphs/). For the block trait
and the registry of types those presets reference, see
[03-blocks.md](03-blocks.md).

## Top-level schema

```json
{
  "$schema":      "https://ferrite.example/flowgraph-v1.json",
  "name":         "wbfm",
  "label":        "FM broadcast (mono)",
  "description":  "...",
  "environments": ["node", "browser"],
  "blocks": {
    "<instance_id>": {
      "type":      "<BlockTypeName>",
      "placement": "node" | "browser",
      "params":    { ... }
    }
  },
  "wires": [
    ["<src_id>.<out_port>", "<dst_id>.<in_port>"],
    ["<src_id>.<out_port>", "ui:<name>"]
  ]
}
```

`FlowgraphDoc` ([`runtime/src/doc.rs`](../runtime/src/doc.rs)):

```rust
pub enum Environment { Browser, Node }

pub struct BlockInstanceDecl {
    #[serde(rename = "type")] pub type_name: String,
    pub params:    Option<serde_json::Value>,
    pub placement: Option<Environment>,
}

pub struct Wire { pub src: String, pub dst: String }

pub struct FlowgraphDoc {
    pub schema:       Option<String>,
    pub name:         String,
    pub label:        Option<String>,
    pub description:  Option<String>,
    pub environments: Vec<Environment>,
    pub blocks:       BTreeMap<String, BlockInstanceDecl>,
    pub wires:        Vec<Wire>,
}
```

`environments` lists which sides this preset has work on. Single-env presets
declare one (e.g. the recording presets are `["node"]`); cross-env presets
declare both. Block types with `Placement::NativeOnly` or `WasmOnly` pin
themselves; `Placement::Either` blocks accept a per-instance `placement`
field and otherwise have their side inferred from neighbours during
`split_for_environment`.

Instance IDs are local to the preset. Convention: lower_snake_case nouns. They
appear in error messages, so meaningful names help.

## The `Source` placeholder

Every preset that needs a real source authors it as a placeholder:

```json
"src": {
  "type": "Source",
  "placement": "node",
  "params": {
    "center_freq_hz": 100100000,
    "sample_rate_hz": 2400000,
    "bandwidth_hz":   2000000
  }
}
```

`compose_source` ([`runtime/src/compose.rs`](../runtime/src/compose.rs))
replaces this at load time with the current `SourceConfig` (held on
`AppState` and patched via `PATCH /api/source`). The placeholder's params
become the *base* object; `SourceConfig.params` overlays key-by-key, so
hardware-specific params (e.g. `args = "driver=rtlsdr"`) stay separate from
preset-level tuning hints.

The instance id is fixed: `SOURCE_ID = "src"`. The placeholder type name is
`SOURCE_SENTINEL_TYPE = "Source"` (both in
`runtime/src/compose.rs`).

Swapping the device is a `PATCH /api/source` call; the preset doesn't change.

## `ui:<name>` sinks

A wire whose right-hand side starts with `ui:` declares a UI-bound output
rather than a real input port:

```json
["logmag.out", "ui:fft"]
```

`split_for_environment` rewrites this to the appropriate `WsBridgeTx*` block
on the producing side (e.g. `WsBridgeTxFftU8` for FftU8) and emits *nothing*
on the browser side. The browser learns the allocated `stream_id` and
payload type via `GET /api/ui-sinks`, then subscribes through the same
`/ws/preset` connection it uses for cross-env wires.

## Cross-env wires (auto bridges)

When a wire crosses the env boundary, `split_for_environment` rewrites it:

- **Node → browser** — inserts a `WsBridgeTx` (or a typed variant) on the
  node side and a `WsBridgeRx` on the browser side, sharing a `stream_id`.
- **Browser → node** — rejected (`SplitError::UnsupportedCrossing`). Not
  needed by any shipped preset.

Stream IDs are allocated deterministically starting at
`CROSS_ENV_STREAM_BASE = 1000` (`runtime/src/env_split.rs`). The order is
fixed (UI sinks + cross-env wires enumerated in declaration order), so
server and browser arrive at the same numbers without negotiation.

Authors do not hand-write the bridge blocks. Either side can read its own
post-split doc by calling `RuntimeHandle::split_doc_for_environment` (WASM)
or `split_for_environment` (native).

## Wires

```json
"wires": [
  ["src.out",   "tee.in"],
  ["tee.out0",  "chan.in"],
  ["tee.out1",  "fft.in"],
  ["fft.out",   "logmag.in"],
  ["logmag.out","ui:fft"]
]
```

Rules:

- One output, one input, one wire — fan-out is via a `Tee*` block.
- Port types must match (`IqF32` ↔ `IqF32`, etc.).
- The validator runs before any block is constructed; failures are
  structured errors with the offending wire endpoints.

## Validation order

Implemented in `runtime/src/{doc.rs,env_split.rs,runtime.rs}`. Roughly:

1. JSON shape parses to `FlowgraphDoc`.
2. `split_for_environment` resolves per-block placement, inserts bridges,
   and produces an env-local doc.
3. Block types in the env-local doc exist in the registry with matching
   `Placement`.
4. Each block's params construct successfully (per-block JSON deserialise
   inside `BlockEntry::construct`).
5. Wire endpoints reference real ports; port types match.
6. Topological sort succeeds (no cycles).
7. `Runtime::init` runs each block's `init(ctx)`; rate negotiation happens
   here.

Any failure aborts. Errors carry the relevant block id / wire / param key
so the UI can render them inline.

## Example: WBFM (cross-env)

Verbatim shape of [`flowgraphs/wbfm.json`](../flowgraphs/wbfm.json):

- `src` (Source placeholder, node) — 100.1 MHz, 2.4 MS/s, 2 MHz BW.
- `tee` (TeeIqF32, node) — fans the wideband IQ in two.
- `fft` + `logmag` (node) → `ui:fft` — server-side FFT tap; the waterfall
  shows the full 2.4 MHz span.
- `chan` (Channelizer, node) — picks 240 kS/s out of the 2.4 MS/s wideband.
- *(cross-env wire `chan.out → demod.in` — auto-bridged, stream_id 1001)*
- `demod` (FmDemod, browser) — discriminates ±75 kHz at channel rate.
- `decim` (RealF32Decimator, browser) — drops to 48 kHz audio.
- `audio` (AudioSink, browser) — fills the SAB ring.

Wire shape:

```
src → tee ──> chan → demod → decim → audio   (chan→demod crosses env)
           └─> fft  → logmag → ui:fft        (server-side spectrum tap)
```

`audio`'s `placement` is omitted because `AudioSink` is `WasmOnly`;
`Channelizer` is `NativeOnly` and pins itself; everything else carries an
explicit `placement` for clarity.

## Shipped presets

Catalog (browser-visible, picked from the Signal Catalog panel):

| file                                                                                  | shape                                                                                  |
|---------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------|
| [`flowgraphs/wbfm.json`](../flowgraphs/wbfm.json)                                     | WBFM — wideband + spectrum tap + cross-env audio + RDS                                 |
| [`flowgraphs/wbfm_stereo.json`](../flowgraphs/wbfm_stereo.json)                       | WBFM with stereo pilot decode                                                          |
| [`flowgraphs/wbam.json`](../flowgraphs/wbam.json)                                     | AM broadcast                                                                           |
| [`flowgraphs/nbfm.json`](../flowgraphs/nbfm.json)                                     | Narrowband FM (marine, FRS/GMRS, ham 2m FM)                                            |
| [`flowgraphs/lsb.json`](../flowgraphs/lsb.json)                                       | SSB lower sideband                                                                     |
| [`flowgraphs/usb.json`](../flowgraphs/usb.json)                                       | SSB upper sideband                                                                     |
| [`flowgraphs/cw.json`](../flowgraphs/cw.json)                                         | CW with multimon-ng Morse decoder                                                      |
| [`flowgraphs/aprs.json`](../flowgraphs/aprs.json)                                     | APRS / AX.25 packet — AFSK1200 + 2400 variants + FSK9600 in parallel                   |
| [`flowgraphs/aprs-debug.json`](../flowgraphs/aprs-debug.json)                         | APRS with audio fan-out to WAV for offline A/B against `analyze-packet-wav`            |
| [`flowgraphs/pager.json`](../flowgraphs/pager.json)                                   | POCSAG (3 baud) + FLEX + FLEX_NEXT in parallel                                         |
| [`flowgraphs/nwr.json`](../flowgraphs/nwr.json)                                       | NOAA Weather Radio (NBFM + EAS)                                                        |
| [`flowgraphs/adsb.json`](../flowgraphs/adsb.json)                                     | ADS-B / Mode S — vendored dump1090, 1090 MHz                                           |
| [`flowgraphs/dtmf-e2e.json`](../flowgraphs/dtmf-e2e.json)                             | DTMF events-bridge canary across the server/browser split                              |

Headless / capture (node-only, launched with `--run-for-secs N`):

| file                                                                                  | shape                                                                                  |
|---------------------------------------------------------------------------------------|----------------------------------------------------------------------------------------|
| [`flowgraphs/fm-audio-record.json`](../flowgraphs/fm-audio-record.json)               | All-native FM mono → FileAudioSink                                                     |
| [`flowgraphs/am-audio-record.json`](../flowgraphs/am-audio-record.json)               | All-native AM mono → FileAudioSink                                                     |
| [`flowgraphs/capture_fm.json`](../flowgraphs/capture_fm.json)                         | Wideband IQ capture → FileIqSink (no demod)                                            |
| [`flowgraphs/capture-aprs.json`](../flowgraphs/capture-aprs.json)                     | APRS audio capture for offline analyzer fixtures                                       |
| [`flowgraphs/capture-pager.json`](../flowgraphs/capture-pager.json)                   | POCSAG / FLEX audio capture for offline analyzer fixtures                              |
| [`flowgraphs/morse-e2e.json`](../flowgraphs/morse-e2e.json)                           | Morse round-trip canary (synthesise → decode)                                          |

Catalog entries carry topology only (no `src.center_freq_hz` /
`chan.freq_shift_hz`); per-band tuning lives in
`web/src/lib/presets/bands.json` and is applied on top via the Bands
panel. See [D27](09-decisions.md#d27--catalog--bands-separation-catalog-is-what-to-demod-bands-is-where-to-listen).

## Runtime params

Blocks declare per-param `reconfig_scope` (see
[03-blocks.md](03-blocks.md#params-paramspec-paramkind-reconfigurescope)).
The web UI dispatches param edits through `setBlockParam`
([`web/src/lib/pipeline.svelte.ts`](../web/src/lib/pipeline.svelte.ts) +
[`api/pipelineBlocks.ts`](../web/src/lib/api/pipelineBlocks.ts)):

- `placement = node` blocks → `POST /api/pipeline/blocks/:id/params`,
  which routes through `PresetMount::reconfigure` and applies the diff.
- `placement = browser` blocks → `RuntimeHandle::reconfigure_block(id, json)`
  on the in-Worker WASM runtime.

`SelfBlock`-scope changes are absorbed in-place via `apply_live_params`;
`Downstream`-scope re-`init`s the block plus everything downstream;
`SourceRestart`-scope tears down and rebuilds the source half. The diff
engine takes the coarsest scope across the patch.

## Schema authoring

Block param schemas come from each block's `BlockSpec::params` (static
`&[ParamSpec]`). The server publishes them at `GET /api/blocks`; the UI's
`<BlockParams>` component
([`web/src/lib/controls/BlockParams.svelte`](../web/src/lib/controls/BlockParams.svelte))
renders the right control widget directly from `ParamKind`. Adding a new
block type makes its params editable in the receiver pane without any
Svelte changes.
