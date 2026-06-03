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
| ai | append | – | /ws/chat proxy tap + chat-inject hub |

**AI transcript** also lives in the store (kind `ai`, append log of conversation
turns) so it's server-side, survives a refresh, and is headless/MCP-visible via
`decodes ai` / `reset_decodes ai` (the kind-generic verbs — no new MCP surface).
It's not a flowgraph decoder: ferrited's `/ws/chat` reverse-proxy taps it — user
prompts on the browser→sidecar leg (`fold_ai_user_frame`), assistant text from
the sidecar's full end-of-turn `assistant` messages (`fold_ai_assistant_frame`).
The `chat-inject` hub (headless drivers) folds in `post_chat_inject`
(`fold_ai_inject_batch`) — done there, not in the per-connection proxy, so it's
recorded even with no tab open and the proxy's inject branch can't double-count.
Snapshots (`conversation_snapshot`) are deliberately *not* folded, so reconnects
don't re-log history. **DONE.**

Decision — the browser AI panel is **not** converted to a mirror adapter (unlike
the decoder views). It's a bidirectional *streaming* client: token-by-token
cursor, tool cards, and session/reset/snapshot semantics the sidecar owns
authoritatively. The store kind `ai` is the turn-granular server-side record for
MCP/headless read-back; the panel keeps streaming live from the sidecar (already
a faithful mirror of the authority). Gutting it would regress the chat UX for no
gain.

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
- **P1** store + native EventStore + attach walk + env_split swap for **adsb** + REST + MCP `decodes adsb` + reset. Prove headless via ADS-B sample **replay**. **DONE.**
- **P2** WS `State` frame + browser mirror; repoint ADS-B view; delete adsb store. **DONE.**
- **P3** all remaining kinds; delete the other 7 stores; wasm EventStore POST path. **DONE.**
- **P4** fold readback/flowdiag into the store (Replace kinds, diag tick now **4 Hz**); wire `expire()` (TTL sweep) onto the tick; repoint browser readback + flowdiag off their 1 Hz polls onto the mirror; migrate `rssi` to the mirror too. **DONE.**
- **P5** delete `WsBridgeTxEvents` + dead bridge code. **DONE.** (No goldens referenced it.)

### The "entire store" unification (P4/P5 outcome)
The store is now the single home for **all** live state — decoder kinds + `signals` + `ai` + `readback` + `flowdiag`. One read surface:
- REST `GET /api/state` → the whole store; `POST /api/state/:kind/reset`. WS `GET /ws/state` → snapshot + deltas (the browser mirror; `/api/state` is its resync fetch).
- The dedicated `/api/flowdiag`, `/api/source/readback`, `/api/signals` routes + their `AppState` RwLock caches are **deleted**. The diag tick writes straight into the store; on `stop()` it resets the `readback`/`flowdiag` kinds.
- **MCP slices one snapshot:** `state_op()` GETs `/api/state` once; `flowdiag` / `source_readback` / `signals` / `decodes` each parse their kind out of it (`store_current` helper; `signals` keeps its `max_age_ms` freshness gate via the record `at_ms`). No per-domain routes, no new MCP verbs.
- **MCP is pull-only** — no streaming; the WS feed is browser-only. (`GET /api/state?since_seq=N` delta-poll is a possible future add — store `seq` makes it cheap — but still polling, not push.)
- Browser: `pipeline` readback poll + `flowStore` flowdiag poll → `$effect`s over `decoders.kind('readback'|'flowdiag')`; deleted `api/flowdiag.ts` + `fetchSourceReadback`.

## Open calls
- APRS TTL: long/session vs ADS-B/AIS short-ttl. (lean: APRS persist, adsb/ais expire)
- Keep `decoder recent` text ring for raw/debug, or retire once all kinds structured.
