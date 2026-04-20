# Vendor port guide

Concrete recipe for vendoring a C decoder project as a Ferrite block.
Written from the multimon-ng experience in Phase 2; applies to every
subsequent vendor (direwolf, dump1090, rtl_433, ft8_lib, codec2, etc.).

This doc lives alongside the roadmap because it's most useful *while
doing* a port — referenced from Phases 2+.

## The short version

Yes, it's mostly "compile existing C as WASM, replace I/O, strip options
handling". The details below are what turns "it links" into "it decodes
correctly on the first fixture, and stays that way across upstream bumps".

## Pre-port: the feasibility pass

Before touching code, do what we did in `research/` for the initial
eight:

1. Clone upstream into `/research/<name>/`.
2. Run the three questions from `WASM_PORT_ASSESSMENT.md`:
   - Where is the DSP core vs the I/O shell? (File-level split.)
   - What does the DSP core actually need from libc/libm/pthreads/deps?
   - What's the output path and can it be redirected cleanly?
3. If the answers aren't all green, decide: port-anyway, port-subset,
   clean-Rust re-implement, or skip.

This phase is free compared to discovering the blocker three weeks into
a port.

## The port, step by step

### Step 1 — vendor the source

```
blocks/native/<name>/
  VENDOR.md         # upstream URL, commit hash, date pinned
  vendor/           # upstream files, unmodified
  wrap.c, wrap.h    # the C ABI
  build.rs          # compiler switching
  Cargo.toml
  src/
    lib.rs          # Rust block implementations
    bindings.rs     # bindgen-generated from wrap.h (in OUT_DIR, actually)
  fixtures/         # recorded inputs + expected outputs
```

Copy the specific files listed in the feasibility pass, nothing more.
Leave everything else behind. If upstream has 200 files and you need 15,
you copy 15.

Don't fork the files. Don't reformat. Pin the commit hash in
`VENDOR.md` so the exact provenance is recoverable.

### Step 2 — wire up `ferrite_port.h`

The shim header at `blocks/native/ferrite_port.h` is the **only**
interface the upstream code sees. It defines:

- `verbprintf` (or whatever the upstream uses to log) → `ferrite_emit(...)`.
- Stubs for `fprintf`, `stderr`, `getenv`, `time`, `rand` if upstream
  touches them in the DSP path.
- The `extern void *g_ferrite_ctx` thread-local (single-threaded per
  block, so just a static is fine) that `ferrite_emit` uses to find
  the current block's event ring.

Inject via `-include ferrite_port.h` in `build.rs`. Zero edits to
upstream source. If upstream has macros that conflict, that's a signal
you've picked the wrong file — re-check the feasibility pass.

### Step 3 — write `wrap.c`

This is the ABI surface. It's the only new C we write. Pattern:

```c
// wrap.h
typedef struct foo_ctx foo_ctx_t;

foo_ctx_t *foo_new(const foo_params_t *params);
void       foo_feed(foo_ctx_t *ctx, const float *samples, int n);
int        foo_pop_event(foo_ctx_t *ctx, char *buf, int cap);  // returns bytes written, 0 if empty
void       foo_free(foo_ctx_t *ctx);
```

In `wrap.c`:

- `foo_ctx_t` holds: a copy of `foo_params_t`, any upstream state
  structs the decoder needs, an event ring buffer (bounded, static-size
  — no `malloc` in the hot path), and the "this is the current
  context" thread-local.
- `foo_new` initialises upstream state (calls upstream's `*_init`
  functions), seeds PRNGs deterministically from params (see gotcha
  list), sets `g_ferrite_ctx`, mallocs the context once.
- `foo_feed` sets `g_ferrite_ctx = ctx`, calls upstream's
  sample-consuming entry point(s). Events land in the ring via
  `ferrite_emit`.
- `foo_pop_event` drains one event from the ring.
- `foo_free` calls upstream's cleanup, frees the context.

Aim for under 300 LOC of `wrap.c` per decoder.

### Step 4 — `build.rs`

```rust
// blocks/native/foo/build.rs
fn main() {
    let target = std::env::var("TARGET").unwrap();
    let mut cc = cc::Build::new();
    cc.include("vendor")
      .include("..")  // for ferrite_port.h
      .flag("-include").flag("ferrite_port.h")
      .files(["vendor/thing.c", "vendor/otherthing.c", "wrap.c"])
      .warnings(false);   // upstream warnings are upstream's problem

    if target.starts_with("wasm32") {
        cc.compiler("clang")
          .flag("--target=wasm32-unknown-unknown")
          .flag("-nostdlib")
          .flag("-fno-builtin")
          .include("../libc/wasi-libc/sysroot/include");
        // Link wasi-libc + libm
        let wasi_lib = "../libc/wasi-libc/sysroot/lib/wasm32-wasi";
        println!("cargo:rustc-link-search=native={}", wasi_lib);
        println!("cargo:rustc-link-lib=static=c");
        println!("cargo:rustc-link-lib=static=m");
    }

    cc.compile("foo_vendor");

    bindgen::Builder::default()
        .header("wrap.h")
        .generate().unwrap()
        .write_to_file(std::env::var_os("OUT_DIR").unwrap().into())
        .unwrap();
}
```

### Step 5 — Rust `Block` impl

In `src/lib.rs`, implement Ferrite's `Block` trait. Shape:

```rust
use ferrite_blocks::{Block, BlockIo, Work, InitCtx};

#[ferrite_block(type_name = "PocsagDecoder")]
pub struct PocsagDecoder {
    ctx: *mut ffi::pocsag_ctx_t,
    baud: u32,
}

impl Block for PocsagDecoder {
    fn spec() -> BlockSpec { /* audio in, events out, params: baud */ }

    fn init(&mut self, ctx: &mut InitCtx) -> Result<()> {
        let rate = ctx.negotiated_rate("in")?;
        assert_eq!(rate, 22050.0, "multimon-ng wants 22050 Hz");
        unsafe { self.ctx = ffi::pocsag_new(self.baud as _); }
        Ok(())
    }

    fn process(&mut self, io: &mut BlockIo<'_>) -> Result<Work> {
        let audio: &[f32] = io.input("in")?.slice();
        unsafe { ffi::pocsag_feed(self.ctx, audio.as_ptr(), audio.len() as i32); }

        let mut evbuf = [0u8; 4096];
        loop {
            let n = unsafe { ffi::pocsag_pop_event(self.ctx, evbuf.as_mut_ptr() as _, 4096) };
            if n == 0 { break; }
            io.output("events")?.push_bytes(&evbuf[..n as usize]);
        }
        Ok(Work::consumed_all())
    }

    fn stop(&mut self) -> Result<()> {
        unsafe { if !self.ctx.is_null() { ffi::pocsag_free(self.ctx); self.ctx = std::ptr::null_mut(); } }
        Ok(())
    }
}
```

The block trait does all the Ferrite-facing work; the `ffi::*` calls
are generated by bindgen from `wrap.h`.

### Step 6 — golden fixtures

- Pull 2–5 short recordings from upstream's test assets (most
  decoders have them) into `fixtures/`.
- Write a parity test using `blocks/native/harness/golden_runner.rs`:
  feed fixture → native build and WASM build, assert identical event
  streams.
- Commit fixtures alongside the code. They're small (seconds of audio;
  KB-scale).

### Step 7 — flowgraph preset

Write the `flowgraphs/presets/<foo>.json` that wires the decoder into
a usable chain. This is what the UI's receiver picker will load.

## The nine common gotchas

A running list — add to this file whenever a port reveals a new one.

1. **`-nostdlib` doesn't mean "no libc".** It means "no JS glue (not
   Emscripten)". Link **wasi-libc**. It provides memcpy/sinf/sqrtf/etc
   cleanly.
2. **Thread-local globals (`__thread`, `pthread_key_t`)**. Replace with
   plain statics. Single-threaded per block anyway.
3. **`time(NULL)` in init.** Decoders that seed PRNGs from wall clock
   will be nondeterministic across runs, which makes fixture tests
   flaky. Stub `time` or seed from a params field.
4. **`rand()` / `random()`.** Same deal. Stub to a deterministic PRNG,
   seed from params.
5. **`getenv`.** Upstream sometimes reads config from env vars. Return
   `NULL` from a stub; supply config via params instead.
6. **Output buffering.** Upstream often writes line-buffered via
   `fprintf(stdout, ...)`. Our `verbprintf`-style redirect has to
   handle partial-line accumulation; most decoders emit full events
   per call but not all.
7. **Sample rate assumptions baked in as constants.** Upstream
   sometimes hardcodes `SAMPLE_RATE 22050` everywhere. If the runtime's
   negotiated rate doesn't match, either put a `Resample` block in the
   chain (preferred) or parameterise upstream (invasive, avoid).
8. **Endianness and alignment assumptions.** WASM is little-endian and
   aligned — same as every modern target. No issues unless upstream
   has unusual byte-manipulation for big-endian networks.
9. **Upstream's own `malloc` calls.** Fine if wasi-libc provides it.
   But count them — a decoder that mallocs per sample is going to be
   slow. If upstream allocates in hot path, consider a per-block
   bump-allocator or pre-allocated pool in `foo_new`.

## Debugging the first decode

When the parity test fails (and the first vendor's will, the first
time), the flowchart:

1. **Does native compile + native fixture pass?** If not, the vendor
   set is wrong — missing files, or the shim header stubs too much.
2. **Does WASM compile?** If not, missing libc symbols (expected;
   stub or link) or missing intrinsics (expected; clang emits).
3. **Does WASM run and produce any events?** If not, look at
   `g_ferrite_ctx` — if NULL, the ring buffer isn't being wired. If
   not NULL but no events, upstream's demod isn't being called (check
   `foo_feed` dispatch).
4. **Does WASM produce some but wrong events?** 90% of the time:
   `time`/`rand` nondeterminism, or an uninitialized static the
   native path happens to zero. Grep for `static` without initialiser,
   `time(`, `rand(`.
5. **Exactly wrong events?** Upstream has a version-gated codepath
   (tested against a different build flag than ours). Diff the
   compile-time `-D...` flags; match the native build's expected set.

## Maintaining vendors

- **Upstream bumps.** Pin a commit in VENDOR.md. Bumping is a
  deliberate commit: new hash, re-run fixtures, note breaking changes
  in the commit message.
- **Upstream abandonware.** If upstream stops getting updated (fldigi,
  dsd-fme forks, etc.), we keep the vendor. Mark `UPSTREAM_STALE` in
  VENDOR.md so future us knows.
- **Security patches in upstream.** Apply them to our vendor; we
  don't auto-track. Most DSP decoders don't face a scary CVE threat
  model, but it's worth checking the upstream CHANGELOG yearly.

## When to skip vendor and write Rust instead

- Upstream is C++ with heavy STL / RTTI / exception use and the DSP
  core is modest (≤1k LOC). Clean-Rust wins.
- Upstream is Fortran (wsjt-x). Use a pure-C sibling project
  (ft8_lib) or write Rust.
- Upstream's DSP core is well-documented textbook DSP and an existing
  Rust crate gets us 80% there (e.g. `noaa-apt`).
- The vendor's test suite is so sparse that we'd have to build fixtures
  ourselves anyway, removing the "reuse their tests" advantage.

Rough inflection: under ~500 LOC of pure DSP, Rust usually wins.
Over ~2000 LOC, vendor usually wins. Between, case-by-case.
