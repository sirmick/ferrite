# Vendored upstream — liquid-dsp

- **Upstream:** https://github.com/jgaeddert/liquid-dsp
- **Pinned commit:** `9aca82a883145cdf092bdff838804a552900c0d1`
- **Pulled:** 2026-04-23
- **License:** MIT (see `vendor/LICENSE` once we copy it; the project's
  README also embeds the same MIT terms).

## What we vendored

- `vendor/include/` — every public header (`liquid.h`, `liquid.internal.h`,
  `liquid.argparse.h`, `liquid.autotest.h`).
- `vendor/src/` — the per-module C source tree, **minus**:
  - `*.test.c` — upstream's autotest harness
  - `*.benchmark.c` — performance benchmarks
  - `*.av.c`, `*.avx.c`, `*.avx2.c`, `*.avx512f.c`, `*.sse.c`,
    `*.sse4.c`, `*.neon.c` — SIMD-accelerated dotprod/sumsq/vector
    variants. The portable C versions remain. We may opt SIMD back in
    later for the native build, but a single source set across native
    and WASM keeps the substrate simple.

We did NOT vendor `examples/`, `sandbox/`, `bench/`, `autotest/`,
`scripts/`, `doc/`, `gentab/`, or any of the upstream build-system
files (CMakeLists.txt, makefile.in, configure.ac). Our `build.rs` is
authoritative and lists the per-module source files itself, mirroring
the CMake `add_library(... OBJECT ...)` declarations.

## Bumping

When upstream releases a new version we want:

1. `cd research/liquid-dsp && git pull && git rev-parse HEAD` — note new commit.
2. `cp -r research/liquid-dsp/{include,src} blocks/native/liquid-dsp/vendor/`
3. Re-run the `find vendor/src ... -delete` from the original vendoring
   to drop test/SIMD again.
4. Update `pinned commit` and `pulled` above.
5. Re-run `cargo test --package ferrite-liquid-dsp` and the parity
   tests for any block that wraps liquid primitives.

## Why we vendor instead of depending on the system `liquid` package

- `wasm32-unknown-unknown` cross-compile — Ubuntu's `libliquid-dev` is
  built for the host triple only.
- Reproducible builds — pinned commit, same source on every CI run.
- Safety — upstream bumps are deliberate, not silent over a `make
  upgrade` on a CI host.

## What's NOT vendored

- The crates.io `liquid-dsp-sys` (or the various `liquid-dsp-bindings`
  crates) all assume a system-installed liquid at
  `/usr/include/liquid/liquid.h` and have no WASM cross-compile path.
  We don't depend on them; our `build.rs` + bindgen produces our own
  bindings against the vendored headers.
