# 03 — Block system

## What is a block

A **block** is the unit of DSP. It has:

- A **type name** (`FmDemod`, `FFT`, `AdsbDecoder`, …) that JSON flowgraphs
  reference.
- A **port schema**: zero or more typed input ports, zero or more typed output
  ports.
- A **param schema**: typed static configuration applied at construction, and
  optionally a set of param keys that accept runtime updates.
- An **implementation**: Rust code (native + WASM via one crate) or ported C
  code (via `clang --target=wasm32` + `cc` crate) wrapped in a block trait.
- A **lifecycle**: `init → start → process (many times) → stop`, with `stop`
  idempotent and drain-on-demand.

Blocks are not aware of transport, filesystems, UI, or the network. They take
samples in, produce samples out. Anything else is a **source** or **sink**
(see `04-flowgraphs.md`).

## Port and param types

### Port types

Ports carry one of a fixed set of sample streams. The set is small and
deliberate — extending it is a breaking change.

| id (Rust variant) | payload                                | notes                                         |
|-------------------|----------------------------------------|-----------------------------------------------|
| `IqF32`           | complex 32-bit float, interleaved I,Q  | primary IQ path                               |
| `IqS16`           | complex 16-bit int, interleaved I,Q    | RTL-SDR-native; avoids a float step           |
| `RealF32`         | real-valued 32-bit float               | audio, envelope, magnitude                    |
| `RealI16`         | real-valued 16-bit int                 | PCM audio                                     |
| `FftF32`          | FFT magnitude bins, 32-bit float (dB)  | raw-magnitude (pre-quantisation)              |
| `FftU8`           | FFT bins quantised to `u8` (0..=255)   | **display-ready**; output of `LogMagU8`, wire format for the waterfall |
| `Bits`            | packed bit stream                      | demodulated bitstreams; HDLC input            |
| `Frames`          | discrete framed packets (opaque bytes) | HDLC / AX.25 / Mode-S / FT8 payloads          |
| `Events`          | structured JSON events                 | decoder outputs bound for the UI              |

Sample rate and center frequency are **metadata on the port** — carried in a
lightweight struct alongside the buffer. A block's port schema declares which
rates it accepts (e.g. `FmDemod.in` accepts any rate; `AudioSink.in` accepts a
finite set). The runtime validates matches when wiring.

### Param types

Params are the knobs. Each has a key, a type, a default, and whether it
accepts **runtime updates**.

```json
{
  "bandwidth": { "kind": "range", "min": 5000, "max": 200000,
                 "default": 12500, "runtimeUpdatable": true },
  "deemphasis_us": { "kind": "enum", "values": [0, 50, 75],
                     "default": 75, "runtimeUpdatable": false }
}
```

Runtime-updatable params arrive via a dedicated `events` input or via a side
channel from the runtime; non-updatable params require a block restart to
change.

## Rust block trait

```rust
pub trait Block: Send + AsAny {
    /// Static type metadata — introspectable at registry time.
    fn spec() -> BlockSpec where Self: Sized;

    /// Called once, after construction, before any samples flow.
    /// The scheduler supplies negotiated rates and the nominal
    /// per-call frame budget via `ctx`.
    fn init(&mut self, ctx: &mut InitCtx<'_>) -> Result<()>;

    /// One scheduling tick. Reads from declared inputs, writes to
    /// declared outputs, reports what moved via `Work`. Must not
    /// allocate on the hot path.
    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work>;

    /// Per-step rate ratio between one input and one output
    /// (`(out_samples, in_samples)`). Default `(1, 1)`. Used by the
    /// scheduler to size rings.
    fn relative_rate(&self, _in_port: usize, _out_port: usize) -> (u32, u32) { (1, 1) }

    /// Optional: minimum input on each port for one `process` call to
    /// make progress. Return `None` for "1 sample is fine". Used by
    /// the scheduler to skip blocks whose inputs are too shallow.
    fn forecast(&self, _noutput_items: usize) -> Option<[usize; MAX_PORTS]> { None }

    /// Clean release. Must be idempotent.
    fn stop(&mut self) -> Result<()> { Ok(()) }
}
```

`BlockIo` gives the block ergonomic access to input buffers (typed
slices matching each port's `PortType`) and output buffers
(pre-allocated, the block fills and reports how much it wrote via
`Work`). `Work` carries `consumed: [usize; MAX_PORTS]` and `produced:
[usize; MAX_PORTS]` index-parallel to the spec's port arrays. Returning
`consumed[i] = 0` is how a block says "I need more input on port *i*
before I can run again this tick."

There is no separate `start()` — `init()` is the one-shot setup call,
and the scheduler drives `process()` directly after.

Ctor signature:

```rust
impl FmDemod {
    pub fn new(params: FmDemodParams) -> Result<Self>;
}
```

`FmDemodParams` is derived from the JSON param schema at build time. Keeping
the Rust type authoritative and generating the JSON schema from it (or vice
versa) is a cross-cutting detail — see `04-flowgraphs.md`.

## Block registration

Blocks register with the runtime via the `inventory` crate:

```rust
#[ferrite_block]
impl Block for FmDemod { /* ... */ }
```

The `#[ferrite_block]` attribute takes **no arguments** — the type name
comes from the block's own `spec().type_name`. The macro:

- Emits an `inventory::submit!` descriptor so the block is registered
  at binary link time (no init-order hazards).
- Wires up the `BlockFactory` trait so the runtime can construct the
  block from its JSON params without a manual match arm.
- Resolves both from downstream crates and from inside `blocks/` itself
  via `extern crate self as ferrite_blocks`.

The runtime calls `registry::find("FmDemod")` to look up a block type
then `BlockFactory::construct(&params)` to instantiate. Name
collisions are a runtime error at registry-walk time and are asserted
by the `registry_contains_every_shipped_block` test.

## Lifecycle

```
init()   → block materializes, param/port schemas validated
start()  → scheduler begins calling process()
process()→ one or many calls; each reports items consumed/produced
stop()   → flush, release resources
```

`init()` vs construction: construction takes raw params; `init()` is where
the block is told its negotiated sample rates and buffer sizes by the runtime.
Some blocks (e.g. FIR filter kernels) precompute coefficients here.

Blocks should be **stateless across instantiations**. Reusing an instance
across flowgraphs is not supported.

## Scheduling model

The `ferrite-runtime` scheduler is **synchronous and single-threaded**.
One `tick()` call:

- Walks blocks in pre-computed topological order (computed once at
  graph load by `topological_order()`).
- Calls `process()` on each block that has available input and
  available output slots.
- Re-walks the topology (up to 1024 passes per tick) until no block
  reports further progress. Sources run at most once per tick to
  prevent starvation of downstream blocks.

Wires between blocks are **power-of-two SPSC rings** (`TypedRing` over
`SpscRing<T>`). Samples not consumed in one tick **persist in the ring
for the next tick** — the FFT block uses exactly this to accumulate an
N-sample frame across many ticks of small-batch input.

The **only extra OS thread** in the native runtime is the `SoapySource`
reader thread, which owns the blocking Soapy `stream.read()` call and
pushes into an `Arc<Mutex<IqRing>>` that the scheduler pops from.
Nothing else runs off the tick thread.

Blocks **must not allocate on the hot path**. Scratch buffers live as
`Vec<u8>` / `Vec<Complex<f32>>` fields on the block, grown once in
`init()` or lazily on first call, then reused.

### Browser side (transitional)

Today the browser's Web Worker runs a small TS flowgraph runtime; the
main thread receives frames from `/ws/preset` directly (no Worker hop
for data). The M1–M5 plan replaces this with a WASM build of
`ferrite-runtime` so the same scheduler runs on both sides. When that
lands, the Worker gains a data plane and browser-side blocks (WASM
compiles of the same `ferrite-blocks` crate) run inside it, with a
`SharedArrayBuffer` ring between the Worker and the AudioWorklet.

## Rust → WASM build

One crate, two targets. Shape of `blocks/Cargo.toml`:

```toml
[package]
name = "ferrite-blocks"

[lib]
crate-type = ["rlib", "cdylib"]

[features]
default  = []
# Enables wasm-bindgen + serde-wasm-bindgen glue for browser targets.
wasm     = ["dep:wasm-bindgen", "dep:serde-wasm-bindgen",
            "dep:serde_bytes", "dep:js-sys"]
# Links libSoapySDR and registers `SoapySource`. Native only —
# blocks with this feature panic at monomorphization on wasm32.
soapysdr = ["dep:soapysdr", "dep:tracing"]

[dependencies]
anyhow                = "1"
ferrite-blocks-macros = { path = "../blocks-macros" }
inventory             = "0.3"
num-complex           = "0.4"
postcard              = { version = "1", default-features = false, features = ["alloc"] }
rustfft               = "6"
serde                 = { version = "1", features = ["derive"] }
serde_json            = "1"
# …optional deps gated by feature flags above
```

Native link into `ferrited` (with hardware support):

```
cargo build -p ferrited --features soapysdr
```

Browser WASM (future — built by the M5 worker-unification step):

```
wasm-pack build blocks --target web --features wasm
```

Identical source, same `cargo test` (native binary); WASM parity is
verified separately — see `05-testing.md`.

## C/C++ decoder port strategy

Projects we want to reuse — **dump1090** (ADS-B), **codec2** (audio for M17),
**ft8_lib** (kgoba) — are C with their own I/O assumptions. Strategy:

1. **Vendor only the DSP core.** Drop stdio/file/socket/audio entry points.
   Keep the math.
2. **Expose a tight C ABI.** A few functions: `adsb_decode_new`,
   `adsb_decode_feed(ctx, iq_samples, n)`, `adsb_decode_pop_frame(ctx, buf)`,
   `adsb_decode_free`. Nothing else.
3. **Compile twice.**
   - Native: Rust build script using the `cc` crate links the C objects into
     the `blocks` crate.
   - WASM: `clang --target=wasm32-unknown-unknown -fno-builtin -nostdlib`
     (or the minimal subset of libc we need via `wasi-libc` / `wasm2` shims
     if required). **Avoid Emscripten** — it drags in JS fs/stdlib glue that
     serves no purpose for us. See `06-build.md` for the actual command.
4. **Write a thin Rust block wrapper** that implements the `Block` trait and
   calls the C ABI. Same wrapper on both sides — the Rust→C FFI is identical.
5. **Ship the same golden fixtures** (recorded IQ bursts) to verify the
   native and WASM builds decode identically.

### Example block wrapper (sketch)

```rust
#[ferrite_block(type_name = "AdsbDecoder")]
pub struct AdsbDecoder {
    ctx: *mut adsb_sys::AdsbCtx,
    sample_rate: f32,
}

impl Block for AdsbDecoder {
    fn spec() -> BlockSpec { /* ports: iq_f32 in, frames out */ }

    fn init(&mut self, ctx: &mut InitCtx) -> Result<()> {
        self.sample_rate = ctx.negotiated_rate("in")?;
        assert!((self.sample_rate - 2_400_000.0).abs() < 1.0,
                "dump1090 core wants 2.4 MS/s");
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let in_buf: &[Complex<f32>] = io.input("in")?.slice();
        unsafe {
            adsb_sys::adsb_decode_feed(self.ctx, in_buf.as_ptr(), in_buf.len());
            while let Some(frame) = pop_frame(self.ctx) {
                io.output("frames")?.push(frame);
            }
        }
        Ok(Work::consumed_all())
    }

    fn stop(&mut self) -> Result<()> {
        unsafe { adsb_sys::adsb_decode_free(self.ctx); }
        Ok(())
    }
}
```

## What does not belong in a block

- **Network calls.** Sources handle that.
- **Filesystem I/O.** Sinks handle that.
- **UI updates.** The sink side emits `events` the runtime routes to the UI.
- **Global state.** Blocks own what they own; the runtime owns scheduling.
- **Threads of their own.** The runtime decides where they run.

Blocks with side effects are always wrong. The AdsbDecoder wrapper above has
no side effects — the "writing to stdout" you may remember from dump1090 is
gone; frames come out of an output port and the flowgraph decides what a
sink does with them.

## Shipped blocks

Registered blocks (asserted by
`registry_contains_every_shipped_block` in `blocks/src/lib.rs`):

### Sources

| block              | ports               | placement    | purpose                                                                    |
|--------------------|---------------------|--------------|----------------------------------------------------------------------------|
| `Source`           | — → IqF32           | author-pinned| placeholder resolved by `compose_source` at load time (see flowgraphs doc) |
| `SoapySource`      | — → IqF32           | NativeOnly   | RTL-SDR / SDRPlay / any Soapy device (requires `soapysdr` feature)         |
| `FileIqSource`     | — → IqF32           | NativeOnly   | reads IQ from a local file (`cf32` raw or `wav-s16`)                       |
| `SineSource`       | — → IqF32           | Either       | synthetic tone — test fixture                                              |
| `DtmfAudioSource`  | — → RealF32         | Either       | synthetic DTMF generator (tests / demos)                                   |

### DSP

| block          | ports                       | placement | purpose                                                 |
|----------------|-----------------------------|-----------|---------------------------------------------------------|
| `Channelizer`  | IqF32 → IqF32               | Either    | frequency shift + FIR + decimate, one VFO               |
| `Decimator`    | IqF32 → IqF32               | Either    | FIR + decimate                                          |
| `TeeIqF32`     | IqF32 → 2 × IqF32           | Either    | 1 → 2 IQ fan-out                                        |
| `FFT`          | IqF32 → IqF32 (bins)        | Either    | windowed DFT with input accumulation to size N          |
| `LogMagU8`     | IqF32 (bins) → FftU8        | Either    | log-magnitude → dBFS → smooth → `u8` 0..=255            |
| `FmDemod`      | IqF32 → RealF32             | Either    | phase-discriminator WBFM demod                          |
| `AmDemod`      | IqF32 → RealF32             | Either    | envelope AM demod                                       |
| `AmModulator`  | RealF32 → IqF32             | Either    | AM modulator (test fixture / loopback)                  |
| `DtmfDecoder`  | RealF32 → Events            | Either    | Goertzel-based DTMF digit detector                      |

### Sinks

| block            | ports                 | placement    | purpose                                                             |
|------------------|-----------------------|--------------|---------------------------------------------------------------------|
| `AudioSink`      | RealF32 → —           | WasmOnly     | feeds AudioWorklet ring (SAB) in the browser                        |
| `FileIqSink`     | IqF32 → —             | NativeOnly   | capture mode — writes `cf32` or `wav-s16` + JSON sidecar            |
| `EventsSink`     | Events → —            | NativeOnly   | terminal decoder events (logs today; MQTT / HTTP webhook planned)   |

### Cross-env bridges (auto-inserted by `env_split`)

| block               | ports            | placement  | notes                                                                                                 |
|---------------------|------------------|------------|-------------------------------------------------------------------------------------------------------|
| `WsBridgeTx`        | IqF32 → —        | NativeOnly | egress side of an IQ crossing                                                                         |
| `WsBridgeTxFftU8`   | FftU8 → —        | NativeOnly | egress side for FFT streams (waterfall feed)                                                          |
| `WsBridgeRx`        | — → IqF32        | WasmOnly   | browser-side ingress; also authored directly when a preset subscribes to a server-published stream_id |

Planned (see `docs/decoder-roadmap/`):

| block           | ports                              | purpose                              | phase |
|-----------------|------------------------------------|--------------------------------------|-------|
| `SsbDemod`      | iq_f32 → real_f32                  | SSB listening                        | 1     |
| `Squelch`       | real_f32 → real_f32                | carrier-gate                         | 1     |
| `Deemphasis`    | real_f32 → real_f32                | FM de-emphasis                       | 1     |
| `Agc`           | real_f32 → real_f32                | audio AGC                            | 1     |
| `Resample`      | real_f32 → real_f32                | rational-rate resampler              | 1     |
| `AdsbDecoder`   | iq_f32 → frames                    | ADS-B (dump1090 port)                | 3     |
| `Multimon`      | real_f32 → events                  | POCSAG/FLEX/DTMF/EAS/CTCSS umbrella  | 2     |
| `Ft8Decoder`    | real_f32 → events                  | FT8 (ft8_lib port)                   | 4     |
| `Codec2Decoder` | frames → real_f32                  | M17 audio                            | 6     |
