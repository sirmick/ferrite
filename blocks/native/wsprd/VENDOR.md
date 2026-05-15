# Vendored wsprd (WSPR decoder)

Source: <https://github.com/Guenael/rtlsdr-wsprd> (the `wsprd/`
decode-core subtree — a clean library extraction of WSJT-X's WSPR
detector/demodulator/decoder by K1JT / K9AN).
License: GPL-3.0 (see `vendor/LICENSE`).
Vendored at: commit `1ca9b83dd2562ce9ef2453aacdd5bc3aab982c7d`
(master tip 2026-03-05).

## What we ship

The decode core only — enough to take one 120 s WSPR window as
complex baseband I/Q at 375 Hz and return decoded spots.

| Vendored | Purpose |
|---|---|
| `vendor/wsprd.{c,h}` | Detector + sync/demod + per-pass decode loop. The single FFT site. |
| `vendor/fano.{c,h}` | Fano sequential decoder for the K=32 r=½ convolutional code. |
| `vendor/nhash.{c,h}` | Callsign hashing (compound/hashed message types). |
| `vendor/tab.c` | Static sync/encoding tables. |
| `vendor/wsprd_utils.{c,h}` | pack/unpack callsign + locator, helpers. |
| `vendor/wsprsim_utils.{c,h}` | Channel-symbol regen used by signal subtraction. |
| `vendor/metric_tables.h` | Fano metric tables (header only). |
| `vendor/fft/{kiss_fft.c,kiss_fft.h,_kiss_fft_guts.h}` | kiss_fft (copied from `blocks/native/ft8/vendor/fft/`) backing the FFTW shim. |

## Deliberate vendor deltas

The C subset is kept as close to upstream as possible for clean
re-syncs. Two minimal, greppable changes:

1. **FFTW3 → kiss_fft.** Upstream `wsprd.c` `#include <fftw3.h>` and
   uses one 512-pt forward complex FFT (single plan, executed in a
   loop). FFTW does not cross-compile to `wasm32-unknown-unknown`.
   `shim/fftw3.h` provides the exact `fftwf_*` surface wsprd touches
   on top of the vendored `kiss_fft` (the same FFT ft8_lib uses here).
   `build.rs` puts `shim/` first on the include path so the upstream
   `#include <fftw3.h>` line stays **byte-identical**. Verified: the
   reference 0 dB signal decodes to `K1JT FN20 20` natively, identical
   to upstream.
2. **FFTW-wisdom file blocks `#if 0`'d.** Two small `fopen
   ("fftw_wisdom.dat", …)` regions in `wsprd.c` (import + export) are
   wrapped in `#if 0 /* ferrite: … */ … #endif`. They reference FFTW
   wisdom functions the shim intentionally doesn't provide and are
   meaningless without FFTW / a filesystem (WASM). Grep
   `ferrite:` in `vendor/wsprd.c` to find them.

Nothing else is modified. The `hashtable.txt` persistence is left
intact but disabled at the API boundary (`usehashtable = 0` in the
Rust wrapper) so no file is written into cwd; under WASM `fopen`
returns NULL so the guard short-circuits regardless.

## What we deliberately don't ship

| Skipped | Why |
|---|---|
| `rtlsdr_wsprd.c` / `.h` | The RTL-SDR front-end (USB capture, decimation, WSPRnet upload). Ferrite feeds 375 Hz I/Q from its own DSP chain; spot upload is out of scope. |
| FFTW3 | Replaced by the kiss_fft shim (see deltas). |
| libcurl / WSPRnet | No network egress in this crate; decode-and-display only. |

## Refresh procedure

```sh
cd /tmp && git clone --depth 1 https://github.com/Guenael/rtlsdr-wsprd.git
# Re-copy the wsprd/ subtree files listed above into vendor/.
# Re-apply the two deltas (FFT include resolves via shim; #if 0 the
# two fftw_wisdom.dat blocks). Keep kiss_fft in sync with the ft8
# crate's copy.
```

When refreshing, bump the commit pin above and run
`cargo run -p ferrite-wsprd --bin decode-wspr-iq -- <ref.iq>` plus
`cargo build -p ferrite-wsprd --lib --target wasm32-unknown-unknown`
to catch decode regressions and ABI / wasm drift.
