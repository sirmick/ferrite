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
- Six shipped presets in [`flowgraphs/`](../flowgraphs/): `wbfm`, `wbam`,
  `dtmf-e2e`, `fm-audio-record`, `am-audio-record`, `capture_fm`.

The full historical record of how we got here lives in
[`09-decisions.md`](09-decisions.md) (D01–D26) and the milestone tally in
[`10-commits.md`](10-commits.md).

## Forward work

Two parallel tracks, both rooted in concrete decisions already in the log:

- **Decoders** — sequencing in [`docs/decoder-roadmap/`](decoder-roadmap/).
  Phase 1 (analog listening: `AmDemod`, `SsbDemod`, `Deemphasis`, `Squelch`,
  `Agc`, `Resample` — see D20) is the next post-M5 push and feeds every
  later decoder chain.
- **UX** — the UX-1 cluster in [`10-commits.md`](10-commits.md):
  click-to-tune (D25), preset switching with retention (D26), sample-rate
  dropdown from `/api/source/capabilities`, generic receiver pane built on
  `<BlockParams>` (D24).

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
