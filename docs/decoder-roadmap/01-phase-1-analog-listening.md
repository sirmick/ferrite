# Phase 1 — Analog listening

**Status:** not started.
**Entry criterion:** M5 closed; runtime (native + WASM) + preset loading +
`FmDemod` end-to-end working against live hardware.
**Exit criterion:** a user can open Ferrite, pick a preset from a menu,
tune, and hear WBFM, NBFM, AM, USB, LSB, or CW audio with proper de-emphasis
/ squelch / AGC behaviour. No new C vendor work. All Rust.

## Why this is the first phase after M5

Before adding a single decoder, Ferrite needs the **small, composable
building blocks** that every later decoder chain will hang off. Every
multimon-ng, direwolf, or dump1090 preset starts with some subset of
"narrowband slice → FM/AM/SSB demod → resample → ...". Building those
five or six helper blocks well, once, makes every subsequent phase cheap.
Building them sloppily, under pressure, when we're also debugging the
first C vendor, is the failure mode.

This phase also turns Ferrite into a **useful receiver for a human** —
which matters for morale, demos, and user testing while the deeper
decoder work is underway.

## Blocks to add (Rust)

All live in `blocks/` next to the existing `FmDemod`. All dual-compile
(rlib + cdylib), all register via `#[ferrite_block]`.

### `AmDemod`

- **Ports:** `iq_f32` in → `real_f32` out.
- **Algorithm:** complex magnitude + DC blocker (one-pole high-pass).
- **Params:** `bandwidth` (runtime-updatable).
- **~40 LOC.** Already planned in `docs/03-blocks.md`; this phase lands it.

### `SsbDemod`

- **Ports:** `iq_f32` in → `real_f32` out.
- **Algorithm:** Weaver modulator (cleanest for the symmetric USB/LSB/CW
  case) or Hilbert-transform SSB. Pick Weaver — simpler, lower DSP cost.
- **Params:** `mode` (enum: `usb` / `lsb` / `cw`), `bandwidth`, `cw_tone_hz`
  (for CW only; drives BFO mixing frequency).
- **~120 LOC.** Includes the tiny BFO mixer for CW mode — don't make
  `BfoTone` a separate block, it's 5 lines inside `SsbDemod`.

### `Deemphasis`

- **Ports:** `real_f32` in → `real_f32` out.
- **Algorithm:** one-pole IIR, coefficient derived from `tau_us` and the
  port's sample rate (read from port metadata at `init()`).
- **Params:** `tau_us` (enum: `off`, `50`, `75`; runtime-updatable).
- **~25 LOC.**

### `Squelch`

- **Ports:** `real_f32` in → `real_f32` out.
- **Algorithm:** moving-RMS power estimate, hysteresis gate, attack/decay
  envelope. Don't hard-mute — smooth with a short attack (~20 ms) to avoid
  clicks.
- **Params:** `threshold_db`, `attack_ms`, `decay_ms`, `hysteresis_db`
  (all runtime-updatable).
- **~50 LOC.**

### `Agc`

- **Ports:** `real_f32` in → `real_f32` out (plus IQ variant `AgcIq` for
  SSB — AGC needs to track magnitude *before* demod for USB/LSB, otherwise
  it distorts). Start with the real-valued one; add `AgcIq` when SSB lands.
- **Algorithm:** feed-forward peak detector with configurable attack/decay,
  optional hang time.
- **Params:** `target_dbfs`, `attack_ms`, `decay_ms`, `max_gain_db`,
  `hang_ms`.
- **~70 LOC.**

### `Resample`

- **Ports:** `real_f32` in → `real_f32` out (and/or `iq_f32` variant).
- **Algorithm:** polyphase FIR resampler. Rate ratio derived from source
  port's sample rate metadata and the `target_rate` param.
- **Params:** `target_rate` (e.g. `48000`).
- **Notes:** can defer to `liquid-dsp::resamp_rrrf` in Phase 2 if
  substrate is in by then; for Phase 1, a clean hand-rolled polyphase is
  fine (~80 LOC) and worth keeping around regardless.

### `AudioSink` (browser-side)

- Already planned. Wraps the Web Audio API's `AudioWorklet`. Lives in the
  TS frontend as one of the few blocks that *don't* port to WASM (browser
  API binding). Server-side doesn't need it; server audio goes out via the
  WS audio stream to the browser.

## Preset flowgraphs to ship

One JSON file per preset under `flowgraphs/presets/`. These are the
user-visible "choose a receiver type" list. Each is small and
schema-validated (`docs/04-flowgraphs.md`).

```
flowgraphs/presets/
  wbfm.json           # WBFM broadcast, 200 kHz, 75µs de-emph, stereo pilot detect (later)
  nbfm.json           # 12.5 kHz, squelch on, no de-emph
  am.json             # 9 kHz, AGC on, for airband / SW BC
  usb.json            # 3 kHz, AGC-IQ, SSB upper
  lsb.json            # 3 kHz, AGC-IQ, SSB lower
  cw.json             # 500 Hz, SSB+BFO, AGC with slow decay
```

Each preset is a complete flowgraph: `SoapySource → Channelizer(vfo0) →
WsBridge → [browser] → Demod → Deemphasis/Squelch/Agc → Resample(48k) →
AudioSink`.

## Receivers pane

The UI receivers pane (landed in M5) already supports AM/FM selection.
Phase 1 expands this to the full list above. Selecting "SSB (USB)" is a
preset swap: tear down the current flowgraph, load `usb.json`, reconfigure
(the source block stays up if device settings are unchanged — that's what
M3's reconfigure event is for). Bandwidth slider wires to the `bandwidth`
runtime-updatable param of the demod block.

## Testing

- **Unit tests per block.** Use existing `blocks/tests/` pattern. For
  demods: golden IQ fixtures with known tones at known offsets, assert
  bit-exact output. For helpers: step responses and RMS/level assertions.
- **Integration test per preset.** Load the preset JSON, instantiate the
  full flowgraph, feed a recorded IQ file (a few seconds each), assert
  audio output has the expected spectral content.
- **Parity tests for WASM.** Same fixtures, same assertions, but built
  for `wasm32-unknown-unknown` and run in `wasm-bindgen-test`. This is
  the standing requirement from `docs/05-testing.md`; we just extend it
  to each new block.

## Commit-level plan

Rough granularity, to slot into `docs/10-commits.md`:

1. `feat(blocks): AmDemod — complex-magnitude demod with DC block`
2. `feat(blocks): SsbDemod — Weaver demod, USB/LSB/CW modes`
3. `feat(blocks): Deemphasis — one-pole IIR, selectable tau`
4. `feat(blocks): Squelch — RMS gate with hysteresis and envelope`
5. `feat(blocks): Agc — feed-forward with hang time`
6. `feat(blocks): AgcIq — complex-domain variant for SSB`
7. `feat(blocks): Resample — polyphase FIR, rate-metadata driven`
8. `feat(flowgraphs): analog-listen presets — wbfm/nbfm/am/usb/lsb/cw`
9. `feat(web): receiver picker expanded to full analog set; bandwidth slider`
10. `test(blocks): WASM-parity fixtures for all analog demod chains`

Each is a small, isolated commit. Expect ~2–3 days each for the blocks,
~1 week for the UI changes and preset wiring, ~1 week for tests.

## Risks and dependencies

- **M3 (reconfigure event) must be done** before runtime-updatable params
  like `bandwidth`, `squelch_threshold`, `agc_target` are wired up end-to-end.
  The demod blocks can *accept* these params via their JSON schema before
  M3 ships, but the UI→server propagation path depends on the reconfigure
  plumbing. If M3 slips, Phase 1 ships with commit-on-apply (full
  flowgraph reload) instead of live sliders — degraded UX, still functional.
- **Resampler correctness.** Polyphase resamplers are easy to implement
  with subtle bugs (off-by-one in commutator index, wrong anti-alias
  taps). Lean heavily on fixtures; don't trust ear-tests.
- **SSB group delay.** Weaver demod introduces some group delay; doesn't
  matter for voice but does for CW skimming. Note it, don't fix it this
  phase.

## What this phase deliberately doesn't do

- No stereo FM pilot / MPX decoding. Add in Phase 1.5 or bundle into a
  future "broadcast enhancements" phase. Getting mono WBFM right first.
- No `AudioSink` beyond the current browser implementation. Server-side
  audio stays as WS stream to browser.
- No decoders, no `events` output, no C vendor code. That's all Phase 2.
- No RDS (the FM-broadcast sidechannel). It's a Tier-2 decoder, handled
  in later phases; don't mix it into Phase 1 even though WBFM reception
  is a Phase 1 thing.
