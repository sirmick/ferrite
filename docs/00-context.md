# 00 — Context and goals

## What is Ferrite

Ferrite is a modern web-based SDR application. One listens to, explores, and
decodes radio spectrum through a browser, with the radio itself connected to a
small Rust daemon on an ARM single-board computer placed close to the antenna.

"Close to the antenna" matters. RF cable loss is real; network cable has none.
Putting the SDR on a Pi in the shack next to the feedline and backhauling over
wifi or fiber beats running coax across a house.

## Why build it

Current options cluster into three flavors, each with real limitations:

- **Desktop native applications** — SDR++, CubicSDR, SDRangel, Gqrx, HDSDR, SDR#.
  Capable; typically dense UIs with a "many knobs in a crowded grid" aesthetic;
  none run in a browser. Installation, driver fights, platform-specific bugs.
  Work you do to get them running does not follow you between machines.

- **Web-based multi-user receivers** — KiwiSDR, OpenWebRX (+, Plus). Aim at
  "let a crowd listen through my radio over the internet." Good at that.
  Not primarily designed as the tool the operator of the radio uses day-to-day;
  UIs are functional but feel like mid-2010s web. Extensibility is limited;
  adding a new decoder usually means patching the server.

- **GNU Radio / GRC flowgraphs** — flexible, authoritative for signal
  processing, but the UX is an engineering tool, not a listening tool. Building
  a pleasant receiver on top of GRC requires a second layer of work that most
  people skip.

Ferrite sits in the gap: a browser-native listening tool with a first-class
block-based DSP architecture underneath. No plugin install, no client-side
native code, no cross-platform installers. Open a URL, pick a device, listen.

## Target users

- Solo operators of their own SDR hardware who want a good day-to-day listening
  tool, not a crowd-sharing station.
- Developers who want to port decoders (ADS-B, APRS, FT8, digital voice, …) by
  writing a block, not a fork of an application.
- Experimenters who want to record, replay, and identify signals they don't
  recognize.

Not (for now) multi-tenant web-SDR station operators. Ferrite's single-listener,
LAN-trust posture is a conscious choice for v0.1 — see `09-decisions.md`.

## Goals

- **Feels modern.** Web UI that behaves the way people expect good web UI to
  behave in 2026: fast, keyboard-friendly, keyboard-accessible, clean type,
  smooth zoom, no layout jitter.
- **Spectrum is the main object.** The waterfall/panadapter is not one panel
  among many — it is the thing. Everything else orbits it.
- **Block-based DSP, authored in JSON.** Adding a new decoder is writing a
  block (Rust or ported C core) and a flowgraph file. No UI code required to
  wire up the DSP.
- **Runs on an ARM SBC.** The backend fits on a Pi next to the antenna.
  Deployment is `systemctl start ferrited` and open a browser.
- **Portable blocks.** The same block runs server-side (native, linked into
  `ferrited`), browser-side (WASM, in a Worker), and optionally server-side
  again in a small Node sidecar for headless decoding.
- **Testable without hardware.** A first-class IQ replay mode means every
  decoder has a golden fixture in CI. You can develop the full stack on a
  laptop with no radio attached.
- **Identify unknown signals.** Drag a box on the waterfall, send a snapshot
  plus metadata to an LLM with a scraped sigidwiki index as RAG context, get
  a guess with deep-links back to the wiki.

## Non-goals (v0.1)

- **Multi-user / crowd-sharing.** Single listener, last-connect wins. Adding
  multi-listener later is a natural extension, not a rewrite.
- **Transmit.** Listening and decoding only.
- **Visual flowgraph editor.** Flowgraphs are JSON files. GRC-style node
  editor is a different product; we are not building it.
- **Windows-style configuration sprawl.** No plugin system, no per-driver
  UI code — driver knobs come from Soapy capability introspection.
- **Authentication / accounts.** The box is on a trusted network or behind a
  VPN. Remote access is the user's tunneling problem.

## Hardware scope

Developed and validated against RTL-SDR (RTL2832U) and SDRPlay RSPduo.
Both work via SoapySDR today. Design does not close doors on HackRF, Airspy,
USRP, or any other Soapy-supported device — the driver layer is Soapy, and
the options dialog is generated from Soapy's capability introspection, so new
drivers appear in the UI without frontend changes.

## What "done" looks like for v0.1

A user plugs an RTL-SDR into a Raspberry Pi, flashes a Ferrite image, powers
it on, opens a browser on their laptop pointed at the Pi, picks the device,
tunes to a local FM broadcast station via the per-digit frequency widget, and
hears it. They drag the VFO cursor on the waterfall; audio follows. They
toggle AGC; gain behavior updates without reconnecting. The experience feels
quick and purposeful on a 2020s laptop.

Everything else — decoders, identify feature, spectrum explorer, headless
sidecar — slots on top of that foundation without touching it.
