# Phase 3 — Aviation, APRS, ISM bulk

**Sketch.** Detail to be filled in near Phase 2's close, when we have
real numbers from the multimon-ng lift.

## Goal

Four additional C-vendor blocks riding the `blocks/native/` infrastructure
from Phase 2. Each is a bigger lift than multimon-ng's individual
decoders, but each delivers a chunky capability — ADS-B traffic, the
whole APRS network, 200+ ISM sensors, marine AIS.

## Ship list

| Block            | Vendor source     | IQ/audio            | Notes                                 |
|------------------|-------------------|---------------------|---------------------------------------|
| `AdsbDecoder`    | dump1090          | iq_f32 @ 2.4 MS/s   | Already sketched in `docs/03-blocks.md` |
| `ModeAcDecoder`  | dump1090          | iq_f32 @ 2.4 MS/s   | `mode_ac.c` — same vendor, extra 160 LOC |
| `Afsk1200Decoder` | direwolf         | real_f32 @ 48 kHz   | APRS — flagship ham feature          |
| `Afsk9600Decoder` | direwolf         | real_f32 @ 48 kHz   | G3RUH, same vendor                    |
| `Rtl433Decoder`  | rtl_433           | iq_f32 @ 1 MS/s     | ~200 device decoders in one block; params select subset |
| `AisDecoder`     | rtl-ais / aisdec  | real_f32 @ 48 kHz (or iq) | Marine AIS; separate pure-C decoder |

## Flowgraph presets

- `adsb.json` — fixed 1090 MHz source, channelizer @ 2.4 MS/s → AdsbDecoder
  → EventsSink with aircraft map UI.
- `aprs.json` — source → channelizer(12.5 kHz @ 144.390 MHz region-default)
  → FmDemod → Resample(48k) → Afsk1200Decoder → EventsSink with map UI.
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
