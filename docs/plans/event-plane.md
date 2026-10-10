# Event plane: epoch, record log, embedded MQTT

Status: plan of record, 2026-10-10. Decisions agreed with Mick; findings
from a code + test deep dive (server, browser, MQTT ecosystem).

## The model

- **State is a set of kinds** in `server/src/decoder_store.rs`, each with a
  policy (`replace`, `append`, `upsert`/`expire`, `merge`) and one global
  `seq`. Nothing observable lives outside a kind.
- **Every change is one record** `{kind, seq, epoch, op, key?, at_ms, data}`.
  That record is the UI reducer input, the MQTT payload, the snapshot entry
  and the MCP response. One vocabulary.
- **`epoch`** is a store-wide integer bumped once per real topology commit.
  It rides on every record and every sample frame. Live param edits never
  bump it.
- **Three transports, deliberately:** REST = control (unchanged), MQTT =
  the record log + an ingest door, the frame bus = samples only.

## Findings that change the plan (from the deep dive)

1. **Decoder tables are wiped on every live param edit.**
   `PresetMount::live_reconfigure_block` (preset_pipeline.rs:113-115) always
   re-attaches sinks; `EventStore::attach_store` resets its kind by default.
   A VFO drag or `/api/tune` broadcasts `reset` for every decoder kind.
   Fix in phase 1 (only reset on a real rebuild, i.e. an epoch bump).
2. **Stream ids are positional and reused** (`env_split.rs`,
   `CROSS_ENV_STREAM_BASE = 1000`): id 1000 can flip from IQ to FFT across a
   preset swap with nothing on the wire. The epoch is the fix.
3. **Browser-side decoder events are lost.** `postEvents` →
   `client.injectLocal(sid, …)` and nothing subscribes; they never reach the
   store. Phase 3 (ingest) fixes this.
4. **Frames are postcard; `epoch` is a lockstep change** (server + both WASM
   builds from the same crate). Append it after `payload`.
5. **The UI assumes one global `seq` across kinds** (FT8/WSPR merge-sort,
   APRS/fldigi line ids). Keep the store's global seq; never per-topic.
6. **Other latent bugs found:** `transcript.lastSeq` never resets on
   snapshot; NR presets are a silent no-op under `audio_split=server`;
   `FrameClient` has no reconnect; client log lines appear twice; the
   `/api/decodes/:kind` route referenced by `event_store.rs` doesn't exist.
7. **MQTT building blocks:** `rumqttd 0.20.0` (in-process `Broker::link`
   → `LinkTx/LinkRx`, retained since 0.18, WS listener **v4 only**, no ACL,
   `Broker::start()` blocks → own OS thread, own port — cannot share axum's);
   `mqtt` (MQTT.js) 5.16.0 in the browser (worker-safe, `protocolVersion: 4`
   over WS, must echo subprotocol `mqtt`); **no mature MQTT MCP connector**
   exposes "collect N or timeout" — write `await` against `LinkRx::recv_deadline`.

## Topic tree

```
ferrite/state/<kind>            record stream (QoS 0). Replace kinds: retained whole doc.
ferrite/state/<kind>/<key>      keyed kinds: retained per entity; empty payload on expire.
ferrite/ingest/<kind>           anyone publishes raw events; server folds → state.
```
`graph`, `params/<block>`, `status`, `readback`, `signals`, `flowdiag`,
`rssi` are replace kinds. `jobs`, `adsb`, `aprs` keyed. `activity`,
`log/<target>`, `ai/<session>`, decodes, `transcribe` append. Expiry is in
the payload (`expires_at_ms`), not v5 properties (WS is v4).

## Phases

### Phase 1 — epoch + graph record + reset bug (server + WASM, old wire kept)
- `AppState::Inner.epoch: AtomicU64`; bump at the commit point of a real
  rebuild (`Runtime::reconfigure` commit / `spawn_preset` / `stop`), once.
  `live_reconfigure_block` bumps only if it fell back to a rebuild.
- `attach_bridge_sinks` resets kinds only when the epoch bumped (fixes #1).
- `Record` gains `epoch`; `StoreSnapshot` gains `epoch`; `apply`/`reset`/
  `expire` stamp it.
- New kinds: `graph` (replace: `{epoch, composed, browser_half, browser_half_hash,
  source}`), `params/<block>` (replace per block; written by
  `apply_block_params`, including browser-placed blocks → closes the
  browser live-edit gap), `status` (replace), `activity` (append; written by
  handlers; replaces the header-sniffing middleware), `jobs` (keyed; the
  capture registry publishes into it).
- `Frame::*` gains `epoch` appended after `payload`; `BroadcastSink::push`
  stamps it; WASM `decodeFrame` exposes it; runner drops frames whose epoch
  ≠ loaded; runner reloads from the `graph` record (hash compare) instead
  of the 8-GET refresh. `refreshFromServer` retired.
- Tests: decoder_store (epoch on records, reset-only-on-bump), bridge_sink +
  frame round-trips with epoch, preset_pipeline (live edit keeps tables),
  smoke (`/ws/preset` frames carry epoch; graph record on swap), runnerCore
  (epoch filter), wbfmE2E (no 8-GET refresh).

**Status (2026-10-10): shipped**, with these deltas from the text above:
- The epoch lives on `DecoderStore` (`Arc<AtomicU64>`, shared with
  `BroadcastSink`) rather than `AppState::Inner` — one counter, two
  stampers. Rebuild detection is `Runtime::generation()` before/after.
- `attach_bridge_sinks` resets only the kinds an `EventStore` in the new
  topology owns (that was always its contract); the fix is that the live
  path no longer calls it at all.
- `graph` is seeded at daemon start (before autostart) so a fresh
  `/ws/state` snapshot always carries one.
- Browser: `pipeline.graphEpoch` + `startGraphSync` (the only writer of
  `browserHalf` after init; hash-deduped); runner `load(doc, ws, epoch)` +
  `setEpoch` message move the gate without a reload; gate passes epoch 0
  (local injection / tests). `refreshFromServer` stays for the kinds that
  are not records yet (status, source, blocks, profile) — retired in Phase 2
  when those become records.
- `activity` as a store kind is deferred to Phase 2 (it goes with deleting
  the header-sniffing middleware and `ai_activity_layer`).

### Phase 2 — embedded broker + browser subscriptions
- `server/src/event_plane.rs`: `rumqttd` broker (TCP v4 `FERRITED_PORT+1`,
  WS `FERRITED_PORT+2`), started on its own thread; one in-process link
  publishes every store delta: replace kinds as retained whole docs, keyed
  kinds as retained per-entity + stream, append kinds stream-only.
  Coalesce to ≤10 Hz per kind (latest wins for replace). Republish retained
  state from the store at boot.
- Browser: `mqtt` (MQTT.js) client on the main thread; `decoders/store`
  subscribes `ferrite/state/#` (snapshot via `/api/state`, then records with
  seq > snapshot.seq; resync = snapshot again). Log sidebar subscribes
  `ferrite/state/log/#` and `activity`; the mute list becomes the
  subscription list. AI panel renders `ferrite/state/ai/<session>`.
- Vite: proxy `/mqtt` (ws) to the broker; prod: ferrited reverse-proxies the
  WS upgrade or documents the port. Subprotocol `mqtt` echoed.
- Delete: `/ws/state`, `/ws/logs`, chat-inject hub, `ai_activity_layer`.
- Tests: an MQTT client in server tests (rumqttc) asserting retained graph +
  decode records; web store tests with a fake MQTT client.

**Status (2026-10-10): shipped**, with these deltas:
- `server/src/event_plane.rs`: rumqttd 0.20 (`websocket` feature), TCP on
  HTTP+1, WS on HTTP+2 (`--mqtt-port` / `--mqtt-ws-port` / `--no-mqtt`;
  port 0 = free loopback port); `GET /api/hello` advertises both. The
  browser reaches the broker through ferrited's `GET /mqtt` reverse proxy
  (vite proxies `/mqtt` → ferrited), so one same-origin URL in dev and prod.
- Replace kinds coalesce leading-edge (first publish immediate, then ≤10 Hz,
  latest wins). Reset deltas are published *retained* on the kind topic and
  clear every retained entity the publisher has seen for that kind.
- Log lines ride `ferrite/state/log/<target>` straight from the tracing
  layer (structured `LogEntry` broadcast) — not folded into the store; the
  history ring behind `/api/decoder/recent` is unchanged. `rumqttd`'s own
  lines are kept off the bus.
- Ingest (Phase 3's server half) landed here: `ferrite/ingest/<kind>` →
  `store.apply`, stale `epoch` dropped.
- `activity` is a store kind written by the (kept) header-sniffing
  middleware — one place that knows the headers beats touching every
  handler; it still echoes an `ai::activity` tracing line for stdout. The
  AI panel's transcript and the page's refresh-on-activity read the kind.
- Browser: `web/src/lib/events/{bus,connect}.ts` (MQTT.js 5.16, one broker
  subscription `ferrite/state/#`, local filter routing, uncovered filters
  added broker-side only). Mirror dedups **per kind** (coalescing made the
  global watermark wrong). Log sidebar = `ferrite/state/log/#` subscriber.
- Deleted: `/ws/state`, `/ws/logs`. **Kept for now:** the chat-inject hub
  (`/api/ai/chat-inject` → `/ws/chat` fan-out): moving the AI panel onto
  `ferrite/state/ai/<session>` is a rewrite of the 877-line chat store and
  belongs with the sidecar-publishes-via-ingest work in Phase 3.
  `refreshFromServer` also stays until status/source/blocks/profile are
  records.
- Not verified live: the browser tab itself (no Chromium on `devel`). Verified
  live with MQTT.js from node: TCP retained `graph`, WS through vite →
  `/mqtt` → broker, `activity` + `log/ai/activity` on a CLI call, ingest →
  `state/pager`.

### Phase 3 — ingest
- Server link subscribes `ferrite/ingest/#`, folds into the store with
  epoch validation (drop stale), republishes on `state`.
- Runner worker publishes browser-side decoder events + transcript via its
  own MQTT.js connection (fixes #3). Sidecar sessions publish via ingest too.

### Phase 4 — MCP/CLI
- `state(kind, since)` and `await(kind, since, timeout, max)` tools over the
  in-process link (`recv_deadline`); CLI `tail` = the same subscription.
  Retire `recent_decodes`, `decodes`, `capture_jobs`, `capture_status`.
  Jobs/captures/activity visible via `jobs`/`activity` kinds.

**Status (2026-10-10): `await` shipped.** `await_events` MCP tool and
`ferrite-ctl await <kind> [--max N] [--wait S] [--since-seq] [--retained]
[--follow]` subscribe to `ferrite/state/<kind>/#` on the daemon's broker
over **TCP** (port from `GET /api/hello`), not an in-process link — the
CLI/MCP is a separate process, and a real client is the same path any
agent or `mosquitto_sub` takes. `decodes` already is `state(kind)`; the
retirements (`recent_decodes`, `capture_status`) and `tail`-over-MQTT for
log targets are left until the AI prompts stop referencing them.

### Phase 5 — resilience, survey, scan (separate plan).

### Phase 6 — persist the store across daemon restarts.

## Non-goals
- MQTT as a control plane. Commands stay REST, transactional, under
  `patch_gate`; each handler appends an `activity` record.
- Samples on MQTT. The frame bus stays.
- ACL/auth on the broker (LAN deployment); revisit with TLS.
