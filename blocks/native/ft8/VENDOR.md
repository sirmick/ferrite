# Vendored ft8_lib

Source: <https://github.com/kgoba/ft8_lib>
License: MIT (see `vendor/LICENSE`)
Vendored at: commit `9fec6ca39886edbf96f4f5e71edc76da5074e871` (master tip 2026-05-08)

## What we ship

The streaming-decoder subset only — enough to feed 12 kHz mono `f32`
audio into `monitor_process` and drain decoded messages on FT8 slot
boundaries (every 15 s wall clock; FT4 every 7.5 s).

| Vendored | Purpose |
|---|---|
| `vendor/ft8/{constants,crc,decode,encode,ldpc,message,text}.{c,h}` | Message coding (LDPC + Costas sync + CRC), text decode + format. |
| `vendor/ft8/debug.h` | Macros only; no source. |
| `vendor/common/{monitor.c,monitor.h,common.h}` | Rolling waterfall + STFT bookkeeping that the per-slot `decode()` reads from. |
| `vendor/fft/{kiss_fft,kiss_fftr}.{c,h}` + `_kiss_fft_guts.h` | Bundled kiss_fft used by the monitor. |
| `vendor/decode_ft8_reference.c` | Upstream's CLI demo, kept as a reference for the safe-Rust wrapper but not compiled. |

## What we deliberately don't ship

| Skipped | Why |
|---|---|
| `common/audio.c`, `common/audio.h` | portaudio wrapper for live audio capture; the Rust runtime feeds samples in via FFI — we never want a second audio path. |
| `common/wave.c`, `common/wave.h` | WAV file I/O; the `decode-ft8-wav` CLI uses Rust-side WAV parsing instead. |
| `demo/gen_ft8.c` | Encoder demo; we don't transmit FT8. |
| `test/` corpus | Reused at e2e-test time via a one-off copy into `samples/`; not built into the linked artifact. |

## Refresh procedure

```sh
cd /tmp && git clone --depth 1 https://github.com/kgoba/ft8_lib.git ft8_lib_clone
# Then re-copy the subtree listed above into vendor/. Don't pull all
# of upstream — `audio.c` etc. add unneeded portaudio links.
```

When refreshing, bump the commit pin at the top of this file and
re-run `cargo test -p ferrite-ft8` to catch ABI drift.
