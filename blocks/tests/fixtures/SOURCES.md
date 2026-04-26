# Block test fixtures

Binary fixtures the `blocks/tests/*_e2e.rs` integration tests load.
Kept under version control so test runs are deterministic without an
external download step.

| File | Source | Used by | Notes |
|---|---|---|---|
| `modes1.bin` | [antirez/dump1090](https://github.com/antirez/dump1090) `testfiles/modes1.bin`, BSD-2-Clause | `adsb_e2e.rs` | Raw u8 interleaved I/Q at 2 MS/s, ≈178 ms recording. Decodes a DF 17 ADS-B (ICAO `4d2023`, 24 275 ft baro altitude), a DF 11 all-call reply, and a DF 4 surveillance reply when fed through `AdsbDemod`. The sigidwiki ADS-B page links its IQ sample on mega.nz only — too friction-y for an in-repo fixture, so we ship the dump1090 upstream test recording instead. |
