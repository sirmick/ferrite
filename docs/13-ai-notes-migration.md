# 13 — AI notes migration

Refactor the AI sidecar's system prompt so that *what specific things
exist* (blocks, params, presets, SDR drivers) is sourced from each
artifact's own definition rather than hand-maintained inside
`tools/ferrite-ai/prompts/explorer.md`.

Goal: `explorer.md` shrinks to the SDR-agnostic core (pipeline lifecycle,
gain-calibration discipline, scan workflow patterns, DC-spike concept,
[REVIEW] flagging, style). Everything device- / block- / preset-specific
moves to where the thing lives.

## Why this matters

- Prompt and code drift: today the audio_nr stage list lives in
  `explorer.md` and the actual stages live in `blocks/src/audio_nr_*.rs`.
  Adding a stage means two edits; forgetting one rots the AI's mental
  model.
- AI doesn't pick the `ft8` preset for "scan for FT8 stations" because
  `explorer.md` never lists what decoders exist. Forcing the AI to grep
  the catalog each turn wastes turns; auto-injecting the catalog fixes
  it.
- SDRplay-isms (`rfgain_sel`, antenna A/B/C, `sdrplay_api_Update`
  errors) currently live in `explorer.md`, making the prompt
  non-portable when a different SDR is attached.

## Schema

### Rust — `BlockSpec` + `ParamSpec`

```rust
pub struct BlockSpec {
    pub type_name: &'static str,
    pub placement: Placement,
    pub inputs:  &'static [PortSpec],
    pub outputs: &'static [PortSpec],
    pub params:  &'static [ParamSpec],
    pub ai_notes: &'static str,          // NEW
}

pub struct ParamSpec {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: ParamKind,
    pub reconfig_scope: ReconfigureScope,
    pub ai_notes: &'static str,          // NEW
}
```

**Single field**, dual-purpose: AI prompt injection *and* UI param
tooltip. One source of truth — no `description` (UI) vs `ai_notes`
(AI) split. If the prose is too long for a tooltip, the tooltip
component clips. UI today has no per-param tooltips at all, so this is
also a small UI improvement.

**Length budget:**
- Block-level: ~50–100 words. *What this block does, when to use it,
  one key gotcha.*
- Param-level: 1–2 sentences. *What the knob controls, useful range,
  what it interacts with.*

### Flowgraph JSON — `ai_notes`

```json
{
  "name": "wbam",
  "label": "AM broadcast (wideband)",
  "description": "AM broadcast 540-1700 kHz...",
  "ai_notes": "Listen preset for AM broadcast + shortwave SWL. Pins agc_enable=false via force_params (envelope chasing causes audible breathing). Full audio_nr stack on by default. Best with the high-Z HF antenna port on multi-port SDRs. No decoder output — this is a listen preset, not a decode preset.",
  "environments": [...],
  "blocks": {...}
}
```

Length budget: 3–6 sentences.

### Driver JSON — already shipped

`web/src/lib/controls/sdr-presets/*.json` already has
`ai_operator_notes`. No schema change; we just move SDRplay-isms out of
`explorer.md` into `sdrplay.json`'s field (Phase 2 below).

## Injection order in the system prompt

Per turn, the sidecar concatenates:

```
1. explorer.md base                                 (SDR-agnostic, stable)
2. Active SDR — driver-specific operator notes      (sdrplay.json ai_operator_notes)
3. Active preset — AI notes                         (flowgraphs/<name>.json ai_notes)
4. Active blocks — AI notes per block + params      (joined: /api/pipeline/blocks × /api/blocks)
5. Operator-supplied radio setup                    (existing; user-supplied)
6. Local clock + region                             (existing)
```

Layers 3 and 4 refresh whenever the preset changes. The UI is the
source of "what's currently loaded" and pushes layers 2–4 to the
sidecar in each WS turn message — the same channel `driver_notes`
already uses (`tools/ferrite-ai/index.ts:appendPromptExtras`).

Prompt-caching makes the per-turn cost of the largeish active-blocks
section negligible after the first turn; we're not token-constrained.

## Enforcement

A unit test on every commit asserts the schema is complete:

```rust
#[test]
fn every_block_has_ai_notes() {
    for entry in registry::iter() {
        let s = entry.spec();
        assert!(!s.ai_notes.is_empty(),
                "block {} missing ai_notes", s.type_name);
        assert!(s.ai_notes.split_whitespace().count() >= 10,
                "block {} ai_notes too terse — write at least 10 words",
                s.type_name);
        for p in s.params {
            assert!(!p.ai_notes.is_empty(),
                    "param {}::{} missing ai_notes", s.type_name, p.key);
        }
    }
}
```

Equivalent Vitest test reads every `flowgraphs/*.json` and asserts a
non-empty `ai_notes` field. Both tests gate CI.

No "user-facing" carve-out: utility blocks (`TeeIqF32`, `LogMagU8`,
`FFT`) get one-sentence notes too. Discipline beats judgement calls.

## Discipline — what goes where

Avoid duplication across the four layers:

| Layer    | Answers the question                                          |
| -------- | ------------------------------------------------------------- |
| Driver   | *How do I drive this SDR?* Antenna ports, LNA stages, notches |
| Preset   | *When would I pick this preset? What signal does it expect?*  |
| Block    | *What does this block do? When does it appear in a chain?*    |
| Param    | *What does this knob control? Useful range?*                  |

When a preset note duplicates a block note, lint at review time and
cross-reference instead ("this preset includes `audio_nr` — see its
block notes for tuning").

## Rollout — four phases, each independently shippable

### Phase 1 — schema + plumbing (no behaviour change)

- Add `ai_notes: &'static str` to `BlockSpec` and `ParamSpec` with
  empty-string defaults everywhere.
- Add `ai_notes` field to flowgraph JSON schema; default empty.
- Wire `/api/blocks` and `/api/pipeline/blocks` responses to include
  the new fields.
- Extend `tools/ferrite-ai/index.ts` `appendPromptExtras` to accept and
  inject `preset_notes` + `block_notes` sections.
- Extend the WS message shape (UI → sidecar) with `preset_notes` and
  `block_notes` fields, populated by reading the active preset's JSON
  and joining `/api/pipeline/blocks` against `/api/blocks`.
- *Don't* enable the enforcement tests yet (everything is empty).

Output: zero observable change to AI behaviour; injection plumbing is
in place and idle.

### Phase 2 — extract SDRplay-isms from `explorer.md`

- Move into `sdrplay.json` `ai_operator_notes`: the `{2,4,6,8,10}` MS/s
  rate list, antenna A/B/C descriptions, `rfgain_sel` semantics, the
  "Reading driver warnings" section's SDRplay-specific messages,
  Zero-IF aside.
- Generalise the scan workflow ("Sweep antenna (A/B/C) and gain
  (15/30/45 dB)" → "sweep through each antenna port the driver
  exposes; sweep gain 15/30/45 dB"). Leave the byte-range thresholds
  (those are Ferrite's u8 FFT bytes, SDR-agnostic).
- The audio_nr stage prose stays in `explorer.md` for now — it'll move
  to the block notes in Phase 3.

Output: `explorer.md` is SDR-agnostic; SDRplay-specific knowledge lives
in one place; swapping in a different SDR driver only requires
authoring that driver's `ai_operator_notes`.

### Phase 3 — backfill block + param `ai_notes`

One PR per ~5 blocks, reviewer-sized. Suggested order (highest-impact
first):

1. Source blocks — `SineSource`, `SoapySource`, `FileSource`,
   `DtmfAudioSource`, `MorseAudioSource`
2. Channelizer + utility — `Channelizer`, `TeeIqF32`, `RealF32Resamp`,
   `Decimator`
3. Demods — `FmDemod`, `AmDemod`, `SsbDemod`, `StereoDecoder`
4. Audio chain — `AudioNrMono`, `AudioNrStereo`, `AudioSink`
5. Visualisation + sinks — `FFT`, `LogMagU8`, `FileIqSink`,
   `FileAudioSink`, `EventsSink`, `RssiProbe`
6. Decoders — `AdsbDemod`, `AisDemod`, `AprsDemod`, `PocsagDemod`,
   `PagerDemod`, `EasDemod`, `Ft8Demod`, `CwDemod`, `RdsDemod`,
   `MorseDemod`, `DtmfDecoder`, `PacketDemod`

When the audio_nr block notes land, *delete* the equivalent prose from
`explorer.md` in the same PR (one source of truth).

Final PR in the phase: flip the enforcement test from `#[ignore]` to
active.

### Phase 4 — backfill flowgraph `ai_notes`

One PR per ~3 presets. Suggested order:

1. Listen presets — `wbfm`, `wbfm_stereo`, `nbfm`, `wbam`, `usb`,
   `lsb`, `nwr`
2. Decoder presets — `ft8`, `packet`, `adsb`, `ais`, `nbfm` (paging
   variant), `pager`, `cw`, `dtmf-e2e`, `morse-e2e`, `eas`
3. Capture / record presets — `capture_fm`, `capture-aprs`,
   `capture-pager`, `am-audio-record`, `fm-audio-record`

Final PR in the phase: flip the Vitest enforcement test active.

## What `explorer.md` looks like after migration

- Pipeline lifecycle (don't stop)
- First moves (`status`)
- Sample-rate strategy *concept* — wide-first, then zoom (specific
  rates → driver notes)
- Operator rules — AGC/manual exclusion, the gain calibration LOOP
- DC spike concept + channelizer-VFO pattern (general zero-IF)
- Antenna selection mechanics (the API call); *which* antenna covers
  what → driver notes
- How captured FFTs work (PNG axes, sidecar duration, fft_peaks)
- Workflow patterns ("what's at freq", "decode this", "scan range")
- [REVIEW] flagging
- Style + CLI reference

Target length: ~50 % of today's file.

## Known risks / pitfalls

- **Authoring fatigue.** ~30 blocks × careful notes is 2-3 weeks of
  part-time work. Phasing per-PR keeps each batch small.
- **Style drift across authors.** Mitigation: write the first 5 blocks
  carefully as templates, link them from `CONTRIBUTING.md`'s new
  "Block authoring" section.
- **Cross-layer duplication.** Mitigation: the "Discipline" table
  above; reviewer checks for it.
- **UI churn.** Adding `ai_notes` to ParamSpec changes the
  `/api/blocks` response shape. The UI already consumes this — adding
  a field is backward-compatible, but the param-tooltip rendering work
  (showing `ai_notes` on hover over a control) is a separate small UI
  PR that lands whenever convenient.

## Related work — `gain-check` CLI

Parallel to the prompt refactor, add `ferrite-ctl gain-check` that
captures briefly, runs the same logic as `fft_peaks.py`, and prints a
verdict line the AI can act on directly:

```
gain assessment (18.000 MHz, 2.0 MS/s, gain=30, agc=off):
  ADC clipping: NO  (max byte 187)
  noise floor:  byte 18
  carriers > 3σ: 0
  verdict: GAIN TOO LOW
  next: ferrite-ctl tune 18.000M --gain 40   # +10 dB
        (if still nothing at gain=60, see driver notes for LNA controls)
```

Cheaper for the AI than assembling `fft_peaks` output into a
verdict. SDR-agnostic; when at max IFGR with no signal, it points the
AI back at the driver notes for LNA-stage controls.

Ship after Phase 1 (so the AI prompt is stable while the tool lands)
or in parallel — independent surfaces.
