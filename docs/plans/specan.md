# Spectrum analyzer tool (Panorama) — plan of record

First slice of scan/survey ([event-plane Phase 5](event-plane.md)). A
second **radio tool** next to the receiver: a stepped sweep across a
range wider than one span, stitched into a panorama trace + waterfall.

## Decisions (agreed 2026-10-14/15)

- **Tool registry in the UI.** `{id, icon, kind: radio|desk, sidebarTabs,
  QuickBar, Content, enter/exit}`. Receiver and Spectrum are the first
  two entries. Radio tools are exclusive and server-derived (every tab
  and the AI agree); desk tools (Inspector, Logbook, Schedule — later)
  are per-tab views over whatever radio tool runs.
- **Spectrum mode layout.** Toolbar unchanged except the Main/Right
  dropdowns hide; buttons become Tabler icons; a Receiver|Spectrum
  switch sits where the dropdowns were. Sidebar tabs: Bands, Settings,
  Logs, AI (Bands click = set sweep range; Settings = SDR input + the
  sweep block's params via `<BlockParams>`). Quick bar: start/stop (or
  centre/span) freq, RBW, dwell/averages, gain/antenna, est. pass time;
  right: trace mode (live/max/min/avg + clear), display range. Content:
  one area — trace over stitched waterfall. Click = marker, drag = zoom
  range, double-click = listen there (back to Receiver, tuned).
- **One mega block, not a flowgraph bypass.** `SweepSource` owns the hop
  loop and emits self-describing rows; `panorama.json` = `SweepSource →
  ui:panorama`. It shares SoapySource's device layer (open / reader /
  recovery / supervisor) rather than duplicating it. Source kind
  `stepped` vs `streaming`.
- **Step size is derived** (usable bandwidth − overlap), not a control.
- **Holds are per-tab display state**; MCP `sweep` gets its own
  server-side summary (last pass + max hold).
- **Lease v1 is minimal.** Entering Spectrum remembers the receiver
  preset; leaving restores it. While the panorama runs, tune / preset /
  source-patch calls return `409 RADIO_BUSY` unless `force` (the UI's
  own gestures preempt). Full job/priority framework comes with survey.
- **Units v1: dBFS.** RSPdx LNA ladder changes per band → floor steps
  at band edges; compensate via the true-dB readback tables if
  practical, else mark band edges.

## Spike results (2026-10-15, RSPdx on bigear via SoapyRemote)

`blocks/examples/{sweep,passband}_spike.rs`; carrier = KEZR 106.5 MHz on
antenna A, hop 105.5 ↔ 115.5 MHz, 10 hops per rate.

| Rate | Delivery (3 s) | `set_frequency` median/max | Stale after return median/max | Open → first sample |
|---|---|---|---|---|
| 2 MS/s | 100 %, 0 ovf | 32.5 / 37 ms | 61 / 82 ms | 447 ms |
| 6 MS/s | 100 %, 0 ovf | 18 / 30 ms | 33 / 40 ms | 393 ms |
| 8 MS/s | 100 %, 0 ovf | 19 / 24 ms | 35 / 43 ms | 397 ms |
| 10 MS/s | 100 %, 0 ovf | 23 / 29 ms | 33 / 45 ms | 391 ms |

Findings that shape the design:

1. **No hardware timestamps over SoapyRemote** — `time_ns` is 0 on every
   read. Stale-sample dropping must be by **sample count after
   `set_frequency` returns**: default discard 50 ms at ≥6 MS/s (max seen
   45), 90 ms at 2 MS/s. Make it a block param.
2. **No settling ramp.** The first block on the new LO is already at
   steady level (AGC off); discard covers it.
3. **Never change sample rate on an open stream.** A deactivated
   10 → 2 MS/s change **segfaulted SoapySDRServer** on bigear (systemd
   restarted it); 8 MS/s rate changes took 600 ms. Rate is fixed for a
   sweep's lifetime; a different rate = full re-open (~400 ms).
4. **Stream deactivate → activate on an open handle is ~1 ms** (and
   returns buffered data immediately) vs ~400 ms for a full re-open — so
   keeping the handle open across a panorama ↔ receiver switch is the
   win the shared device layer is for.
5. **10 MS/s sustains over gigabit** with zero overflows. devel's
   `net.core.rmem_max` caps the SoapyRemote socket buffer at 4 MiB (it
   asks for 42 MiB) — fine at 10 MS/s so far; raise in devel
   provisioning if overflows appear under load.
6. **Passband (noise floor on a quiet 115.5 MHz):** at 10 MS/s the IF
   filter is 8 MHz; the floor is within ~±2 dB over **±4.0 MHz (80 %)**,
   with a ~2–3 dB tilt low→high. At 8 MS/s usable is **±3.4 MHz (85 %)**,
   falling 14–22 dB past ±3.6 MHz. DC spur is small (+3–6 dB over 3 bins,
   ≈ ±4 kHz). A per-rate flatness calibration (noise-floor shape)
   removes the tilt.

**Defaults:** 10 MS/s, usable ±4.0 MHz, 5 % overlap → **7.6 MHz/step**;
per-step overhead ≈ 25 ms retune + 50 ms discard + dwell. With a
~13 ms dwell: **~90 ms/step** → FM band (88–108) ≈ 3 steps ≈ 0.3 s/pass;
0–2 GHz ≈ 260 steps ≈ 23 s/pass.

### Follow-up measurements (2026-10-15, step 1)

7. **Rate change on an open device is safe once the stream is
   *closed*** (dropped, not just deactivated): 9 switches 2.4 ↔ 10 MS/s,
   0.6–0.8 s each, no crash (`rate_switch_spike`).
8. **A live preset swap with a rate change already costs only
   325–420 ms over SoapyRemote** (`POST /api/preset` wbfm ↔ adsb,
   measured in the running daemon): the runtime builds the new source
   first and SoapySDR's factory hands back the same cached device by
   args, so there is no close → 1 s settle → reopen. The "2–8 s
   restart" figure in older comments is stale (or local-USB only).
   **⇒ No device-handle pool.** Step 1 became a pure extraction:
   `soapy_device::RadioHandle` owns open / configure / live radio params
   / readback / safe release; `SoapySource` and `SweepSource` both hold
   one.

## Row format (`ui:panorama`) — the backend/UI contract

One row per step:

```text
PanoramaRow {
  pass: u32,          // pass counter since start
  step: u32,          // index within the pass
  steps: u32,         // steps per pass (for progress)
  f_lo_hz: f64,       // first bin centre
  bin_hz: f64,        // bin spacing (= RBW-ish)
  power_db: Vec<f32>, // dBFS, already cropped (edges + DC masked = NaN)
  t_ms: u64,          // wall clock at capture
  range: (f64, f64),  // whole sweep range, so a late joiner can size
  gain_db: f32, antenna: String,
}
```

**Transport (revised):** rows go out on an `Events` port →
`ui:panorama` → `EventStore` → store kind `panorama`, **keyed by step**
(retained per key), so the event plane gives every tab, MCP
(`await_events` / state) and late joiners the *last full pass* for free.
`power_db` is base64 of little-endian `i16` centi-dB (`i16::MIN` =
masked) — ~9 KB/row at the default RBW, ≤ ~11 rows/s. `panorama` is a
live kind (never persisted). A max-hold summary for MCP is folded
server-side from the same rows.

## Build order

1. ~~Shared device layer~~ — done as the `RadioHandle` extraction (no
   pool; see finding 8).
2. ✅ `SweepSource` / `SweepSim` + `panorama.json` (bd27c77).
3. ◐ Lease v1 done (stash + `POST /api/preset/return`, tune 409
   `RADIO_BUSY` unless `force`); MCP `sweep` summary tool not yet —
   agents read the `panorama` kind via state / `await_events` today.
4. ✅ UI (e899cba): Tabler icons, tool registry, `SpecAnQuickBar`,
   Panorama (Canvas 2D, not WebGL — enough at ≤ 11 rows/s).
5. ◐ Live on the RSPdx ✓ (headless-Chromium walkthrough); full local CI
   matrix + merge pending.

### Known gaps / next

- **LNA-band floor steps** (RSPdx gain ladder changes at band edges —
  visible at ~50 and ~225 MHz in a 30–300 MHz sweep). Compensate from
  the true-dB readback tables, or mark the edges.
- **Detector choice**: peak-preserving reduction lifts the apparent
  floor when zoomed out (max of ~300 bins/pixel); add a sample/average
  detector option.
- **Listen picks the stashed preset** (e.g. `wbfm` even on NOAA) — pick
  a mode from the band plan instead.
- MCP `sweep` tool (pass summary + max hold), the three-surface test.
- The AGC checkbox still shows in the Spectrum quick bar (forced off by
  the preset; harmless but misleading).

## Emitter list (2026-10-15)

One model behind the receiver's Strongest Signals pane and the
analyzer's emitter panel: `server/src/emitters.rs` folds the `signals`
kind (receiver) and `panorama` rows (sweep) into numbered, persisted
emitters (store kind `emitter`). Frame detections merge first — by
200 kHz channel cell in broadcast FM (a fine-RBW detector splits a
station + HD sidebands into a dozen fragments), within 10 kHz elsewhere
(land-mobile neighbours stay separate); weak satellites ≥10 dB down fold
into a ≥100 kHz carrier without widening it; widths are smoothed (a
union only ever grows — a 2.4 MHz detection once became a black hole).
The browser's matcher mirrors the rules (`emitters/store.test.ts`). UI:
`EmitterTable` shared by both panes (number, MHz, level, SNR, width,
band / label, ✎ label, ⊘ ignore); numbered ticks on the panorama.
MCP: `emitters`, `label_emitter`. Next: `inspect`.

