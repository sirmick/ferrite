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

| id          | payload                                   | notes                                  |
|-------------|-------------------------------------------|----------------------------------------|
| `iq_f32`    | complex 32-bit float (native endian)       | primary IQ path                         |
| `iq_s16`    | complex 16-bit int                         | RTL-SDR-native; avoids a float step    |
| `real_f32`  | real-valued 32-bit float                   | audio, envelope, magnitude             |
| `real_i16`  | real-valued 16-bit int                     | PCM audio                              |
| `fft_f32`   | FFT magnitude bins, 32-bit float           | log-magnitude in dB                    |
| `bits`      | packed bit stream                          | demodulated bitstreams; HDLC input     |
| `frames`    | discrete framed packets (opaque bytes)     | HDLC / AX.25 / Mode-S / FT8 payloads   |
| `events`    | structured JSON events                     | decoder outputs bound for the UI       |

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
pub trait Block: Send {
    /// Static type metadata — introspectable at registry time.
    fn spec() -> BlockSpec where Self: Sized;

    /// Called once, before any samples flow.
    fn init(&mut self, ctx: &mut InitCtx) -> Result<()>;

    /// Producer: called when output ports need samples.
    /// Consumer: called when input ports have samples.
    /// Process returns how many input/output samples were consumed/produced.
    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work>;

    /// Clean release. Must be idempotent.
    fn stop(&mut self) -> Result<()>;
}
```

`BlockIo` gives the block ergonomic access to input buffers (slices of the
declared port types) and output buffers (pre-allocated, the block fills and
reports how much it wrote). `Work` tells the scheduler how many items the
block consumed and produced so it can advance buffer cursors.

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

Blocks register with the runtime via an inventory pattern:

```rust
#[ferrite_block(type_name = "FmDemod")]
impl Block for FmDemod { /* ... */ }
```

The `#[ferrite_block]` attribute:

- Emits a static descriptor (`BlockSpec`) into a registry.
- Exports a WASM-visible constructor (`#[wasm_bindgen]`-generated glue for the
  browser build; plain extern `"C"` for native).
- Checks at compile time that the block's ports + params match the JSON schema
  we ship.

Browser and Node runtimes call `registry.instantiate("FmDemod", params)` to
produce an instance. Name collisions are a compile-time error.

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

The flowgraph runtime runs inside a Worker. Inside that Worker:

- Each block gets a small amount of input-buffer and output-buffer space
  (typically a few thousand samples).
- The scheduler walks the DAG in topological order, calling `process()` on
  each block with the samples currently available to its inputs.
- A block is **ready** when all its inputs have ≥1 item and all its outputs
  have ≥1 slot. Non-ready blocks are skipped this tick.
- When no block is ready, the scheduler parks on the input source's "new
  samples" signal (a `MessageChannel` post from the WS worker for browser
  runtime; a `fs.read` or loopback WS read for Node runtime).

One flowgraph = one Worker. Multiple flowgraphs (e.g. a second VFO with its
own demod chain) run in separate Workers. The main thread never blocks.

Realtime is handled at the sink side: the AudioWorklet's consumer ring buffer
provides backpressure; when the sink falls behind, the block upstream of it
stops being "ready" and naturally pauses. No allocations occur inside
`process()`.

## Rust → WASM build

One crate, two targets. `blocks/Cargo.toml`:

```toml
[package]
name = "ferrite-blocks"

[features]
default = []
wasm = ["wasm-bindgen"]

[lib]
crate-type = ["rlib", "cdylib"]

[dependencies]
rustfft = "..."
num-complex = "..."
anyhow = "..."
wasm-bindgen = { version = "...", optional = true }
```

Native link into `ferrited`:

```
cargo build -p ferrite-blocks
```

Browser/Node WASM:

```
wasm-pack build blocks --target web --features wasm
```

Identical source, same `cargo test` (the test binary runs natively; WASM
parity is verified separately — see `05-testing.md`).

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

Post-M5, the registered blocks are:

| block           | ports                              | purpose                                        | status  |
|-----------------|------------------------------------|------------------------------------------------|---------|
| `Source`        | — → iq_f32                         | placeholder resolved to a real source via `compose_source` | shipped |
| `SoapySource`   | — → iq_f32                         | RTL-SDR / SDRPlay / any Soapy device           | shipped |
| `FileIqSource`  | — → iq_f32                         | reads IQ from a local file                     | shipped |
| `SineSource`    | — → iq_f32                         | synthetic tone — test fixture                  | shipped |
| `Channelizer`   | iq_f32 → iq_f32                    | frequency shift + FIR + decimate, one VFO      | shipped |
| `Decimator`     | iq_f32 → iq_f32                    | FIR + decimate                                 | shipped |
| `TeeIqF32`      | iq_f32 → 2 × iq_f32                | 1→2 IQ fan-out                                 | shipped |
| `FFT`           | iq_f32 → iq_f32 (bins)             | windowed DFT, with input accumulation          | shipped |
| `LogMagU8`      | iq_f32 (bins) → bytes              | log-magnitude scale → `u8` bins for waterfall  | shipped |
| `FmDemod`       | iq_f32 → real_f32                  | WBFM listening                                 | shipped |
| `AmDemod`       | iq_f32 → real_f32                  | AM listening                                   | shipped |
| `AudioSink`     | real_f32 → —                       | feeds AudioWorklet ring (SAB) in the browser   | shipped |
| `WsBridgeTx`    | * → —                              | auto-inserted by `env_split` on crossings      | shipped |
| `WsBridgeRx`    | — → iq_f32                         | auto-inserted by `env_split`; also subscribes to a `stream_id` on `/ws/preset` when authored directly | shipped |

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
