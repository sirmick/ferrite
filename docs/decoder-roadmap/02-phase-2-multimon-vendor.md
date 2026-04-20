# Phase 2 — First C-vendor wave via multimon-ng

**Status:** not started.
**Entry criterion:** Phase 1 shipped — analog listening works across all
six presets; the Rust helper blocks (`Deemphasis`, `Squelch`, `Agc`,
`Resample`) are solid.
**Exit criterion:** Ferrite has five data decoders running end-to-end from
real RF: POCSAG (512/1200/2400), FLEX, DTMF, EAS (SAME), CTCSS. All
vendored once from `multimon-ng` via the new `blocks/native/`
infrastructure. `blocks/native/` is the reusable substrate for every
subsequent C-vendor port.

## Why multimon-ng is the right first vendor

Per `research/WASM_PORT_ASSESSMENT.md`, multimon-ng is the single cleanest
C codebase in the SDR ecosystem: uniform `demod_param` vtable, zero
globals, zero threads, all I/O in one file we don't include. Vendoring
multimon-ng is **simultaneously** the lowest-risk way to prove the
`blocks/native/` pattern *and* the highest-ROI decoder ship (five
user-visible decoders from one lift).

Doing a harder vendor first (direwolf's global state, dump1090's output
coupling) means debugging two things at once — our tooling and the port.
multimon-ng lets us debug only the tooling.

## Deliverables — three threads

This phase has three parallel threads that converge:

1. **`blocks/native/` infrastructure** — the build system, shim header,
   libc substrate, and test harness that every future C vendor will reuse.
2. **`liquid-dsp-sys`** — optional but strongly recommended substrate
   lift; turns a lot of "re-implement in Rust" later work into
   "call through the wrapper".
3. **The multimon-ng vendor itself** — five decoder blocks.

## Thread 1 — `blocks/native/` infrastructure

Directory layout:

```
blocks/native/
  README.md                 # the contract; references 90-vendor-port-guide.md
  ferrite_port.h            # shared shim header; every vendored .c includes it
  libc/
    wasi-libc/              # pinned wasi-libc checkout (git submodule or fetched in build.rs)
    libm-stubs.c            # any libm gaps that wasi-libc doesn't cover
  harness/
    golden_runner.rs        # Rust test helper: run a fixture through native+WASM, diff
```

### The build pattern

Each vendored block is its own Cargo crate under `blocks/native/<name>/`.
`build.rs` chooses its compiler based on target:

```rust
// blocks/native/multimon/build.rs (sketch)
fn main() {
    let target = std::env::var("TARGET").unwrap();
    let mut build = cc::Build::new();
    build
        .include("vendor")
        .include("../ferrite_port.h")
        .files(["vendor/demod_poc12.c", "vendor/pocsag.c", "vendor/bch.c", ...]);
    if target.starts_with("wasm32") {
        build
            .compiler("clang")
            .flag("--target=wasm32-unknown-unknown")
            .flag("-nostdlib")
            .flag("-fno-builtin")
            .include("../libc/wasi-libc/sysroot/include");
        println!("cargo:rustc-link-arg=-L{}", /*wasi-libc lib path*/);
        println!("cargo:rustc-link-lib=static=c");  // wasi-libc
        println!("cargo:rustc-link-lib=static=m");
    }
    build.compile("multimon_vendor");
    // bindgen for the wrap.h ABI
    bindgen::Builder::default()
        .header("wrap.h")
        .generate().unwrap()
        .write_to_file("src/bindings.rs").unwrap();
}
```

Details are in `90-vendor-port-guide.md`; Phase 2 is where that guide
gets *written by doing*.

### The shim header contract

`blocks/native/ferrite_port.h` is the only file the vendored upstream
sources "know about". All redirects live here:

```c
// blocks/native/ferrite_port.h
#ifndef FERRITE_PORT_H
#define FERRITE_PORT_H

// Output redirection — upstream calls these, we route to our event ring buffer.
void ferrite_emit(void *ctx, int lvl, const char *fmt, ...);
#define verbprintf(lvl, ...)  ferrite_emit(g_ferrite_ctx, lvl, __VA_ARGS__)

// Stubs for things upstream assumes but we don't want.
#define fprintf(f, ...)       ((void)0)
// time(), rand(): seeded deterministically in wrap.c
// pthreads: upstream that calls these shouldn't be in our vendor set;
// if it is, stub here and reconsider the cut.

extern void *g_ferrite_ctx;  // current block's event ring buffer

#endif
```

Upstream files include this header as a forced preamble (via `-include`
compiler flag — zero source edits required).

### The test harness

`blocks/native/harness/golden_runner.rs` is a Rust test utility:

```rust
pub fn parity_test(
    native_block: impl Block,
    wasm_block: impl Block,     // loaded via wasm-bindgen-test
    fixture_audio: &Path,
    expected_events: &[Event],
) { /* feed both, assert identical output */ }
```

Used by every C-vendor block's test suite. `multimon-ng` ships its own
reference recordings in `test/samples/`; check a few of those into
`blocks/native/multimon/fixtures/` and run them through this harness.

## Thread 2 — `liquid-dsp-sys` substrate

Separate crate under `blocks/native/liquid-dsp/`. Lift once, use
everywhere subsequently. BSD-licensed, pure C, already known to compile
to WASM.

- Use the build pattern from Thread 1.
- Expose a Rust-idiomatic wrapper for the primitives Ferrite actually
  uses: `resamp_rrrf` (resampler), `firpfb_rrrf` (polyphase filter
  bank), `nco_crcf` (NCO), `firfilt_rrrf` / `firfilt_crcf`, `agc_rrrf`,
  `dds_cccf` (digital synthesiser for channelizer / BFO), the FEC
  codecs for later phases (convolutional, RS, LDPC-lite).
- Replace the Phase 1 hand-rolled `Resample` with a liquid-dsp-backed
  version *as a rewrite commit*, not a live migration — preserve the
  hand-rolled one in git history as a reference implementation; it has
  tests.

**Why this goes in Phase 2 and not Phase 1:** Phase 1 delivers user-
visible listening; adding a 50k-LOC C dep in parallel risks destabilising
the demos. Phase 2 already has the `blocks/native/` pattern; liquid-dsp
is the second use of that pattern (after multimon-ng), which makes it
the correct-size exercise.

**If it slips:** Phase 2 still ships. liquid-dsp is a force multiplier
for Phases 3+; absence doesn't block Phase 2's multimon-ng work (it
doesn't use liquid-dsp). Can defer to Phase 2.5 or Phase 3 if needed.

## Thread 3 — multimon-ng vendor

### Vendored file set

From `research/multimon-ng/`, vendor into `blocks/native/multimon/vendor/`:

- `multimon.h` — demod_param vtable + demod_state union
- `demod_poc5.c`, `demod_poc12.c`, `demod_poc24.c` — POCSAG 512/1200/2400
  L1 demodulators
- `pocsag.c` — POCSAG L2 framing, codeword recovery, text formatting
- `bch.c` — BCH(31,21) error correction used by POCSAG
- `demod_flex.c`, `flex.c` (if separate) — FLEX pager
- `demod_dtmf.c` — DTMF Goertzel detector
- `demod_eas.c` — SAME/EAS emergency alert
- `demod_ctcss.c` — CTCSS subaudible tone detector
- `costabi.c`, `costabf.c` — sin/cos lookup tables (pre-generated data)

Do NOT include: `unixinput.c`, `cJSON.c` (use Rust `serde_json` on the
wrap layer instead), X11/SDL scope files, `demod_display.c`.

Pin the commit hash in `blocks/native/multimon/VENDOR.md`. No
reformatting; no file edits beyond whatever `-include ferrite_port.h`
achieves.

### Block list — five Ferrite blocks from one vendor

Each is its own `#[ferrite_block(type_name = "…")]` impl in
`blocks/native/multimon/src/`. They share one `foo-sys` binding layer
but present distinct block types to the registry.

| Block type       | Port in   | Port out | Params                               |
|------------------|-----------|----------|--------------------------------------|
| `PocsagDecoder`  | real_f32 (22050 Hz) | events   | `baud` (enum: 512/1200/2400)   |
| `FlexDecoder`    | real_f32 (22050 Hz) | events   | —                              |
| `DtmfDecoder`    | real_f32 (22050 Hz) | events   | `hold_ms`                      |
| `EasDecoder`     | real_f32 (22050 Hz) | events   | `locale` (enum: US/CA/…)       |
| `CtcssDecoder`   | real_f32 (48000 Hz) | events   | `tones_hz` (array, optional)   |

`CtcssDecoder` wants 48 kHz because it works on sub-audible tones
(67–254 Hz); 22050 Hz is overkill but fine. Pick one rate or let the
block advertise both and have the runtime pick via metadata.

### ABI shape (wrap.c)

One ABI per decoder, not one umbrella ABI — simpler; each block pays
only for its own state:

```c
typedef struct pocsag_ctx pocsag_ctx_t;
pocsag_ctx_t *pocsag_new(int baud);
void          pocsag_feed(pocsag_ctx_t *, const float *samples, int n);
int           pocsag_pop_event(pocsag_ctx_t *, char *buf, int cap);
void          pocsag_free(pocsag_ctx_t *);
```

Events are serialised as JSON strings internally (use multimon-ng's
existing JSON output code path — `json_mode = 1`). The Rust wrapper
parses them into typed `Event` structs for the Ferrite `events` port.

## Preset flowgraphs to ship

Under `flowgraphs/presets/`:

- `pocsag-1200.json` — source → channelizer(12.5 kHz @ any freq) →
  FmDemod → Resample(22050) → PocsagDecoder → EventsSink → WsBridge →
  UI event log.
- `flex.json`, `dtmf.json`, `eas.json`, `ctcss.json` — similar.
- `combo-pager-monitor.json` — source → channelizer(12.5 kHz @ 929 MHz
  typical US paging freq) → FmDemod → fan-out to (PocsagDecoder,
  FlexDecoder) in parallel. Demonstrates multi-decoder-on-one-slice.

## Receivers pane + event UI

- Add a **decoder pane** alongside the existing receivers pane. Selecting
  a decoder preset loads its flowgraph. The pane shows a live table of
  decoded events (timestamp, source, content) with filtering.
- Events are typed: `PocsagEvent { capcode, function, message }`,
  `DtmfEvent { digit, duration_ms }`, etc. The Rust block emits typed
  JSON; the TS UI has a render per event type.

## Testing

- **Golden-fixture parity** (native ↔ WASM) for each of the five blocks
  using the multimon-ng test recordings.
- **Integration test per preset** using the flowgraph-runner plus those
  fixtures.
- **Regression test**: take a ~30-minute off-air recording of a pager
  channel (the user almost certainly has an RTL-SDR dump available),
  decode it, snapshot the resulting event list; re-run on every change
  to the vendor. Catches subtle regressions from liquid-dsp swap-ins or
  shim-header changes.

## Commit-level plan

Roughly:

1. `feat(blocks/native): scaffolding — ferrite_port.h, wasi-libc setup,
   harness crate`
2. `feat(blocks/native): liquid-dsp-sys — BSD DSP substrate`  (can slip to later)
3. `chore: vendor multimon-ng @ <commit> into blocks/native/multimon/vendor`
4. `feat(blocks/native/multimon): wrap.c + bindings — minimal POCSAG ABI`
5. `feat(blocks): PocsagDecoder — Block impl, events-port output`
6. `feat(flowgraphs): pocsag-1200 preset`
7. `feat(web): events pane; POCSAG event renderer`
8. `feat(blocks): FlexDecoder — second decoder, same vendor`
9. `feat(blocks): DtmfDecoder, EasDecoder, CtcssDecoder`
10. `test(blocks/native/multimon): golden-fixture parity tests`
11. `feat(flowgraphs): flex/dtmf/eas/ctcss presets + combo-pager-monitor`
12. `docs: 90-vendor-port-guide.md finalised from actual experience`

## Risks and dependencies

- **wasi-libc linking quirks.** First time through, expect a day of
  missing-symbol whack-a-mole. Keep a running list in
  `blocks/native/libc/NOTES.md` for the next vendor's benefit.
- **JSON event schema drift.** Don't let upstream's JSON output format
  become the API shape we expose to the browser. Parse it in Rust, emit
  a Ferrite-native event schema. Otherwise we'll regret it when
  multimon-ng updates its format.
- **FLEX is the big one.** ~1.5k LOC with its own stateful allocator
  (`struct Flex *`). If FLEX misbehaves, ship POCSAG/DTMF/EAS/CTCSS
  first (four decoders) and add FLEX in a follow-up commit.
- **liquid-dsp slipping.** Hedged already — Phase 2 doesn't depend on
  it. If it's ready, great; if not, defer.
- **Browser WASM bundle size.** Five decoders + wasi-libc + liquid-dsp
  could be 1–2 MB in one blob. If it hurts cold-start, split into
  per-block WASM modules loaded on demand (wasm-pack-per-block).
  Measure first, optimize later.

## What this phase deliberately doesn't do

- No direwolf, dump1090, rtl_433. Those are Phase 3 — they use the
  infrastructure this phase builds.
- No decoder-specific UI cleverness beyond the events table. That's a
  UX phase, not a decoder phase.
- No RDS, no SSTV, no navtex. All audio-domain but different enough
  from multimon-ng's mode list that they fit better with fldigi-style
  ham digital work in Phase 5.
- No automatic decoder hinting ("you're on 929 MHz — try POCSAG?"). Cute
  feature, belongs in a polish phase after the decoders themselves are
  solid.
