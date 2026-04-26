# Vendored aisdecoder (from rtl-ais)

Source of `vendor/` in this crate.

| field        | value |
|--------------|-------|
| upstream     | https://github.com/dgiardini/rtl-ais (the `aisdecoder/` subdirectory) |
| pinned at    | (latest at vendor time — `aisdecoder/` plus `aisdecoder/lib/`) |
| license      | GPL-2.0-or-later (compatible with this codebase's GPL-3.0-or-later) |
| trimmed      | only the `aisdecoder/` subdirectory copied; the `rtl_ais.c` RF
                  pipeline and the `tcp_listener/` TCP/UDP NMEA bridge are
                  *not* vendored |

## Why only the aisdecoder subdirectory

rtl-ais is two layers:

- `rtl_ais.c` — the RF pipeline: opens an RTL-SDR, dual-rotates two AIS
  channels into baseband, downsamples, FM-demods, resamples to 48 kHz,
  feeds the audio into aisdecoder. None of this fits Ferrite's existing
  block model — we'd be duplicating Channelizer + FmDemod +
  RealF32Resamp.
- `aisdecoder/` — the GMSK clock-recovery PLL + AIVDM frame builder.
  This is the part that doesn't already exist in Ferrite, and it has a
  clean per-channel API (`receiver_run` walks one mono buffer's worth
  of samples through one decoder instance).

Vendor only the second layer. The first is replaced by a Ferrite preset:
`flowgraphs/ais.json` puts `Channelizer × 2 → FmDemod × 2 → RealF32Resamp × 2`
on each AIS channel and feeds the two real_f32 streams into `AisDemod`'s
two input ports.

## What we don't compile

- `rtl_ais.c` and `main.c` — the CLI binary. Not in `vendor/`.
- `tcp_listener/` — the TCP NMEA bridge. Not in `vendor/`.
- `convenience.{c,h}`, `heatmap/`, `Dockerfile`, etc. — packaging /
  helpers we don't need. Not in `vendor/`.
- `aisdecoder/sounddecoder.c::runSoundDecoder` — file-/stdin-driven
  main loop, replaced by synchronous `run_mem_decoder` from the
  runtime tick. Wrapped in `#if 0` inline.

## Edits inside the keep-zone

Two surgical changes inside `vendor/aisdecoder.c` (everything in
`vendor/lib/` is verbatim):

1. Headers `<netdb.h>`, `<sys/socket.h>`, `<netinet/in.h>`, the WIN32
   `<winsock2.h>` block, `<getopt.h>`, and `tcp_listener.h` removed —
   the network bridge is excised and they have no other consumers.
   The `#include "lib/callbacks.h"` and `#include "sounddecoder.h"`
   includes that the keep-zone needs stay.
2. `init_ais_decoder` body collapsed: removed the `initSocket` /
   `initTcpSocket` calls (and the `EXIT_FAILURE` early returns that
   went with them). Argument list unchanged so the shim's call site
   matches upstream's signature; the host/port/use_tcp/keep arguments
   become `(void)`-cast unused.
3. `send_nmea` redefined as a `#define send_nmea(...) (0)` no-op so
   `nmea_sentence_received` (which we keep — that's where decoded
   frames hand off to `append_message`) compiles without the socket
   path. The `#if 0`-wrapped original definition stays inline for
   future-sync legibility.
4. `isBroadcastAddress` and the original `initSocket` definition
   wrapped in `#if 0`. They reference excised types (`struct
   addrinfo`, `WSADATA`) and have no other callers.
5. `free_ais_decoder` drops the `freeaddrinfo` / `WSACleanup` calls
   on the same grounds.

The shim layer (`shim/ais_shim.{c,h}`) wraps `init_ais_decoder`,
`run_rtlais_decoder`, and the existing `aisdecoder_next_message` queue
drain into Ferrite's standard four-call API
(`init` / `push_audio` / `drain` / `reset`).

## Output capture

aisdecoder already exposes a per-process linked-list message queue
(`append_message` enqueues, `aisdecoder_next_message` dequeues) — the
upstream binary uses it to feed its UDP/TCP bridge. We re-export that
queue as `ais_drain` joining sentences with `\n`, the same envelope
every other decoder wrap uses. No `printf` redirect needed.

## Resyncing upstream

When bumping to a new upstream commit:

1. Re-clone `dgiardini/rtl-ais` next to `research/rtl-ais`.
2. `cp -r research/rtl-ais/aisdecoder/* blocks/native/rtl-ais/vendor/`
   (overwrite).
3. Re-apply the `#if 0` wraps and the `send_nmea` macro override
   inside `aisdecoder.c`. The diff is small enough to rebuild by
   hand from this doc.
4. Re-run `cargo test -p ferrite-rtl-ais`. The `silence_does_not_panic`
   test exercises the full init + push + drain path.
