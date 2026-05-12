# 00 — Context and goals

## What Ferrite is

A browser-native SDR application with an optional AI operator. The radio is
connected to a small Rust daemon (`ferrited`) that runs near the antenna —
typically on an ARM SBC or a small Linux box on the LAN. A SvelteKit front
end runs in any modern desktop browser and does demodulation in WASM. The
wire between them carries a wideband spectrum tap plus narrowband IQ slices
for any active VFO.

An optional Node sidecar (`ferrite-ai`, wrapping the Claude Agent SDK) sits
beside `ferrited` and exposes a chat surface that drives the radio through
the same REST API a human would use — same tools, no privileged path.

Source layout that mirrors this split:

- [`server/`](../server/) — `ferrited` axum daemon. Owns hardware, the FFT
  tap, the WS transport, and reverse-proxy for the AI sidecar.
- [`runtime/`](../runtime/) — `ferrite-runtime`. The flowgraph scheduler.
  Dual-compile (`rlib` + `cdylib`); `ferrited` links it natively and the
  browser loads it as WASM.
- [`blocks/`](../blocks/) — `ferrite-blocks`. The DSP unit. Same
  dual-compile story; same source feeds both ends. Five native vendor
  crates under `blocks/native/` (`liquid-dsp`, `multimon-ng`, `dump1090`,
  `rtl-ais`, `ft8`) also compile twice.
- [`tools/ferrite-ai/`](../tools/ferrite-ai) — Claude Code sidecar.
- [`tools/ferrite-ctl/`](../tools/ferrite-ctl) — thin CLI that drives
  `ferrited` via REST. Used by humans and by the AI.
- [`tools/fft_to_png.py`](../tools/fft_to_png.py),
  [`tools/fft_peaks.py`](../tools/fft_peaks.py) — capture analysis tools.
- [`web/`](../web/) — SvelteKit app served by `ferrited`'s static layer.
- [`flowgraphs/`](../flowgraphs/) — 21 JSON presets the runtime
  instantiates (listening modes, decoder chains, capture helpers).
- [`packaging/`](../packaging/) — Docker matrix that builds native
  `.deb` / `.rpm` packages.

## Why

The existing options cluster:

- **Desktop native apps** (SDR++, CubicSDR, SDRangel, Gqrx, HDSDR, SDR#) —
  capable, dense, install-per-machine. Configuration doesn't follow you
  between machines.
- **Crowd-sharing web SDRs** (KiwiSDR, OpenWebRX) — built so a *crowd* can
  listen through one radio, not so the radio's owner uses it day-to-day.
- **GNU Radio / GRC** — authoritative for DSP; UX is an engineering tool.

Ferrite sits in the gap: a browser-native listening tool with a JSON-driven
DSP graph underneath, plus an AI operator that knows what your rig is and
can drive it for you. Open a URL on the LAN, pick a band entry, listen —
or tell the chat panel what you want and let it find a signal.

## Target users

- Solo operators of their own SDR who want a good day-to-day listening tool.
- Hobbyists who want to explore a band without having to remember every
  driver's gain ladder and antenna conventions — the AI knows them.
- Developers porting decoders (ADS-B, APRS, FT8, digital voice, …) by
  writing a Rust block plus a flowgraph file rather than forking an
  application.
- Experimenters who want to capture, replay, render, and identify signals
  from the same browser surface.

Not multi-tenant web-SDR station operators. Single-listener, LAN-trust
— see [09-decisions.md](09-decisions.md) D06 and D07.

## Goals

- **Spectrum is the main object.** The waterfall and spectrum line are the
  thing; controls orbit them. Auto-contrast (P5/P98) and auto-scale by
  default; manual sliders for pixel-peeping.
- **JSON-authored flowgraphs.** Adding a decoder is `flowgraphs/<name>.json`
  plus any new blocks. No UI code required to wire DSP.
- **One block crate, two compile targets.** `ferrite-blocks` builds native
  for `ferrited` and `wasm32-unknown-unknown` for the browser; same
  `cargo test` covers both. See D02.
- **One runtime crate, two compile targets.** `ferrite-runtime` runs the
  same scheduler in both ends. See D19.
- **The AI uses the same tools the human does.** No privileged AI path —
  `ferrite-ai` invokes `ferrite-ctl` and the same Python capture tools an
  operator runs from a shell. Every command surfaces in the UI activity
  log via reverse-log.
- **Replay mode is first-class.** `FileIqSource` is a registered source
  like any other; `--source-type FileIqSource --source-path …` substitutes
  for an RTL-SDR. See D09.
- **Preset-imposed hard overrides.** `force_params` lets a preset pin
  known-good values that win over live state — used by `wbam.json` to
  pin `agc_enable=false` because AM AGC pumping resets the AGC about
  once per second.

## Non-goals

- **Multi-user / crowd-sharing.** Single listener, last-connect wins.
- **Transmit.** Receive only.
- **Visual flowgraph editor.** Flowgraphs are JSON files.
- **Authentication.** LAN-trust; remote access is the user's tunnel
  problem.
- **Mobile / tablet.** Desktop-first.
- **DMR / DSD.** AMBE patent encumbrance, deferred.

## Hardware scope

Developed and validated against RTL-SDR (RTL2832U), SDRplay (RSPduo /
RSPdx), HackRF, Airspy R2, Airspy HF+, BladeRF, PlutoSDR. Anything else
SoapySDR exposes should also work — the source dialog renders from
`GET /api/devices` capability schema (see D13), so adding a driver does
not require frontend changes.

## What "done" looks like for 1.0

The user pipeline from package install to listening:

1. `sudo apt install ./ferrite_*.deb` (or the matching rpm).
2. `sudo systemctl enable --now ferrited` (plus `sdrplay` if SDRplay).
3. Open `http://<host>:10001`.
4. Pick a device + antenna from the source dialog.
5. Pick a preset from the catalog.
6. Listen.

The optional path: open the AI tab, describe what you have plugged in
(*"discone for VHF, 40 m end-fed for HF"*), and ask it to find something.
It tunes, captures, renders, and reports back — with the spectrum strips
inline in chat.

What's still missing for 1.0 is decoder UI surface area (ADS-B aircraft
table + map, APRS station list + map), the `rtl_433` ISM bundle, and
Mode A/C — see [08-roadmap.md](08-roadmap.md).
