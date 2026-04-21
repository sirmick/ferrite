# samples — test captures for replay

Short, representative recordings used to exercise Ferrite's replay
path and smoke-test downstream DSP without live hardware.

Two flavours live side-by-side:

- **IQ captures** (`vhf/`, `uhf/`) — raw complex baseband, replayed
  through `FileIqSource`. The full demod chain runs in the pipeline.
- **Audio captures** (`audio/`) — mono-PCM demodulated output from
  real broadcasts, captured through the shipped `fm-audio-record` /
  `am-audio-record` presets. Useful as "known good" targets when
  validating changes to the demod or decimator blocks.

## Playback

Run `ferrited` pointed at one of the WAVs:

    ferrited --source file://$PWD/samples/vhf/aprs_145.070mhz_iq-s16.wav --loop

The server auto-detects the format (RIFF/WAVE vs raw `cf32`) and, for
WAV files, picks up the sample rate from the header. Pass `--freq` to
override the displayed centre frequency — IQ files do not carry it.

## Filename convention

    <mode>_<center-freq>mhz_iq-<fmt>.wav       # IQ capture
    <mode>_<center-freq>mhz_audio-<fmt>.wav    # demodulated audio capture

- `<mode>` — signal descriptor (`aprs`, `nbfm`, `fm`, `am`, `ssb`, …)
- `<center-freq>` — centre frequency at which the capture was made
- `<fmt>` — sample format: `s16` (16-bit int; stereo L=I R=Q for IQ,
  mono for audio), `f32` (32-bit float IQ), `cf32` for raw
  interleaved-float `.cf32`

Every capture gets a JSON sidecar (`<basename>.json`) with rate, centre
frequency, license, and upstream source. Audio captures additionally
record the full signal chain so the result can be reproduced. Keep
sidecars in lock-step with the WAV — WAV headers do not carry centre
freq or provenance.

## Inventory

| File | Band | Kind | Mode | Rate | Center |
|------|------|------|------|------|--------|
| `vhf/aprs_145.070mhz_iq-s16.wav` | VHF | IQ | APRS (AFSK over FM) | 39 062 Hz | 145.070 MHz |
| `audio/fm_98.500mhz_audio-s16.wav` | FM bcast | audio | wideband FM (stereo pilot confirmed, 35.7 dB SNR) | 48 kHz | 98.500 MHz |
| `audio/am_0.810mhz_audio-s16.wav` | MW bcast | audio | AM (envelope detection) | 48 kHz | 0.810 MHz |

## Licensing

Captures sourced from [sigidwiki](https://www.sigidwiki.com) are
redistributed under [CC-BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/).
See each `*.json` sidecar for per-file attribution. If you add a new
capture, record its upstream licence in the sidecar; do not commit
samples whose licence is unclear.
