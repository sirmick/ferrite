# sigidwiki sample corpus

Reference audio samples downloaded verbatim from
[sigidwiki.com](https://www.sigidwiki.com), the Signal Identification
Guide. Used as a regression-test corpus for the multimon-ng wrapper
analyzer binaries (`analyze-pocsag-wav`, `analyze-packet-wav`) and to
A/B captured live audio against canonical reference content.

Run `convert.py` to (re-)generate `22050_mono/*.wav` — 22 050 Hz mono
16-bit PCM as multimon expects.

## File → source page

| File | sigidwiki page |
|---|---|
| `AFSK1200_Sound.mp3` | [APRS](https://www.sigidwiki.com/wiki/Automatic_Packet_Reporting_System_(APRS)) |
| `Cw_morse.mp3` | [Morse Code (CW)](https://www.sigidwiki.com/wiki/Morse_Code_(CW)) |
| `EAS.mp3` / `EAS_Alert_Tornado_Warning.mp3` | [EAS](https://www.sigidwiki.com/wiki/Emergency_Alert_System_(EAS)) |
| `FLEX.mp3` | [FLEX](https://www.sigidwiki.com/wiki/FLEX) |
| `FLEX_2-LVL_1600_bps.mp3` | FLEX, 2-level 1600 bps variant |
| `Flex_3200.mp3` | FLEX, 4-level 3200 bps variant |
| `FLEX_6400bps.mp3` | FLEX, 4-level 6400 bps variant |
| `POCSAG_Sound.mp3` | [POCSAG](https://www.sigidwiki.com/wiki/POCSAG) — busy, mixed POCSAG/FLEX traffic |
| `POCSAG_512.mp3` | POCSAG @ 512 baud |
| `POCSAG_1200.mp3` | POCSAG @ 1200 baud (most common rate) |
| `POCSAG_2400.mp3` | POCSAG @ 2400 baud |
| `1200_variant.wav` | non-APRS 1200 baud AFSK variant — does not decode under standard AX.25 |

## License

Samples are user-contributed to sigidwiki under that site's terms. Kept
here for offline test purposes; if a sample's licence is unclear, use
the sigidwiki link to confirm before redistribution.
