# samples — test IQ captures for replay

Short, representative IQ recordings used to exercise the `FileIqSource`
block and smoke-test downstream DSP without live hardware.

## Playback

Run `ferrited` pointed at one of the WAVs:

    ferrited --source file://$PWD/samples/vhf/aprs_145.070mhz_iq-s16.wav --loop

The server auto-detects the format (RIFF/WAVE vs raw `cf32`) and, for
WAV files, picks up the sample rate from the header. Pass `--freq` to
override the displayed centre frequency — IQ files do not carry it.

## Filename convention

    <mode>_<center-freq>mhz_iq-<fmt>.wav

- `<mode>` — signal descriptor (`aprs`, `nbfm`, `ssb`, …)
- `<center-freq>` — centre frequency at which the capture was made
- `<fmt>` — underlying sample format: `s16` (16-bit int IQ, L=I R=Q),
  `f32` (32-bit float IQ), `cf32` for raw interleaved-float `.cf32`

Every capture gets a JSON sidecar (`<basename>.json`) with rate, centre
frequency, license, and upstream source. Keep it in lock-step with the
WAV — WAV headers do not carry centre freq or provenance.

## Inventory

| File | Band | Mode | Rate | Center |
|------|------|------|------|--------|
| `vhf/aprs_145.070mhz_iq-s16.wav` | VHF | APRS (AFSK over FM) | 39 062 Hz | 145.070 MHz |

## Licensing

Captures sourced from [sigidwiki](https://www.sigidwiki.com) are
redistributed under [CC-BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/).
See each `*.json` sidecar for per-file attribution. If you add a new
capture, record its upstream licence in the sidecar; do not commit
samples whose licence is unclear.
