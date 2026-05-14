# Phase 3 — Aviation, APRS, ISM bulk

**Status:** in progress (April 2026). Two of the planned capabilities
already shipped, two outstanding.

What landed:

- **AX.25 packet (APRS / BBS / AMSAT digipeaters)** — shipped in Phase 2
  via multimon-ng (see Phase 2 doc), not direwolf. `PacketDemod` runs
  AFSK1200 + three AFSK2400 timing variants + FSK9600 in parallel.
  `packet.json` preset (formerly `aprs.json` — renamed to reflect that
  the block decodes generic AX.25, not APRS-specifically; APRS is the
  typical 144.39 MHz payload). Validated live with K6RPT-0 / W6TST-1 /
  BKELEY-0 captures.
- **ADS-B / Mode S** — `AdsbDemod` block at `blocks/src/adsb.rs`,
  vendored from antirez's classic dump1090 (`blocks/native/dump1090/`).
  `adsb.json` preset, 1090 MHz, 2 MS/s. Validated against upstream's
  test fixture (DF 17 / DF 4 / DF 11 frames decode cleanly).

What's outstanding:

- **Mode A/C** (secondary radar transponder) — same `dump1090` vendor,
  separate `mode_ac.c` we didn't pull in. ~160 LOC follow-up.
- **rtl_433** — 200+ ISM device decoders at 433/915 MHz. The big lift
  in this phase; deferred.
- **AIS** (marine) — separate vendor (rtl-ais or aisdecoder, not in
  multimon-ng or dump1090). Deferred.

The original plan called for direwolf as the APRS vendor. Multimon-ng's
AFSK1200 turned out to be the same capability for ~10× less integration
effort (it was already part of the Phase 2 lift), so direwolf isn't
needed unless we discover a multimon-ng decoding gap that direwolf would
close. None observed so far.

## Goal

Four additional C-vendor blocks riding the `blocks/native/` infrastructure
from Phase 2. Each is a bigger lift than multimon-ng's individual
decoders, but each delivers a chunky capability — ADS-B traffic, the
whole APRS network, 200+ ISM sensors, marine AIS.

## Ship list

| Block            | Vendor source     | IQ/audio            | Status                                 |
|------------------|-------------------|---------------------|----------------------------------------|
| `AdsbDemod`      | antirez/dump1090  | iq_f32 @ 2 MS/s     | ✓ shipped (`blocks/native/dump1090/`)  |
| `ModeAcDecoder`  | dump1090          | iq_f32 @ 2 MS/s     | ✗ not shipped — `mode_ac.c` follow-up  |
| `PacketDemod` (AFSK1200/2400) | multimon-ng | real_f32 @ 22050 Hz | ✓ shipped via Phase 2 lift            |
| `PacketDemod` (FSK9600)       | multimon-ng | real_f32 @ 22050 Hz | ✓ shipped via Phase 2 lift            |
| `Rtl433Decoder`  | rtl_433           | iq_f32 @ 1 MS/s     | ✗ not shipped — biggest remaining lift |
| `AisDecoder`     | rtl-ais / aisdec  | real_f32 @ 48 kHz (or iq) | ✗ not shipped                    |

## Flowgraph presets

- `adsb.json` — fixed 1090 MHz source, channelizer @ 2.4 MS/s → AdsbDecoder
  → EventsSink with aircraft map UI.
- `packet.json` — source → channelizer(25 kHz) → FmDemod → Resample(22050)
  → PacketDemod (five multimon variants in parallel) → audio + decoder
  events. 144.39 MHz NA / 144.800 EU / 145.175 AU come from the bands
  config; APRS is the typical payload but the block decodes any AX.25.
- `rtl433-ism.json` — source → channelizer(1 MS/s @ 433.92 MHz or
  915 MHz) → Rtl433Decoder → EventsSink.
- `ais.json` — source → channelizer(48 kHz @ 161.975/162.025 MHz) →
  AisDecoder → vessel-map UI.

## Key architectural calls

- **ADS-B bundle size.** dump1090's DSP core is small; a per-block WASM
  module is easy.
- **rtl_433 size.** 200 decoders = large WASM blob. Decision needed:
  ship as one block with a runtime `enabled_decoders: [...]` param
  (simpler), or compile N tiny WASMs (smaller browser payload, more
  build complexity). Default to the former unless measurements force
  the latter.
- **direwolf per-ctx state.** Direwolf assumes global per-channel state
  arrays; the port needs to heap-allocate per block instance. Documented
  in `research/WASM_PORT_ASSESSMENT.md`. Mechanical but requires care.
- **Server vs browser placement.** ADS-B at 2.4 MS/s is heavy in the
  browser. Consider running `AdsbDecoder` server-side by default — the
  `WsBridge` makes this trivial. rtl_433 similarly, if bundle size or
  CPU become issues.

## UI additions

- **Map-oriented panes.** ADS-B (aircraft), APRS (stations/beacons),
  AIS (vessels) all want a Leaflet-style map view with live markers and
  a list-panel sidecar. Build one generic `MapEventsSink` component,
  configure per-decoder.
- **ISM sensor log.** rtl_433 events are tabular — reuse the Phase 2
  events pane with column templates per device type.

## Risks

- **Test fixtures for ADS-B.** Beast-format reference recordings are
  standard; parity-test against FlightAware or a local known-good
  receiver over the same RF capture.
- **rtl_433 library test matrix.** 200 decoders × per-device fixtures =
  a lot of green dots to maintain. Rely on rtl_433's own fixture set
  rather than rolling our own.
- **Legal footprint of AIS in some regions.** None meaningful for
  receive-only, but worth a line in the decoder docs.

## Estimated effort

Given the Phase 2 infrastructure exists:

- dump1090 (ADS-B + Mode A/C): ~1 week end-to-end.
- direwolf (AFSK 1200/9600): ~1.5 weeks.
- rtl_433: ~2 weeks — mostly decoder-table ergonomics and test suite.
- rtl-ais: ~1 week.

Total Phase 3: ~5–6 weeks of focused work. Can run two tracks in
parallel (e.g. ADS-B + rtl_433) if a second hand is available.
