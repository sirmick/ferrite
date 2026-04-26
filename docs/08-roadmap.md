# 08 — Roadmap

## Where we are

Working today, end-to-end:

- `ferrited` boots, enumerates SoapySDR devices, caches per-device probes
  ([`server/src/device_cache.rs`](../server/src/device_cache.rs)).
- Preset-first server: one `FlowgraphDoc` + one `SourceConfig` on
  `AppState`, no per-session state. `--flowgraph PATH` is required.
- Rust runtime ([`runtime/`](../runtime/)) drives both ends. Native scheduler
  in `ferrited`; `RuntimeHandle` WASM facade in the browser
  ([`runtime/src/wasm.rs`](../runtime/src/wasm.rs)).
- Per-wire `TypedRing` accumulating buffers
  ([`runtime/src/typed_ring.rs`](../runtime/src/typed_ring.rs)) with a
  demand-driven scheduler that honours `Work.consumed`.
- Cross-env split: `split_for_environment` auto-inserts
  `WsBridgeTx*`/`WsBridgeRx` pairs and stamps `stream_id`s deterministically
  from `CROSS_ENV_STREAM_BASE = 1000`. `ui:<name>` sentinel sinks become
  Tx-only blocks; the browser learns the id via `GET /api/ui-sinks`.
- Source placeholder + `compose_source`
  ([`runtime/src/compose.rs`](../runtime/src/compose.rs)) — preset names
  `"type": "Source"`; `SourceConfig` overlays at load time.
- Postcard `Frame` transport on `/ws/preset`; encoder and decoder both come
  from `ferrite_blocks`, so wire-format drift is a build break.
- Browser runtime in a Worker
  ([`web/src/lib/runner/`](../web/src/lib/runner/)); SAB ring + AudioWorklet.
- Generic block-params pipe (D24): `GET /api/pipeline/blocks` +
  `POST /api/pipeline/blocks/:id/params`, mirrored on the WASM side by
  `RuntimeHandle::reconfigure_block`. One `<BlockParams>` Svelte component
  renders every block's params from its `BlockSpec`.
- Preset registry: `GET /api/presets` + `POST /api/preset` with hot reconfig.
- Nineteen shipped presets in [`flowgraphs/`](../flowgraphs/) covering
  analog listening (wbfm / wbfm_stereo / wbam / nbfm / lsb / usb / cw),
  data decoders (aprs / cw / nwr / pager / adsb), capture/record helpers
  (capture_fm / capture-aprs / capture-pager / fm-audio-record /
  am-audio-record), the dtmf-e2e canary, and an aprs-debug diagnostic.
- Four native vendor crates under [`blocks/native/`](../../blocks/native/):
  `liquid-dsp` (FIR / NCO / FEC primitives), `multimon-ng` (POCSAG / FLEX /
  AFSK / FSK9600 / Morse / EAS / DTMF — eleven `Decoder::*` variants),
  `dump1090` (Mode S / ADS-B), `libc-stubs` (WASM substrate).
- Logs panel category badges (`decoder::pocsag`, `decoder::flex`,
  `decoder::packet`, `decoder::adsb`, `decoder::cw`, `decoder::eas`,
  `flowdiag::node`, `flowdiag::browser`, …) — every tracing target is
  visible in stdout *and* the UI logs broadcast layer.
- Catalog vs. bands separation: catalog entries (`flowgraphs/*.json`) carry
  demod topology only; bands entries (`web/src/lib/presets/bands.json`)
  carry frequency + optional VFO offset. Per the UX refactor that
  superseded D26 — see D27.
- Spectrum click-to-tune (D25) wired through `Spectrum.svelte`: left-click
  retunes the source, double-click re-centres the SDR, right-click cancels.
- HTTPS dev server (`run.sh` flips `FERRITE_HTTPS=1`) for LAN/mobile
  testing where AudioWorklet + SharedArrayBuffer require a secure context.

The full historical record of how we got here lives in
[`09-decisions.md`](09-decisions.md) (D01–D27) and the milestone tally in
[`10-commits.md`](10-commits.md).

## Forward work

Three concrete tracks:

- **Phase 3 close-out** — `rtl_433` (200+ ISM device decoders), `AisDecoder`
  (marine), `mode_ac.c` follow-up to dump1090. See
  [`docs/decoder-roadmap/03-phase-3-aviation-aprs-ism.md`](decoder-roadmap/03-phase-3-aviation-aprs-ism.md).
- **Decoder UI** — structured outputs for ADS-B (aircraft table + map)
  and APRS (station list + map). The block-side hook is in place
  (`AdsbDemod` already maintains `Modes.aircrafts`); the UI side is
  greenfield.
- **UX-1 leftovers** — sample-rate dropdown driven by
  `/api/source/capabilities` (the endpoint exists; the web side still
  hard-codes choices). Tracked in [`10-commits.md`](10-commits.md).

The decoder-roadmap directory is the authoritative source for decoder-side
planning; this doc deliberately doesn't duplicate it.

## What's deferred

Captured in [`09-decisions.md`](09-decisions.md):

- **Multi-listener / multi-device** — D06.
- **Server-side recordings** — D08 (browser owns user state).
- **Authentication / remote access** — D07 (LAN-trust; user's tunnel
  problem).
- **Mobile UI** — desktop-first.
- **DMR / DSD** — D15 (AMBE patent encumbrance).
