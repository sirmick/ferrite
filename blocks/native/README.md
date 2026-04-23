# `blocks/native/` — C-vendor substrate

Build pipeline for compiling C source (vendored from upstream
projects like `liquid-dsp`, `multimon-ng`, `dump1090`) into Ferrite
blocks that run on **both** native (`ferrited`) and the browser WASM
runtime.

## Why this exists

`ferrite-blocks` is dual-compile pure Rust: same source tree builds
into both the native `ferrited` binary and a browser WASM module via
`wasm-pack`. Vendored C decoders need the same property — the cost
of writing Phase 2's substrate once is paid back across multimon-ng,
liquid-dsp, dump1090, rtl_433, ft8_lib, codec2, and every future
C-vendor port.

See `docs/decoder-roadmap/02-phase-2-multimon-vendor.md` for the
strategic context, and `docs/decoder-roadmap/90-vendor-port-guide.md`
for the per-vendor recipe (which is being written by doing).

## Layout

```
blocks/native/
  README.md           # this file
  libc-stubs/
    include/          # minimal stdio.h / time.h to interpose ahead of
    stubs.c           # wasi-libc on the wasm32-unknown-unknown path,
                      # plus link-time no-op implementations
  hello/              # M1 substrate proof — smallest possible C-as-WASM
                      # vendor block. Verifies the build.rs pattern,
                      # libm linkage, and FFI calling convention end
                      # to end. Don't ship; it's a smoke test.
  <vendor>/           # one crate per upstream project, e.g. liquid-dsp,
                      # multimon, ft8_lib. Each follows the build.rs
                      # template and links libc-stubs.
```

## Build pattern

Each vendor crate's `build.rs`:

1. **Detect target**: `std::env::var("TARGET")`.
2. **Set compiler**:
   - native (`x86_64-linux-gnu`, `aarch64-apple-darwin`, …): system
     `cc` (gcc / clang) — cc-rs picks the right one.
   - `wasm32-unknown-unknown`: `clang` with `--target=wasm32-unknown-unknown`,
     `-nostdlibinc`, `-fno-builtin`.
3. **Add include paths** (wasm32 only):
   - `blocks/native/libc-stubs/include` — interposes our stdio/time
     stubs ahead of wasi-libc so vendored C compiles even though
     wasi-libc gates `<wasi/api.h>` on the `wasm32-wasi` ABI.
   - `/usr/include/wasm32-wasi` — wasi-libc headers (libm in
     particular). Ubuntu installs this via the `wasi-libc` apt
     package.
4. **Compile vendored sources** + `wrap.c` + the ABI shim.
5. **Link libm + libc-stubs** (wasm32 only):
   ```
   cargo:rustc-link-search=native=/usr/lib/wasm32-wasi
   cargo:rustc-link-lib=static=m
   ```

The `hello/` crate is the canonical example — copy + adapt for new
vendors.

## Toolchain

Required on the build host:

- **Native**: `gcc` or `clang` (anything cc-rs can find).
- **WASM**: `clang` (with `wasm32-unknown-unknown` target support — any
  modern clang has this), `wasm-ld` (ships with the `lld` package on
  Debian/Ubuntu), `wasi-libc` (the `wasi-libc` apt package — `dpkg -L
  wasi-libc` confirms headers under `/usr/include/wasm32-wasi/` and
  static libs under `/usr/lib/wasm32-wasi/`).

Install on Ubuntu 24.04:

```
sudo apt install clang lld wasi-libc
```

## What's missing (and intentionally so)

- **No vendored upstream sources committed yet.** `hello/` is pure
  Ferrite-authored stub C. Real vendors land per-milestone:
  liquid-dsp first (M2), multimon-ng later (Phase 2 proper).
- **No bindgen wired up.** Will land alongside the first vendor that
  needs it (bindgen on liquid.h is the M2 task).
- **No `ferrite_port.h` shim.** The `libc-stubs/` redirect handles the
  stdio surface; `ferrite_port.h` will appear when we have a vendor
  that emits log output we want to capture (multimon-ng).
- **No `golden_runner.rs` test harness.** Will appear when we have
  parity tests across native ↔ WASM (multimon-ng decoder fixtures).

## Validating the substrate

```
cargo test --package ferrite-hello-vendor      # native compile + FFI
cargo build --target wasm32-unknown-unknown \
    --package ferrite-hello-vendor             # WASM compile + link
```

Both should pass without warnings. `hello/`'s tests cover:

- `add()` — 32-bit int FFI calling convention
- `sumf()` — pointer + length slice convention
- `rmsf()` — libm linkage (calls `sqrtf`)

If any of these break, the build pipeline regressed.
