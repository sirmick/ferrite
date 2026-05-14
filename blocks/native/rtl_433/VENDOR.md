# Vendored rtl_433

Source of `vendor/` in this crate.

| field        | value |
|--------------|-------|
| upstream     | https://github.com/merbanan/rtl_433 |
| pinned at    | `d855d1597ad4bc3af6abe355c9689a0ce71d2b62` (2026-04-14) |
| license      | GPL-2.0-or-later (compatible with this codebase's GPL-3.0-or-later) |
| trimmed dirs | `.git/`, `tests/`, `examples/`, `docs/`, `man/`, `cmake/`, `conf/`, `debian/`, `getopt/` |

## What we don't compile

The lift mirrors the multimon-ng / dump1090 / rtl-ais pattern: keep the
DSP + decoder library, drop the I/O shell. Specifically removed from
`vendor/src/` (after `cp`'ing upstream `src/`):

- `rtl_433.c` — upstream's `main()` + CLI option parser. We supply a
  thin replacement in `shim/` that wires the C side into the Rust
  `Block` interface.
- `sdr.c` — librtlsdr / SoapySDR device I/O. Ferrite's `Source` block
  feeds samples to the Rtl433Demod block; the decoder never touches
  hardware directly.
- `mongoose.c`, `http_server.c` — embedded HTTP server. Not relevant.
- `output_mqtt.c`, `output_influx.c`, `output_rtltcp.c`, `output_udp.c`,
  `output_trigger.c`, `output_file.c` — upstream output sinks. Ferrite
  emits events via its own `events` port + `decoder::rtl_433` tracing
  target; we hook `output_fn` in the shim.
- `term_ctl.c` — VT100 colour codes for the upstream CLI. Not relevant.

What stays compiled but unused at runtime (Ferrite never invokes them;
the linker drops most of the object code as dead):
`am_analyze.c`, `samp_grab.c`, `pulse_analyzer.c`, `write_sigrok.c`,
`confparse.c`, `fileformat.c` — referenced from `r_api.c` /
`r_private.h` includes but the corresponding state fields stay NULL.

## Build targets

Native (`x86_64-unknown-linux-gnu`, etc.) and `wasm32-unknown-unknown`,
following the multimon-ng / dump1090 / ft8 dual-compile pattern:

- Native: straight `cc::Build`, links against system libm.
- WASM: `clang --target=wasm32-unknown-unknown -nostdlibinc`, includes
  `../libc-stubs/include` first (provides the minimal `<stdio.h>`,
  `<time.h>`, etc. stubs) then `/usr/include/wasm32-wasi` for `<math.h>`
  and friends. Links static `libc.a` + `libm.a` from wasi-libc; the
  linker drops every symbol the trimmed sources don't reach.

The expectation is that any vendor file we keep compiles cleanly under
both targets after the trim — anything that doesn't (e.g. a new upstream
file that pulls in pthread or sockets) gets added to the trim list when
resyncing.

## Resyncing upstream

When bumping to a new upstream commit:

1. `git -C ../../../research/rtl_433 fetch && git -C ../../../research/rtl_433 checkout <commit>`
2. `cp ../../../research/rtl_433/src/*.c vendor/src/`
3. `cp ../../../research/rtl_433/src/devices/*.c vendor/src/devices/`
4. `cp ../../../research/rtl_433/include/*.h vendor/include/`
5. Re-run the trim list above (the `rm -f` set in `vendor/src/`).
6. Update `pinned at` row above with the new SHA + date.
7. `cargo build -p ferrite-rtl-433` and chase any new compile errors —
   upstream sometimes adds source files; the trim list may need
   extending if they introduce more OS-touching paths.
