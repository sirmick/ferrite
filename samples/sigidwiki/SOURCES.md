# sigidwiki sample corpus

Reference audio + IQ samples and waterfall images downloaded verbatim
from [sigidwiki.com](https://www.sigidwiki.com), the Signal
Identification Guide. Used for two things:

1. **e2e regression tests** — the IQ recordings (`AIS_IQ_5s.wav`,
   `AM_IQ_5s.wav`) and the digital-protocol audio recordings
   (`POCSAG_*.mp3`, `AFSK1200_Sound.mp3`, `EAS_*.mp3`) drive the
   `*_e2e.rs` tests under `blocks/tests/`.
2. **UI preview** — the "post-demod" audio recordings
   (`Nfm_voice.mp3`, `USB_Sound.mp3`, `AM_Sound.mp3`, `WFM.mp3`) and
   the waterfall images under `images/` are surfaced in the preset
   catalog so users can hear/see what a signal looks like before
   tuning.

Run `convert.py` to (re-)generate `22050_mono/*.wav` — 22 050 Hz mono
16-bit PCM as multimon expects.

## Audio + IQ files

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
| `Nfm_voice.mp3` | [NFM Voice](https://www.sigidwiki.com/wiki/NFM_Voice) — UI preview only (post-FmDemod audio, not IQ) |
| `USB_Sound.mp3` | [Single Sideband Voice](https://www.sigidwiki.com/wiki/Single_Sideband_Voice) — VOLMET aviation USB; UI preview only |
| `AM_Sound.mp3` | [AM](https://www.sigidwiki.com/wiki/Amplitude_Modulation_(AM)) — UI preview only |
| `WFM.mp3` | [FM Broadcast Radio](https://www.sigidwiki.com/wiki/FM_Broadcast_Radio) — UI preview only |
| `AIS_IQ_5s.wav` | [AIS](https://www.sigidwiki.com/wiki/Automatic_Identification_System_(AIS)) — first 5 s of `AIS IQ.zip` (sigidwiki/Cartoonman, 2016-01-04). 48 kHz stereo s16; despite the upstream filename, this is I/Q (left = I, right = Q) of a single AIS channel, *not* post-FmDemod audio. The `ais_e2e` test FmDemods it before feeding `AisDemod.ch_a`. Trimmed to 5 s (≈940 KB) so the repo doesn't carry the full 8 MB original; the window contains one decodable AIVDM frame. |
| `FT8_websdr_test.wav` | [FT8](https://www.sigidwiki.com/wiki/FT8) — 12 kHz mono s16 from [kgoba/ft8_lib's MIT-licensed test corpus](https://github.com/kgoba/ft8_lib/tree/master/test/wav) (`websdr_test7.wav`). One full 15-second slot of real off-air HF FT8, captured via WebSDR. The `ft8_e2e` test feeds it through `ferrite_ft8::Monitor`; ~30 messages decode reliably (callsigns from across Europe — DL / SP / EA / ON / OM / RA / etc.), making it a solid end-to-end fixture. |
| `AM_IQ_5s.wav` | [AM](https://www.sigidwiki.com/wiki/Amplitude_Modulation_(AM)) — first 5 s of `AM_IQ.zip` (sigidwiki). 64 kHz stereo u8; left = I, right = Q. Trimmed from the 61 s / 7.8 MB original to 5 s / 640 KB. Drives `am_e2e`. |

## Waterfall images (`images/`)

Representative spectrum / waterfall images, one per receiver preset.
Surfaced in the preset catalog. Filenames preserved from sigidwiki to
keep provenance obvious.

| File | sigidwiki page | Used by preset |
|---|---|---|
| `ADS-BTHMB.jpg` | ADS-B | `adsb` |
| `AIS_Waterfall.jpg` | AIS | `ais` |
| `AFSK1200_Waterfall.png` | APRS | `aprs` |
| `CW.jpg` | Morse Code | `cw` |
| `EAS.jpg` | EAS | `nwr` |
| `POCSAG_Waterfallthmb.png` | POCSAG | `pager` |
| `Nfm.jpg` | NFM Voice | `nbfm` |
| `USB_Waterfall.png` | Single Sideband Voice | `usb`, `lsb` (LSB shows the same wedge mirrored) |
| `FT8_Waterfall.png` | FT8 (rendered locally from `FT8_websdr_test.wav` via `tools/fft_to_png.py`-style spectrogram, not from sigidwiki) | `ft8` |
| `AM_Waterfall.jpg` | Amplitude Modulation | `wbam` |
| `Broadcast_FM.jpg` | FM Broadcast Radio | `wbfm` |
| `Stereo_left_right_fmbroadcast.jpg` | FM Broadcast Radio | `wbfm_stereo` |

## License

Samples are user-contributed to sigidwiki under that site's terms.
Kept here for offline test + preview purposes; if a sample's licence
is unclear, use the sigidwiki link on the matching page to confirm
before redistribution.
