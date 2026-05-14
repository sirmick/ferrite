# Block test fixtures

Binary fixtures the `blocks/tests/*_e2e.rs` integration tests load.
Kept under version control so test runs are deterministic without an
external download step.

| File | Source | Used by | Notes |
|---|---|---|---|
| `modes1.bin` | [antirez/dump1090](https://github.com/antirez/dump1090) `testfiles/modes1.bin`, BSD-2-Clause | `adsb_e2e.rs` | Raw u8 interleaved I/Q at 2 MS/s, ≈178 ms recording. Decodes a DF 17 ADS-B (ICAO `4d2023`, 24 275 ft baro altitude), a DF 11 all-call reply, and a DF 4 surveillance reply when fed through `AdsbDemod`. The sigidwiki ADS-B page links its IQ sample on mega.nz only — too friction-y for an in-repo fixture, so we ship the dump1090 upstream test recording instead. |
| `rtl433_acurite_606tx_433mhz_250ks.cu8` | [merbanan/rtl_433_tests](https://github.com/merbanan/rtl_433_tests/blob/master/tests/acurite/Acurite_606TX/gfile002.cu8), GPL-2.0+ | `rtl433_e2e.rs` | Raw u8 interleaved I/Q at 250 kS/s centred on 433.92 MHz, ≈0.5 s. Acurite 00606TX temperature-only sensor — when fed through `Rtl433Demod` the upstream pulse_detect locks an OOK_PULSE_PWM burst and emits a `decoder::rtl_433` event with the `model: "Acurite-606TX"` line. Sigidwiki has the SigID page for Acurite weather stations but no shipping IQ sample on the site itself; rtl_433_tests is the canonical fixture corpus. |
