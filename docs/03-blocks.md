# 03 — Block system

Blocks are the unit of DSP. The trait and supporting types live in
[`blocks/src/block.rs`](../blocks/src/block.rs); each shipped block is one
file in `blocks/src/`. The `#[ferrite_block]` proc-macro
([`blocks-macros/src/lib.rs`](../blocks-macros/src/lib.rs)) registers the
block at link time via `inventory`.

This doc reflects what's in the source today. For the JSON schema that wires
blocks together see [04-flowgraphs.md](04-flowgraphs.md); for the wire frame
format see [02-protocol.md](02-protocol.md).

## The `Block` trait

[`blocks/src/block.rs:423-545`](../blocks/src/block.rs):

```rust
pub trait Block: Send + AsAny {
    fn spec() -> BlockSpec where Self: Sized;
    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()>;
    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work>;

    fn output_capacity_hints(&self) -> [usize; MAX_PORTS] { [0; MAX_PORTS] }
    fn relative_rate(&self, _in_port: usize, _out_port: usize) -> (u32, u32) { (1, 1) }
    fn forecast(&self, _noutput_items: usize) -> Option<[usize; MAX_PORTS]> { None }
    fn stop(&mut self) -> Result<()> { Ok(()) }
    fn apply_live_params(&mut self, _delta: &serde_json::Value) -> Result<bool> { Ok(false) }
}
```

- `spec()` returns a static `BlockSpec` (see "BlockSpec" below). Available
  without an instance so the registry can enumerate types at startup.
- `init(ctx)` runs once after construction. The runtime supplies the
  per-tick budget (`ctx.frames_hint()`) and other context. FIR coefficients
  and similar precomputation belong here.
- `process(io)` is one scheduling pass. It reports samples consumed/produced
  per port via `Work`; the scheduler advances each input ring's reader head
  by `consumed[i]` and each output ring's writer head by `produced[i]`.
  Unconsumed input persists for the next call. Must not allocate on the
  hot path.
- `relative_rate` declares a sync-block ratio `(out_samples, in_samples)`
  — `(1, N)` for a decimator, `(L, 1)` for an interpolator. Defaults to
  `(1, 1)`.
- `forecast(noutput)` — for variable-rate or fixed-chunker blocks. Returns
  the per-input-port samples required to make `noutput` outputs.
- `apply_live_params(delta)` — try to absorb a JSON params delta in-place.
  Returns `Ok(true)` if absorbed, `Ok(false)` if the runtime should fall back
  to a stop-rebuild-start. `SourceRestart`-scope changes always rebuild.

`Work`, `BlockIo`, `InitCtx`, and `MAX_PORTS` are all defined alongside the
trait in `block.rs`. `Work.consumed` and `Work.produced` are
`[usize; MAX_PORTS]` arrays index-parallel to `BlockSpec.inputs/outputs`.

## `BlockSpec`, `Placement`, port types

```rust
pub struct BlockSpec {
    pub type_name: &'static str,
    pub placement: Placement,
    pub inputs:  &'static [PortDecl],
    pub outputs: &'static [PortDecl],
    pub params:  &'static [ParamSpec],
}

pub enum Placement { NativeOnly, WasmOnly, Either }
```

`type_name` is the string presets reference in `"type": …`.

`Placement` decides where the block can land:

- `NativeOnly` — server-side only (`Channelizer`, `SoapySource`, file I/O,
  the WS-bridge Tx blocks).
- `WasmOnly` — browser-side only (`AudioSink`, `WsBridgeRx`).
- `Either` — anywhere; the preset's per-block `placement` field pins it,
  or `env_split` infers from neighbours.

Port types ([`block.rs:35-73`](../blocks/src/block.rs)):

| `PortType` | element            | bytes | use                          |
|------------|--------------------|-------|------------------------------|
| `IqF32`    | `Complex<f32>`     | 8     | primary IQ path              |
| `IqS16`    | `Complex<i16>`     | 4     | RTL-SDR-native, no float pass|
| `RealF32`  | `f32`              | 4     | audio, envelope, magnitude   |
| `RealI16`  | `i16`              | 2     | PCM audio                    |
| `FftF32`   | `f32` (dB)         | 4     | raw log-magnitude            |
| `FftU8`    | `u8` (0..=255)     | 1     | display-ready spectrum       |
| `Bits`     | `u8`               | 1     | demodulated bitstream        |
| `Frames`   | opaque `u8`        | 1     | HDLC / Mode-S / FT8 packets  |
| `Events`   | JSON event bytes   | 1     | decoder→UI events            |

## Params: `ParamSpec`, `ParamKind`, `ReconfigureScope`

[`blocks/src/block.rs:82-206`](../blocks/src/block.rs):

```rust
pub struct ParamSpec {
    pub key:           &'static str,
    pub label:         &'static str,
    pub kind:          ParamKind,
    pub reconfig_scope: ReconfigureScope,
}

pub enum ParamKind {
    Range        { min: f64, max: f64, step: f64, default: f64, unit: &'static str },
    EnumNumeric  { values: &'static [f64], default: f64, unit: &'static str },
    EnumString   { values: &'static [&'static str], default: &'static str },
    Toggle       { default: bool },
    Text         { default: &'static str },
}

pub enum ReconfigureScope {
    SelfBlock,      // wire form "self"          (cost 0)
    Downstream,     // wire form "downstream"    (cost 1)
    SourceRestart,  // wire form "sourceRestart" (cost 2 — default)
}
```

Min/max/step/unit (D24) are already on `ParamSpec`; the
`<BlockParams>` Svelte component
([`web/src/lib/controls/BlockParams.svelte`](../web/src/lib/controls/BlockParams.svelte))
renders the right control widget directly from `ParamKind`.

`ReconfigureScope` drives the diff engine. `Self` is in-place via
`apply_live_params`; `Downstream` re-`init`s the owning block plus every
downstream block; `SourceRestart` tears the source half down and rebuilds
it. When a multi-param patch touches multiple scopes, the runtime takes the
coarsest (`merge()`).

## Registration: `#[ferrite_block]`

```rust
#[ferrite_block]
impl Block for FmDemod { /* … */ }
```

The macro takes no arguments. It:

- Preserves the `impl` verbatim.
- Emits `inventory::submit!` of a `BlockEntry { spec_fn, construct_fn }`
  pointing to the impl's `spec()` and a generated `construct(params)`. Link-
  time registration; no init-order hazards.
- Resolves whether it's invoked from inside `blocks/` (via
  `extern crate self as ferrite_blocks`) or from a downstream crate.

The runtime calls `registry::find(type_name)` then `BlockEntry::construct`
to instantiate from JSON params. The
`registry_contains_every_shipped_block` test in
[`blocks/src/lib.rs`](../blocks/src/lib.rs) is the source of truth for what's
shipped — it enumerates the registry at test time and asserts the expected
set is present.

## Lifecycle

```
construct(params)  → block materializes from JSON
init(ctx)          → one-shot setup (FIR coefficients, scratch buffers)
process(io)        → many; reports Work { consumed, produced }
apply_live_params  → optional in-place delta application
stop()             → idempotent flush + release
```

Construction is from the params JSON in the preset (the macro generates the
glue). `init` is where rate-dependent state gets sized. The scheduler
re-enters `process` until no block makes further progress in a tick (up to
`MAX_TICK_PASSES = 1024` passes).

Blocks must be **stateless across instantiations**. Reusing an instance
across flowgraphs is not supported.

## Frame enum (the wire format)

[`blocks/src/frame.rs`](../blocks/src/frame.rs):

```rust
pub enum Frame {
    IqF32     { stream_id: u16, seq: u32, timestamp_ns: u64, payload: Vec<u8> },
    FftU8     { stream_id: u16, seq: u32, timestamp_ns: u64, payload: Vec<u8> },
    JsonEvent { stream_id: u16, seq: u32, timestamp_ns: u64, payload: Vec<u8> },
}

pub const CONTROL_STREAM:  u16 = 0;
pub const FFT_STREAM:      u16 = 1;
pub const VFO_STREAM_BASE: u16 = 2;
```

Encoded with `postcard`. The variant tag *is* the protocol discriminator.
The browser decodes via `decodeFrame` exported from this same crate
(through wasm-bindgen), so any schema change is a build break, not a
runtime ambiguity. See [02-protocol.md](02-protocol.md) for stream-id
allocation and event payload schemas.

## Cross-env bridges

Auto-inserted by `split_for_environment` (see
[04-flowgraphs.md](04-flowgraphs.md) and `runtime/src/env_split.rs`):

| block               | direction    | port    | placement   |
|---------------------|--------------|---------|-------------|
| `WsBridgeTx`        | node → wire  | IqF32   | NativeOnly  |
| `WsBridgeTxFftU8`   | node → wire  | FftU8   | NativeOnly  |
| `WsBridgeRx`        | wire → browser| IqF32  | WasmOnly    |

(`Events` `ui:` sinks don't bridge over the sample WS — `env_split`
terminates them in an `EventStore` that folds records into the
server-side store, mirrored to the browser over `/ws/state`.)

All Tx blocks call `BridgeSink::push(Frame::…)` with a zero-stamped
envelope; the server's `BroadcastSink`
([`server/src/bridge_sink.rs`](../server/src/bridge_sink.rs)) overrides
`seq` and `timestamp_ns` from per-stream counters before the postcard
serialise. The Rx side holds an internal `IqRing`; the browser's
`runnerCore` pushes decoded payloads in via the WASM facade's `push_iq`.

`WsIqSource` was the predecessor to `WsBridgeRx` and was deleted in D22.

## Shipped blocks

~50 registered blocks plus the soapysdr-feature-gated `SoapySource`.
Source of truth: `registry_contains_every_shipped_block` in
[`blocks/src/lib.rs`](../blocks/src/lib.rs) (the link-time inventory is
the authority; the tables below are a curated tour, not exhaustive).

### Sources

| type              | output          | placement   | file                                                          |
|-------------------|-----------------|-------------|---------------------------------------------------------------|
| `Source`          | IqF32           | placeholder | resolved by `compose_source`                                  |
| `SoapySource`     | IqF32           | NativeOnly  | [`blocks/src/soapy_source.rs`](../blocks/src/soapy_source.rs) (feature `soapysdr`) |
| `FileIqSource`    | IqF32           | NativeOnly  | [`blocks/src/file_source.rs`](../blocks/src/file_source.rs)   |
| `SineSource`      | IqF32           | Either      | [`blocks/src/sine.rs`](../blocks/src/sine.rs)                 |
| `DtmfAudioSource` | RealF32         | Either      | [`blocks/src/dtmf_audio_source.rs`](../blocks/src/dtmf_audio_source.rs) |

### DSP

| type                | ports                     | placement   | file                                                          |
|---------------------|---------------------------|-------------|---------------------------------------------------------------|
| `Channelizer`       | IqF32 → IqF32             | NativeOnly  | [`blocks/src/channelizer.rs`](../blocks/src/channelizer.rs)   |
| `Decimator`         | IqF32 → IqF32             | Either      | [`blocks/src/decimator.rs`](../blocks/src/decimator.rs)       |
| `RealF32Decimator`  | RealF32 → RealF32         | Either      | [`blocks/src/real_decimator.rs`](../blocks/src/real_decimator.rs) |
| `TeeIqF32`          | IqF32 → 2 × IqF32         | Either      | [`blocks/src/tee_iq_f32.rs`](../blocks/src/tee_iq_f32.rs)     |
| `FFT`               | IqF32 → IqF32 (bins)      | Either      | [`blocks/src/fft.rs`](../blocks/src/fft.rs)                   |
| `LogMagU8`          | IqF32 (bins) → FftU8      | Either      | [`blocks/src/log_mag_u8.rs`](../blocks/src/log_mag_u8.rs)     |
| `FmDemod`           | IqF32 → RealF32           | Either      | [`blocks/src/fm_demod.rs`](../blocks/src/fm_demod.rs)         |
| `AmDemod`           | IqF32 → RealF32           | Either      | [`blocks/src/am_demod.rs`](../blocks/src/am_demod.rs)         |
| `AmModulator`       | RealF32 → IqF32           | Either      | [`blocks/src/am_modulator.rs`](../blocks/src/am_modulator.rs) |
| `DtmfDecoder`       | RealF32 → Events          | Either      | [`blocks/src/dtmf_decoder.rs`](../blocks/src/dtmf_decoder.rs) |

### Sinks

| type             | input    | placement   | file                                                              |
|------------------|----------|-------------|-------------------------------------------------------------------|
| `AudioSink`      | RealF32  | WasmOnly    | [`blocks/src/audio_sink.rs`](../blocks/src/audio_sink.rs)         |
| `FileIqSink`     | IqF32    | NativeOnly  | [`blocks/src/file_sink.rs`](../blocks/src/file_sink.rs)           |
| `FileAudioSink`  | RealF32  | NativeOnly  | [`blocks/src/file_audio_sink.rs`](../blocks/src/file_audio_sink.rs) |
| `EventsSink`     | Events   | Either      | [`blocks/src/events_sink.rs`](../blocks/src/events_sink.rs)       |

### Decoders

All `Placement::Either` — they decode native in `ferrited` *and* in
the browser wasm runtime (D28). Each emits newline-delimited JSON on an
`events` port (→ `ui:<name>` → the matching advanced view) and mirrors
the same text to a `decoder::<cat>` tracing target for `tail decoder`.
fldigi modems are C++/STL: native = static link, wasm32 = link-vs-bridge
to a sibling Emscripten module (see [01-architecture.md](01-architecture.md)).

| type                | backing                          | file                                                          |
|---------------------|----------------------------------|---------------------------------------------------------------|
| `AdsbDemod`         | dump1090                         | [`blocks/src/adsb.rs`](../blocks/src/adsb.rs)                 |
| `AisDemod`          | rtl-ais                          | [`blocks/src/ais.rs`](../blocks/src/ais.rs)                   |
| `PacketDemod`       | multimon-ng (AFSK/AX.25 → APRS)  | [`blocks/src/packet.rs`](../blocks/src/packet.rs)            |
| `PagerDemod`        | multimon-ng (POCSAG/FLEX)        | [`blocks/src/pager.rs`](../blocks/src/pager.rs)              |
| `EasDemod`          | multimon-ng (EAS/SAME)           | [`blocks/src/eas.rs`](../blocks/src/eas.rs)                  |
| `MorseDemod`        | multimon-ng (CW)                 | [`blocks/src/morse.rs`](../blocks/src/morse.rs)             |
| `RdsDemod`          | in-tree                          | [`blocks/src/rds_demod.rs`](../blocks/src/rds_demod.rs)      |
| `Rtl433Demod`       | rtl_433 (ISM)                    | [`blocks/src/rtl_433.rs`](../blocks/src/rtl_433.rs)         |
| `Ft8Demod`          | ft8_lib (FT8 / FT4, slot-timed)  | [`blocks/src/ft8.rs`](../blocks/src/ft8.rs)                  |
| `WsprDemod`         | wsprd (WSPR, 120 s slot)         | [`blocks/src/wspr.rs`](../blocks/src/wspr.rs)                |
| `RttyDemod` `Psk31Demod` `CwDemod` `Mt63Demod` `OliviaDemod` `ContestiaDemod` `DominoexDemod` `ThrobDemod` `NavtexDemod` | fldigi v4.2.11 cores | [`blocks/src/fldigi_modes.rs`](../blocks/src/fldigi_modes.rs) |
| `FldigiAuto`        | fldigi RSID — hot-swaps the inner modem on detect | [`blocks/src/fldigi_modes.rs`](../blocks/src/fldigi_modes.rs) |

### Bridges (auto-inserted by `env_split`)

| type                | direction       | port   | placement   |
|---------------------|-----------------|--------|-------------|
| `WsBridgeTx`        | node → browser  | IqF32  | NativeOnly  |
| `WsBridgeTxFftU8`   | node → browser  | FftU8  | NativeOnly  |
| `WsBridgeRx`        | wire → browser  | IqF32  | WasmOnly    |

All in [`blocks/src/ws_bridge.rs`](../blocks/src/ws_bridge.rs).

## What does not belong in a block

- **Network calls.** `Source` blocks own ingress; bridges own cross-env wire.
- **Filesystem I/O.** `FileIq{Source,Sink}` and `FileAudioSink` exist for
  file work; nothing else should touch the disk.
- **UI updates.** Blocks emit `Events` payloads; sinks decide what to do.
- **Threads of their own.** `SoapySource`'s reader is the one exception and
  exists because Soapy's `read()` is blocking.

## Dual-compile

`blocks/Cargo.toml` declares `crate-type = ["rlib", "cdylib"]` and two
features:

- `wasm` — pulls in `wasm-bindgen`, `serde-wasm-bindgen`, `serde_bytes`,
  `js-sys`. The `web/` package's `wasm:build:blocks` script enables it.
- `soapysdr` — links `libSoapySDR` and registers `SoapySource`. Native only;
  blocks behind this feature panic at monomorphization on `wasm32`.

Native test (covers most logic): `cargo test -p ferrite-blocks`. The
authored `blocks/tests/wbfm_e2e.rs` is a synthetic-FM round-trip that
doubles as a parity anchor.

