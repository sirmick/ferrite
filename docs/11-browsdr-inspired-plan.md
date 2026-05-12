# 11 — BrowSDR-inspired follow-ups

Three feature ideas pulled from a session-end review of
[BrowSDR](https://github.com/jLynx/BrowSDR) (covered in
[rtl-sdr.com 2026-04-14](https://www.rtl-sdr.com/browsdr-turn-your-hackrf-or-rtl-sdr-into-a-browser-based-remote-websdr/)).
The architectural overlap with Ferrite is heavy (Rust + WASM DSP,
RustFFT, WebGL waterfall, Web Workers), so these features are
genuinely portable rather than apples-to-oranges. Kept here as
parking for future sessions; nothing committed to a milestone yet.

## 1. Audio transcription panel

**What.** Live audio-to-text of the currently-demodulated stream,
displayed next to the waterfall. Same idea BrowSDR ships under "AI
Transcription": a browser-side STT model running on the post-demod
PCM stream.

**Why.** Pairs naturally with the existing decoders — text output of
voice modes (AM/SSB/NFM) gives the same affordance that
`decoder::pocsag` / `decoder::cw` already give for digital modes. Lets
the operator monitor a slow-talking SSB net without sitting on the
audio.

**Sketch.**
- Drop a `whisper.cpp` WASM build or a hosted Whisper-quality model
  (`onnxruntime-web` + a tiny model) into `web/src/lib/audio/`.
- Tap the audio path the way the existing `AudioSink` block does — a
  new browser-side block (`AudioTranscribeStt`) that wires in
  parallel to the audio sink, accumulates ~5 s of PCM in a ring,
  runs the model on each chunk, emits text events that the chat /
  log surfaces.
- Push transcripts onto the existing `logs` store under
  `decoder::stt::voice` so the activity panel + chat already
  surface them with no new UI work.

**Open questions.**
- Browser-side model size — Whisper-small.en is ~485 MB, tiny is
  ~75 MB. Tiny is realistic; quality varies a lot on noisy HF SSB.
- VAD gating: only transcribe when squelch is open / audio level is
  above a threshold so we're not running Whisper on dead air.
- One stream at a time — Ferrite has one audio output today, so STT
  is naturally single-channel. Multi-VFO (item 3 below) would
  reopen this.

## 2. Frequency bookmarks with categories

**What.** Save the current `(center_freq_hz, freq_shift_hz, preset,
mode, antenna, gain)` tuple under a name + category, recall with one
click. Categories: free-form strings the user picks ("VOLMET",
"NOAA wx", "60m", "QSO 14.230").

**Why.** Pure UX win. Today the operator has to remember
`tune 14.230M`, switch to USB, drop bandwidth — every time. Genuinely
no decoder need; just a saved-state convenience. BrowSDR's
bookmark feature is the cleanest version of this I've seen — one
panel, drag to reorder, categories show as sections.

**Sketch.**
- New client-side state key `client.bookmarks` (a list of
  `Bookmark { id, name, category, source_config_snapshot, t_added }`).
  Lives in localStorage via the existing
  [`clientControls`](../web/src/lib/control/clientStore.svelte.ts)
  store — no server work.
- Panel: a new left-tab "Bookmarks" next to Bands / Catalog /
  Settings / Logs / Flow / AI. Add button captures current
  `pipeline.source` + `pipeline.flowgraph.name` as a snapshot. Click
  a row → PATCH `/api/source` with the snapshot's params + load the
  associated preset.
- Import/export: dump/load as JSON so users can share lists.
- Tag: optional `signal_wiki_url` to let the bookmark deep-link out.

**Open questions.**
- Snapshot scope — do we capture VFO offset, gain, antenna? Or just
  freq + preset (the minimal "tune here next time")? Probably both,
  with the granular fields optional at recall time so the user can
  swap an antenna across bookmarks without re-saving.
- Migration path if `source.params` shape changes — version the
  snapshot envelope.

## 3. Multi-VFO

**What.** Multiple simultaneous demodulators on the same source.
Operator monitors PSK31 on 14.070 *and* FT8 on 14.074 *and* CW on
14.010 from the same 2 MS/s channelizer window, with independent
demod / mode / volume / squelch per VFO. BrowSDR tested 62
simultaneous; a realistic Ferrite ceiling is probably 6–8 before
the audio-mix UX gets confusing.

**Why backend already supports it.** The `Channelizer` block already
does the per-VFO frequency translation (`freq_shift_hz` +
decimate-to-output-rate); the cross-env split already supports
fan-out (`TeeIqF32`). What's missing is (a) a multi-channelizer
preset pattern and (b) a UI that lets the operator declare the N
VFOs without hand-editing flowgraph JSON.

**Sketch (rough).**
- **Preset shape**: a new "VFO bank" pseudo-block declared once in
  the preset, expanded at compose time into N `Channelizer + Demod
  + AudioSink` triplets, one per active VFO. The bank's params
  carry a list of `{id, freq_shift_hz, mode, bandwidth_hz}` entries.
- **UI**: VFO list as a panel — `+ add VFO` button captures the
  current centre + offset, a click on the waterfall could
  drag-to-spawn a new VFO at that frequency. Each VFO row gets a
  mute / solo / mode selector.
- **Audio**: one `AudioSink` per VFO, mixed client-side into a
  single AudioContext destination. Independent per-VFO volume + mute
  fall out naturally from per-sink gain.
- **Waterfall markers**: extend the existing single-VFO orange line
  + bandwidth band overlay to render N of them. Already a `vfoCssPct`
  derived state in `Waterfall.svelte` — generalise to a list.

**Hard parts.**
- **Source-rate ceiling**: at 10 MS/s the channelizer runs at 10 MS/s
  per VFO (the decim happens *inside* the channelizer). N
  channelizers at 10 MS/s is N × ~Liquid-DSP-FIR-cost. Benchmark
  before promising "unlimited." 6–8 is probably the realistic upper
  bound on a modern laptop.
- **Audio routing**: mixing N audio streams with independent gain
  requires either N parallel AudioWorklets or a single AudioWorklet
  that pulls N SAB rings. Latter is faster but harder to spawn at
  runtime.
- **Preset diff hygiene**: the receivers-pane diff-plan
  optimisation (`wbfm_and_wbam_have_identical_source_blocks`) gets
  more complex when the preset has N parameter-defined demod chains
  instead of one. May need to extend `compose_source` to handle
  bank expansion.
- **State persistence**: today a single `freq_shift_hz` is part of
  the channelizer's params, persisted via the standard block-param
  mirror. N of them needs a list, which is a schema change.

**Suggested order if/when this gets picked up.**
1. **Static N=2** — hard-code a preset with two channelizer chains
   (call it `wbfm_dual` or similar), wire two AudioSinks, render
   two waterfall overlays. Proves the audio + UI plumbing in
   isolation from the dynamic-spawn problem.
2. **Dynamic add/remove** — generalise to a runtime-mutable VFO
   bank with the spawn / despawn paths.
3. **UX polish** — drag-to-spawn from waterfall, mute / solo,
   stereo-spread for monitoring multiple at once.

---

**Of the three, ordering by ROI / risk:**
1. **Bookmarks** — pure UX, no architectural risk, all client-side
   state. Probably a day's work.
2. **Audio transcription** — single new block + a model file, one
   panel. Few days; quality of HF SSB transcription is the unknown.
3. **Multi-VFO** — the biggest. Worth scoping a static `dual` preset
   first as a feasibility probe before committing to dynamic.
