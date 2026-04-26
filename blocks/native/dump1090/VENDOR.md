# Vendored dump1090 (antirez classic)

Source of `vendor/` in this crate.

| field        | value |
|--------------|-------|
| upstream     | https://github.com/antirez/dump1090 |
| pinned at    | (latest at vendor time — single-file 3012-line `dump1090.c`) |
| license      | BSD-2-Clause (compatible with this codebase's GPL-3.0-or-later) |
| trimmed      | none — `anet.c/h` and `LICENSE` copied alongside `dump1090.c` |

## Why classic, not FlightAware

We considered `flightaware/dump1090` (also in `research/dump1090`). It's
much more capable — adaptive AGC, fast-DEMOD2400 path, Comm-B BDS
parsing, network bridge — but it's spread across ~30 .c files and
several global structs whose fields are read by every translation unit.
Wrapping it as a library would mean either a huge surface to keep
patched or a substantial rewrite. The antirez classic is one self-
contained file with a single global `Modes` struct; trade off mature-
fork features for a tractable wrap.

If catch rate at 2 MS/s ever becomes the bottleneck (FlightAware's
fast-DEMOD2400 reportedly catches ~30% more frames at the same RF
level), that's the upgrade path.

## What we don't compile

The `printf`-redirect-via-shim trick covers text output, but the IO /
network / interactive / `main()` paths still reference symbols
(`pthread_create`, RTL-SDR open, sockets) we don't link against.
Everything at risk is wrapped inline in `#if 0 … #endif` blocks so
upstream resyncs stay legible. Specifically excised:

- `modesInitRTLSDR`, `rtlsdrCallback`, `readDataFromFile`,
  `readerThreadEntryPoint` — RTL-SDR open and the reader thread.
  Replaced by `dump1090_push_iq_u8` in `shim/dump1090_shim.c`, which
  feeds samples synchronously into the same buffer slot the original
  callbacks wrote into.
- `snipMode`, `modesInitNet`, `modesAcceptClients`, `modesFreeClient`,
  `modesSendAllClients`, `modesSendRawOutput`, `modesSendSBSOutput`,
  `decodeHexMessage`, `aircraftsToJson`, `handleHTTPRequest`,
  `modesReadFromClients`, `modesWaitReadableClients`, `sigWinchCallback`,
  `getTermRows`, `showHelp`, `backgroundTasks`, `main` — all the CLI
  + network output + curses scaffolding.

## Other intentional edits

Two surgical changes inside the keep-zone:

1. **`useModesMessage`** (around line 1827) collapsed to "always track
   + always emit" rather than the original interactive/network-flag
   gating. In library mode we want `interactiveReceiveData` (so a
   future UI can read the aircraft list) and `displayModesMessage`
   (the text stream) on every decoded frame; the network output paths
   are excised regardless.
2. **`modesInitConfig`** drops the `getTermRows()` call (`getTermRows`
   itself is in the excised CLI block).

## Output capture

`dump1090.c` has one `#define printf dump1090_emit_text` near the top.
Every `printf(...)` callsite — `displayModesMessage`'s many fields,
`dumpRawMessage`, the few error/warn lines — routes into
`shim/dump1090_shim.c`'s per-thread ring. The Rust wrapper drains the
ring after each push and emits one `decoder::adsb` tracing event per
line. Same envelope as `multimon-ng`'s `_verbprintf` shim.

## Resyncing upstream

When bumping to a new upstream commit:

1. Re-clone `antirez/dump1090` next to `research/dump1090-classic`.
2. Diff against `vendor/dump1090.c` to pull in any decode-core fixes
   (preamble detector, CRC, CPR — those rarely change).
3. Re-apply the two surgical edits in `useModesMessage` and
   `modesInitConfig`, plus the `#if 0` wraps around the CLI/network
   functions. The wraps keep file shape so a fresh diff is the
   smallest patch shape.
4. Re-run `cargo test -p ferrite-dump1090` — `silence_does_not_panic`
   exercises the full init + push + drain path.
