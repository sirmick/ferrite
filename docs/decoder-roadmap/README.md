# Decoder roadmap

Forward-looking plan for growing Ferrite's decode/demod breadth into a
broad SDR/ham decoder suite. Organised **capability-first**: each phase
ships a set of user-facing capabilities, not a set of project ports.

**Where we are (October 2026):** Phases 1, 2 and 5 are shipped. Phase 3
is shipped bar Mode A/C and broader rtl_433 coverage. Phase 4's
weak-signal half (FT8/FT4/WSPR) shipped; satellite imagery has not
started. Phase 6 shipped DMR (metadata + voice) and the dsd-fme helper;
native P25/NXDN framing, trunking and Codec2/M17 remain.

## Why a separate roadmap

The top-level `docs/08-roadmap.md` covers phases 0–G (platform scaffolding
through runtime pivot). Once the one-runtime-one-block milestone (M1–M5) is
closed — the Rust runtime drives both native and WASM, presets are one
JSON doc, blocks are dual-compiled — Ferrite is ready to *grow decoders*
rather than rework the engine. This roadmap picks up there.

## Architectural levers this plan leans on

Ferrite already has a lot of the hard stuff built. Every phase below is
designed to reuse what exists rather than add parallel machinery.

- **Block trait + `BlockFactory` + `SpecRegistry`** (`blocks/`, `runtime/`)
  — new decoders are just new `BlockSpec`s in the registry. No new plumbing.
- **Typed ports** (`iq_f32`, `real_f32`, `bits`, `frames`, `events`, …) —
  each decoder declares its port shape; the runtime validates wiring.
- **Port-carried metadata** (sample rate, center freq) — blocks negotiate
  at `init()`; the resampler block is the universal glue between rate
  mismatches.
- **Dual-compile crate pattern** — one source tree, `cargo build` for the
  server, `wasm-pack build` for the browser. Same for C-vendor blocks via
  the `cc` crate + `clang --target=wasm32`.
- **`FlowgraphDoc` JSON presets** — every capability in this roadmap is a
  preset file, not a code path. "Decode pagers on 929 MHz" is a JSON doc,
  not a server feature.
- **`WsBridge` pairs** — a decoder can live server-side or browser-side;
  the scheduler inserts the bridge. Heavy C vendors (dumpvdl2, SatDump)
  naturally stay server-side. Lightweight ones (multimon-ng) can run
  either place.
- **Channelizer → N × narrowband-IQ → WS streams** — multiple simultaneous
  VFOs are already the model; decoders hang off individual VFOs without
  the server needing to know about them.

The rest of this roadmap is a matter of (a) growing the block library, (b)
growing the preset library, (c) building the `blocks/native/` infrastructure
once so subsequent C-vendor ports are cheap.

## Phases

| Phase | Focus                                              | Status      |
|-------|----------------------------------------------------|-------------|
| 1     | [Analog listening](01-phase-1-analog-listening.md) | ✓ shipped   |
| 2     | [First C-vendor wave — multimon-ng umbrella](02-phase-2-multimon-vendor.md) | ✓ shipped (with scope creep) |
| 3     | [Aviation, APRS, ISM bulk](03-phase-3-aviation-aprs-ism.md) | mostly shipped — ADS-B, APRS, AIS, rtl_433 (433.92 MHz preset); Mode A/C + broader rtl_433 open |
| 4     | [Weak-signal + satellite imagery](04-phase-4-weak-signal-sat.md) | weak-signal shipped (FT8/FT4 `ft8_lib`, WSPR `wsprd`); satellite imagery / sondes / ACARS not started |
| 5     | [Ham digital modes](05-phase-5-ham-digital.md)     | ✓ shipped via vendored fldigi (RTTY, PSK31, CW, MT63, Olivia, Contestia, DominoEX, Throb, NAVTEX, RSID); RDS shipped in Phase 1. Not shipped: SSTV, WEFAX, Hell, MFSK, DSC |
| 6     | [Digital voice + native-helper protocol](06-phase-6-digital-voice.md) | partial — DMR shipped (`DmrDecode` + mbelib voice, D34) + `DsdHelper` (operator-installed dsd-fme for P25/NXDN/D-STAR/YSF); native P25/NXDN framing, multi-channel, trunking, Codec2/M17 open |

Plus the mechanics reference:

- [Vendor port guide](90-vendor-port-guide.md) — concrete recipe for
  vendoring a C decoder as a Ferrite block. Referenced from Phases 2+.

## Cross-references

- Capability inventory: `/research/CAPABILITIES_ROADMAP.md`
- Per-project feasibility scorecards: `/research/WASM_PORT_ASSESSMENT.md`
- Block trait / port types / build split: `docs/03-blocks.md`
- Flowgraph JSON schema: `docs/04-flowgraphs.md`
- Roadmap through Phase G: `docs/08-roadmap.md`
- Decisions: `docs/09-decisions.md` (decoder-related: D15/D34, D28)

## Sequencing principle

Each phase both **ships user-visible capability** and **builds shared
infrastructure that the next phase leans on**. Phase 1 ships listening
modes while building the small reusable helper blocks (`Deemphasis`,
`Squelch`, `Agc`, `Resample`). Phase 2 ships five decoders while building
the `blocks/native/` C-vendor substrate that every subsequent phase
consumes. Phase 3 consumes that substrate four more times. And so on.

If a phase doesn't build something the next phase will reuse, it's
probably the wrong phase boundary.
