# 00 — Context and goals

## What Ferrite is

A web-based SDR application. The radio is connected to a small Rust daemon
(`ferrited`) that runs near the antenna, typically on an ARM SBC; a SvelteKit
front end runs in any modern browser on the LAN and does the demodulation in
WASM. The wire between them carries a wideband spectrum tap plus narrowband IQ
slices for any active VFO.

Source layout that matches this split:

- [`server/`](../server/) — `ferrited` axum daemon. Owns hardware, the FFT
  tap, and the WS transport.
- [`runtime/`](../runtime/) — `ferrite-runtime`. The flowgraph scheduler.
  Dual-compile (`rlib` + `cdylib`); `ferrited` links it natively and the
  browser loads it as WASM.
- [`blocks/`](../blocks/) — `ferrite-blocks`. The DSP unit. Same dual-compile
  story; same source feeds both ends.
- [`web/`](../web/) — SvelteKit app served by `ferrited`'s static layer.
- [`flowgraphs/`](../flowgraphs/) — JSON presets the runtime instantiates
  (`wbfm`, `wbam`, `dtmf-e2e`, capture and record presets).

## Why

The existing options cluster:

- **Desktop native apps** (SDR++, CubicSDR, SDRangel, Gqrx, HDSDR, SDR#) —
  capable, dense, install-per-machine. Configuration doesn't follow you
  between machines.
- **Crowd-sharing web SDRs** (KiwiSDR, OpenWebRX) — built so a *crowd* can
  listen through one radio, not so the radio's owner uses it day-to-day.
- **GNU Radio / GRC** — authoritative for DSP; UX is an engineering tool.

Ferrite sits in the gap: a browser-native listening tool with a JSON-driven
DSP graph underneath. Open a URL on the LAN, pick a band entry, listen.

## Target users

- Solo operators of their own SDR who want a good day-to-day listening tool.
- Developers porting decoders (ADS-B, APRS, FT8, digital voice, …) by writing
  a Rust block plus a flowgraph file rather than forking an application.
- Experimenters who want to capture, replay, and identify signals.

Not multi-tenant web-SDR station operators. v0.1 is single-listener,
LAN-trust — see [09-decisions.md](09-decisions.md) D06 and D07.

## Goals

- **Spectrum is the main object.** The waterfall and spectrum line are the
  thing; controls orbit them. The current UI is a fixed spectrum-over-
  waterfall layout with a left sidebar (D26).
- **JSON-authored flowgraphs.** Adding a decoder is `flowgraphs/<name>.json`
  plus any new blocks. No UI code required to wire DSP.
- **One block crate, two compile targets.** `ferrite-blocks` builds native
  for `ferrited` and `wasm32-unknown-unknown` for the browser; same
  `cargo test` covers both. See D02.
- **One runtime crate, two compile targets.** `ferrite-runtime` runs the
  same scheduler in both ends. See D19.
- **Replay mode is first-class.** `FileIqSource` is a registered source like
  any other; `--source-type FileIqSource --source-path …` substitutes for an
  RTL-SDR. See D09.

## Non-goals (v0.1)

- **Multi-user / crowd-sharing.** Single listener.
- **Transmit.** Receive only.
- **Visual flowgraph editor.** Flowgraphs are JSON files.
- **Authentication.** LAN-trust; remote access is the user's tunnel problem.
- **Mobile / tablet.** Desktop-first.

## Hardware scope

Developed and validated against RTL-SDR (RTL2832U) and SDRplay RSPduo via
SoapySDR. Anything else SoapySDR exposes should also work — the source dialog
renders from `GET /api/devices` capability schema (see D13), so adding a
driver does not require frontend changes.

## What "done" looks like for v0.1

A user plugs an RTL-SDR into a Pi, opens the page, picks a band entry from
the FM-broadcast list, hears it. The shipped `flowgraphs/wbfm.json` is the
chain that path runs through; the AM equivalent is `wbam.json`.
