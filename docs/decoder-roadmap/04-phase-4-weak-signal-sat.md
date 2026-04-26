# Phase 4 — Weak-signal + satellite imagery

**Status:** not started. Sketch only — detail to follow as Phase 3 closes
(rtl_433 + AIS + Mode A/C still open).

## Goal

Pull in the two big "specialist" categories: weak-signal ham (FT8 / FT4 /
WSPR) and satellite imagery (NOAA APT, Meteor LRPT, GOES, radiosondes).
These are lower-frequency use cases per user but high-impact and
demo-worthy.

## Ship list

| Block               | Vendor source         | Notes                              |
|---------------------|------------------------|------------------------------------|
| `Ft8Decoder`        | `kgoba/ft8_lib`        | Pure C, ~1.5k LOC, already WASM-friendly |
| `Ft4Decoder`        | `kgoba/ft8_lib`        | Same vendor, different cycle       |
| `WsprDecoder`       | `kgoba/ft8_lib` or dedicated | Longer cycle (2 min), different LDPC |
| `AptDecoder`        | `noaa-apt` (Rust!)     | No C-vendor needed — pure Rust crate |
| `LrptDecoder`       | `SatDump` LRPT module  | C++ extract, harder                |
| `GoesDecoder`       | `SatDump` / `goestools`| C++ extract                        |
| `RadiosondeRs41Decoder` | `rs1729/RS` RS41 family | One of many sonde families; pick most-common first |
| `AcarsDecoder`      | `acarsdec`             | Included here to keep aviation-related niche decoders together |

## SatDump — strategic consideration

Rather than vendoring LRPT/GOES/APT as three separate C lifts, evaluate
**SatDump** as a single strategic vendor. It's a modular C++ plugin
system covering APT, LRPT, HRIT, LRIT, GK-2A, and a long list of LEO
satellites. If its plugin architecture extracts cleanly, one lift
unlocks the entire satellite-imagery tier.

Counterargument: it's a larger and more complex codebase than
multimon-ng or dump1090. Do a dedicated feasibility spike at the start
of Phase 4 (clone it into `research/`, ~1 day's scoring) before
committing to the monolithic-vendor path vs the pick-per-sat path.

## UI additions

- **Batch-mode decoder UI.** FT8 is 15-second cycles; WSPR is 2-minute.
  The UI should show a waterfall-correlated decode log per cycle.
- **Image viewer for satellite images.** A tile-buildup view —
  lines arrive over 10–15 minutes of a pass and populate top-to-bottom.
- **Pass predictor integration.** Not in this phase; a Phase 5+ polish
  item. For now, user starts decoding when they know a pass is due.

## Key architectural calls

- **15-second batched input for FT8.** Breaks the streaming-block
  pattern; the block buffers internally and only emits events once per
  cycle. Fine — the `events` port semantics don't require continuous
  output. Document the pattern; it'll recur (WSPR, radiosondes).
- **Timebase sync for FT8.** FT8 decoding is cycle-synchronous to UTC
  wall-clock. The block needs wall-clock injection from Ferrite's
  runtime; add a `time_source` param or pass via block context at
  `init()`.
- **liquid-dsp's FEC codes pay off here.** LDPC for FT8, Reed-Solomon
  for many radiosondes, Viterbi elsewhere. If Phase 2's liquid-dsp lift
  landed, Phase 4 reuses.

## Risks

- **SatDump extraction.** Likely the longest individual lift in this
  plan if it's the chosen path. Ring-fence one week for the feasibility
  spike; bail to per-decoder vendors if it's ugly.
- **Radiosonde family fragmentation.** RS41, DFM, M10, iMet — each its
  own decoder. Ship RS41 first (Europe/US most common), add families
  as user demand arises.
- **WSPR's 2-minute cycle memory footprint.** 12 kHz × 120 s × 4 bytes
  = ~5.7 MB per active WSPR block. Fine on a server, painful for
  browser-side WASM. Ship WSPR as server-side by default.

## Estimated effort

- ft8_lib vendor (FT8 + FT4 + WSPR): ~2 weeks.
- `noaa-apt` integration (Rust crate already): ~3 days.
- SatDump feasibility + first working sat decode: ~2–3 weeks if it
  works, ~1 week to bail and plan per-decoder.
- Per-decoder satellite (LRPT/GOES) if SatDump path dropped: ~2 weeks
  each.
- acarsdec: ~1 week.
- RS41 radiosonde: ~1 week.

Total: ~8–10 weeks depending on satellite path.
