# WS-replicated decoder store

Replace the two disconnected decoder-output paths — the flat tracing **text ring**
(`decoder recent`, MCP) and the **per-decoder browser WS event stores** (the advanced
views) — with **one server-side `DecoderStore`**, mirrored 1:1 to the browser over WS,
with a REST snapshot for MCP/headless.

**Hard cut: no backward compat.** `EventStore` replaces `WsBridgeTxEvents` for every
events sink; the per-decoder browser stores are deleted, not bridged.

## Principles
- One store, **server-authoritative**; browser holds an identical **mirror** synced by WS deltas; **REST snapshot** for MCP.
- **Block owns the record schema; store owns the fold policy.**
- **Reset is first-class** per kind.

## Fold ops (3) + reset
- **Append** — bounded log (recent N): ft8, wspr, pager, eas, dtmf, morse, rtl_433, transcribe, fldigi.
- **Upsert(key) + TTL** — keyed table: adsb (`icao`), aprs (`call`), ais (`mmsi`).
- **Replace** — single current record holding the whole payload: rds, rssi, signals.
- **Reset(kind)** — clear recent+current, bump seq.

## Data model (POJO, serde)
```
DecoderStore { seq, kinds: Map<String, KindState> }
KindState    { policy, recent: VecDeque<Record>, current: BTreeMap<String,Record> }
Record       { seq, at_ms, key: Option<String>, data: Value }   // data = block JSON verbatim
Policy       { Append{cap} | Upsert{key_field,cap,ttl_ms} | Replace }
StateDelta   { kind, op: Add(Record) | Expire(key) | Replace(Record) | ResetKind, seq }
```
Global monotonic `seq` = the WS/REST delta cursor. Policy lives in a static Rust table keyed by kind.

## Kind catalog
| kind | fold | key | source block |
|------|------|-----|--------------|
| adsb | upsert+ttl(~60s) | icao | adsb / aircraft_spot |
| aprs | upsert (long ttl / session) | call | packet→aprs |
| ais | upsert+ttl | mmsi | ais |
| rds | replace | – | rds_demod |
| rssi | replace | – | rssi_probe |
| signals | replace (full snapshot) | – | signal_list |
| ft8 / wspr | append | – | ft8 / wspr |
| pager / eas / dtmf / morse / rtl_433 | append | – | pager / eas / dtmf_decoder / … |
| transcribe | append | – | voice_transcribe |
| fldigi | append | – | fldigi_modes |

## Components
**Backend**
- `server/src/decoder_store.rs` — `DecoderStore` (`Arc<RwLock>` in AppState, like the broadcast sink). `apply/reset/snapshot/expire_ttl`; `tokio::broadcast::Sender<StateDelta>`; static policy table.
- WS `State` frame (snapshot on connect + deltas) on the existing socket.
- REST `GET /api/decodes?since_seq=N` and `/api/decodes/:kind`.
- diag-tick writes `readback`/`flowdiag` into the store as kinds; TTL sweep on the tick. Retire `/api/source/readback` poll.
- `preset_pipeline.rs` attach-walk arm: `EventStore::attach_store`.

**Flowgraph**
- `blocks/src/event_store.rs` — `Placement::Either`, input Events, params `{kind, reset_on_init}`. native = direct store write; wasm = browser runtime drains + POST. Registered in `lib.rs`.
- `runtime/src/inject_event_store.rs` (or in env_split) — every `ui:<kind>` events sink gets an `EventStore` **instead of** `WsBridgeTxEvents`. FftU8/F32 sinks keep their bridges.
- Retire `WsBridgeTxEvents` once unused.

**Browser**
- `web/src/lib/decoders/store.svelte.ts` — mirror `DecoderStore`; applies snapshot + deltas. `kinds[kind].recent/current`.
- Migrate views; delete `adsb/aprs/ft8/rds/fldigi/transcribe/signals/rssi` stores; retire `startReadbackPoll` + the signals refcount-attach.

**MCP**
- `decodes <kind>` + `decodes reset` verbs over REST snapshot.

## Reset triggers
auto on `EventStore::init` (preset-load/rebuild) · live retune resets active kinds · TTL expiry (tables) · explicit MCP/UI.

## Execution (hard cut, staged to stay green)
- **P1** store + native EventStore + attach walk + env_split swap for **adsb** + REST + MCP `decodes adsb` + reset. Prove headless via ADS-B sample **replay**.
- **P2** WS `State` frame + browser mirror; repoint ADS-B view; delete adsb store.
- **P3** all remaining kinds; delete the other 7 stores; wasm EventStore POST path.
- **P4** fold readback/flowdiag/signals/rssi into the store; retire readback poll + transcribe content-sniff hack.
- **P5** delete WsBridgeTxEvents + dead bridge code + goldens.

## Open calls
- APRS TTL: long/session vs ADS-B/AIS short-ttl. (lean: APRS persist, adsb/ais expire)
- Keep `decoder recent` text ring for raw/debug, or retire once all kinds structured.
