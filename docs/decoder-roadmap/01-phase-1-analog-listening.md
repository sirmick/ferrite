# Phase 1 — Analog listening

**Status:** shipped (April 2026), with two intentional deviations from the
plan below.

What landed:

- All six listening modes ship as presets: `wbfm.json`, `wbam.json`,
  `nbfm.json`, `lsb.json`, `usb.json`, `cw.json`. Bonus: `wbfm_stereo.json`
  (stereo pilot decode) and `nwr.json` (NOAA weather, NBFM with EAS).
- Demod blocks: `AmDemod`, `FmDemod`, `SsbDemod` (Weaver), `StereoDecoder`,
  `RdsDemod` (Phase 5 territory but landed early — RDS is just a sidechannel
  on the WBFM audio).
- Helpers: `Squelch`, `RealF32Resamp`, `RssiProbe` + meter UI, `Decimator`,
  `Channelizer` (FreqShift + LPF + decimate, the universal narrowband tuner).
- Audio chain: `AudioNrMono` / `AudioNrStereo` bundles de-emphasis (the
  planned standalone `Deemphasis` block) plus four noise-reduction
  algorithms (spectral, neural, notch, blanker). One block instead of a
  chain — better packaging once we saw how presets composed.
- DTMF events-bridge canary: `DtmfAudioSource` → `AmModulator` →
  `Channelizer` → `Decimator` → `WsBridge` → `AmDemod` → `DtmfDecoder` →
  `EventsSink` (the `dtmf-e2e.json` preset). Proves the events bridge
  end-to-end across the server/browser split.

Deviations from this doc:

- **No standalone `Agc` block.** The hardware AGC (SoapySource's
  `agc: bool` param) covers the headline use case; audio-domain AGC didn't
  pull its weight as a separate block when the demod chains already do
  per-block normalisation. Revisit if a specific SSB workflow needs it.
- **`Deemphasis` collapsed into `AudioNr`.** Same algorithm (one-pole IIR
  with `tau_us`), different home. The audio chain is `Demod → Resamp →
  AudioNr` and AudioNr's params include `deemph_enable` + `deemph_tau_us`.

**Entry criterion (met):** M5 closed; runtime (native + WASM) + preset
loading + `FmDemod` end-to-end working against live hardware.
**Exit criterion (met):** a user can open Ferrite, pick a preset, tune,
and hear WBFM / NBFM / AM / USB / LSB / CW audio with proper de-emphasis
and squelch behaviour.

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

## Where these blocks run

Worth saying up-front because it's a recurring confusion point:

- **Block source is target-agnostic** (D19). Every block in this phase
  compiles to both native (linked into `ferrited`) and WASM (loaded by
  the browser). The same `.rs` file. The runtime decides placement per
  preset via per-block `env` hints + auto-inserted `WsBridge` pairs on
  environment-crossing wires.
- **Conventional placement for analog-listening presets:**
  - **Server-side (`ferrited`):** `SoapySource → Channelizer(narrowband
    VFO) → WsBridgeTx`. Heavy wideband work, device I/O.
  - **Browser-side (Worker):** `WsBridgeRx → FmDemod/AmDemod/SsbDemod →
    Deemphasis → Squelch → Agc → Resample → AudioSink`.
  - Narrowband demod is cheap, so browser-side is the default. Multiple
    browser clients can have their own demod mode selection off one
    shared wideband server stream.
- **Two browser threads are involved** — don't conflate them:
  - **Flowgraph Worker** (a Web Worker) hosts the Rust runtime WASM
    module. *All Phase 1 demod blocks run here.* Scheduler ticks, DSP
    math, block lifecycle.
  - **AudioWorklet** (audio rendering thread) only runs the 128-frame
    `process()` callback that reads PCM from a `SharedArrayBuffer`
    ring and hands it to the audio hardware. It does not run DSP
    blocks.
  - **`AudioSink` is the bridge between them** — writes into the SAB
    ring from the Worker side; the AudioWorklet reads from it.
- **Headless server use is a first-class mode** too. A preset that
  hosts the entire chain inside `ferrited` (no browser attached —
  e.g. record demodulated audio to disk on the SBC) runs every Phase 1
  block natively with the exact same source. Only `AudioSink` is
  browser-only; the headless case uses a `FileSink` or `NullSink`
  terminus instead.

This lets us write native Rust unit tests (`cargo test`) and browser
parity tests (`wasm-bindgen-test`) against exactly the same block
source. See `docs/05-testing.md`.

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

### `DtmfDecoder` — the end-of-phase digital canary (Rust, hand-rolled)

Added specifically to prove the `events` bridge end-to-end before Phase 2
layers on the C-vendor infrastructure. DTMF is the **easiest real digital
mode to implement**: eight Goertzel detectors (four row tones, four
column tones), threshold, pick the pair, emit a character. ~80 LOC.
Generating test DTMF is also trivial (two summed sines) — meaning the
entire headless e2e chain has no external dependencies.

- **Ports:** `real_f32` in (8 kHz or 22050 Hz — pick one) → `events` out.
- **Params:** `hold_ms` (minimum tone duration before emit), `off_ms`
  (minimum gap between emits of the same digit).
- **Event shape:**
  ```json
  { "kind": "dtmf", "digit": "5", "duration_ms": 200, "t_ms": 12345 }
  ```
- **~80 LOC Rust.** No vendor dependency, no FEC, no framing.

**Why Rust-from-scratch here and not wait for Phase 2's multimon-ng
`DtmfDecoder`:** Phase 2 validates the C-vendor path. This block
validates the Rust-native digital-decoder path *and* the `events`
bridge, before Phase 2 starts. When multimon-ng's DTMF ships in Phase
2, both blocks should produce identical event streams on the same
audio fixture — a nice parity test for the C-vendor tooling.

This is also the first demonstration of the Phase 5 bake-off principle
(Rust vs vendor): some modes are cheap enough in Rust that vendoring
is genuinely not worth it. DTMF is clearly on the Rust side of that
line.

## Events bridge — how digital events reach the browser main thread

New in this phase. Minimal design; grows later if needed.

**The chain:**

```
block emits on `events` port
        ↓
EventsSink (Rust block, browser or native)
        ↓
   target-specific delivery:
     - browser: self.postMessage(event) from the flowgraph Worker →
       main thread receives in worker.onmessage → console.log / UI hook
     - native: push onto a caller-provided mpsc::Sender<Event> →
       test code drains and asserts; ferrited CLI logs to stderr
```

**Design rules for the first cut:**

- **Events are JSON.** Each block serialises to `serde_json::Value` at
  its output port. `EventsSink` preserves the JSON as-is — no typed
  re-encoding. Keeps the bridge dumb; lets decoders ship new event
  kinds without runtime changes.
- **`postMessage` is the right primitive here**, not the SAB ring.
  Events are low-rate (tens per second worst case), not realtime
  (late-by-50ms doesn't matter). SAB is reserved for audio.
- **Events are structured-cloneable** — i.e. strings, numbers, arrays,
  objects only. No TypedArrays with transfer (those are for bulk bytes),
  no ArrayBuffer sharing. `postMessage(json_value)` works because the
  Rust side already produced a plain JS object via `serde-wasm-bindgen`.
- **No schema enforcement yet.** Each event has a `kind` discriminator
  string (`"dtmf"`, `"pocsag"`, …); main-thread consumer switches on
  that. Typed schemas can come later — this is a v0.x logging-sufficient
  baseline per the user's explicit ask.
- **Main thread side** is a ~20-line TS utility that subscribes to the
  runtime Worker's `message` channel, filters by `{ type: "event" }`
  messages, and dispatches to a log sink (browser console) + a future
  UI hook. Pluggable, no hard dep on any UI component.

**Flowgraph shape for the canary test:**

```
SignalSource(dtmf("1234"))  → DtmfDecoder  → EventsSink
       ↓ real_f32                ↓ events        ↓
    (samples)              (4 digit events)   (delivered)
```

Native (headless):
- `SignalSource` is Rust, generates the DTMF audio programmatically.
- `EventsSink` in native mode pushes into `mpsc::Sender<Event>` the
  test provides.
- Test asserts `"1234"` arrives in order, each with `duration_ms`
  matching the generator.

Browser (parity):
- Same flowgraph JSON, loaded by the browser runtime.
- `EventsSink` in browser mode posts to main thread.
- Playwright-style test asserts `console.log` sees the same four
  events in order.

**End-of-Phase-1 acceptance:** both tests pass. That's the proof the
`events` bridge works before Phase 2 loads it with multimon-ng's five
decoders.

**Lands early as a pre-Phase-1 E2E canary.** See `docs/10-commits.md`
"Pre-Phase-1 — DTMF events-bridge E2E canary" for the commit list.
The canary wires `DtmfAudioSource → AmModulator → Channelizer →
Decimator → WsBridge → AmDemod → DtmfDecoder → EventsSink` across the
server/browser split, so the events bridge is proven before the rest
of the Phase 1 demod blocks (`SsbDemod`, `Deemphasis`, `Squelch`,
`Agc`, `Resample`) get built against it.

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
