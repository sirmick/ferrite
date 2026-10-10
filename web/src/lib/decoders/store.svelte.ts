// Browser mirror of the server-side store — a 1:1 replica kept in sync
// over the event bus (`ferrite/state/<kind>` topics; snapshot via REST
// `/api/state` on every connect). Every live-state UI (APRS, ADS-B,
// FT8, signals, flowdiag, readback, graph, activity, …) paints out of
// this one store instead of its own bespoke subscription or poll.
// `/api/state` is the same data for MCP/headless.
//
// Record vocabulary (one message per change, the same JSON the server
// broadcasts internally):
//   {"kind", "seq", "epoch", "op": "add"|"upsert"|"expire"|"reset", "record"?, "key"?}
// Replace kinds are retained on `ferrite/state/<kind>`; keyed entities
// on `ferrite/state/<kind>/<key>` (empty payload = cleared); append
// kinds stream only, so history comes from the snapshot.
//
// Dedup is per kind: the server coalesces replace-kind bursts, so a
// `readback` record can arrive after a `pager` record with a higher
// seq. A global watermark would drop it. The snapshot's seq is the
// floor for every kind it covers.

import { eventBus, type BusMessage, type EventBus } from '$lib/events/bus';

/** One decoder record. `data` is the emitting block's JSON verbatim. */
export interface DecoderRecord {
  seq: number;
  epoch?: number;
  at_ms: number;
  key?: string;
  data: unknown;
}

/** Per-kind state: `recent` append log + `current` keyed table. */
export interface KindState {
  recent: DecoderRecord[];
  current: Record<string, DecoderRecord>;
}

const REPLACE_KEY = 'current';
const LOG_CAP = 512;
/** Per-kind overrides of the server's append caps (decoder_store.rs). */
const CAPS: Record<string, number> = { ai: 1500 };
const capFor = (kind: string): number => CAPS[kind] ?? LOG_CAP;
export const STATE_TOPIC_PREFIX = 'ferrite/state/';

export interface Snapshot {
  seq: number;
  epoch?: number;
  kinds: Record<string, KindState>;
}
export interface Delta {
  kind: string;
  seq: number;
  epoch?: number;
  op: 'add' | 'upsert' | 'expire' | 'reset';
  record?: DecoderRecord;
  key?: string;
}

export class DecoderMirror {
  /** Highest store cursor seen (snapshot or delta). */
  seq = $state(0);
  /** Graph epoch of the last snapshot / delta. */
  epoch = $state(0);
  /** All kinds, mirrored from the server. */
  kinds = $state<Record<string, KindState>>({});
  /** Per-kind watermark; kinds absent here fall back to `snapshotSeq`. */
  private kindSeq: Record<string, number> = {};
  private snapshotSeq = 0;

  /** A kind's state, or an empty one — callers read `.recent`/`.current`. */
  kind(name: string): KindState {
    return this.kinds[name] ?? { recent: [], current: {} };
  }

  replaceSnapshot(snap: Snapshot): void {
    this.snapshotSeq = snap.seq ?? 0;
    this.seq = this.snapshotSeq;
    if (typeof snap.epoch === 'number') this.epoch = snap.epoch;
    this.kindSeq = {};
    this.kinds = snap.kinds ?? {};
  }

  applyDelta(d: Delta): void {
    // Skip anything already covered by the snapshot / an older delta of
    // the same kind.
    const floor = this.kindSeq[d.kind] ?? this.snapshotSeq;
    if (d.seq <= floor) return;
    this.kindSeq[d.kind] = d.seq;
    if (d.seq > this.seq) this.seq = d.seq;
    if (typeof d.epoch === 'number' && d.epoch > this.epoch) this.epoch = d.epoch;
    const k: KindState = this.kinds[d.kind]
      ? { recent: [...this.kinds[d.kind].recent], current: { ...this.kinds[d.kind].current } }
      : { recent: [], current: {} };
    switch (d.op) {
      case 'add':
        if (d.record) {
          k.recent.push(d.record);
          if (k.recent.length > capFor(d.kind))
            k.recent.splice(0, k.recent.length - capFor(d.kind));
        }
        break;
      case 'upsert':
        if (d.record) {
          k.current[d.record.key ?? REPLACE_KEY] = d.record;
          // Tables also keep a capped frame log (matches the server) —
          // unless this is a Replace/Merge kind (no real key), which only
          // ever wants the single current record.
          if (d.record.key !== undefined) {
            k.recent.push(d.record);
            if (k.recent.length > capFor(d.kind))
              k.recent.splice(0, k.recent.length - capFor(d.kind));
          }
        }
        break;
      case 'expire':
        if (d.key) delete k.current[d.key];
        break;
      case 'reset':
        k.recent = [];
        k.current = {};
        break;
    }
    this.kinds = { ...this.kinds, [d.kind]: k };
  }

  /** One bus message → one delta. Log topics belong to the log store
   *  and are ignored here; an empty payload is a cleared retained
   *  entity (its `expire` delta already streamed on the kind topic). */
  ingest(m: BusMessage): void {
    if (!m.topic.startsWith(STATE_TOPIC_PREFIX)) return;
    if (m.topic.startsWith(`${STATE_TOPIC_PREFIX}log/`)) return;
    if (m.payload.length === 0) return;
    let d: Delta;
    try {
      d = JSON.parse(m.payload) as Delta;
    } catch {
      return;
    }
    if (typeof d?.kind !== 'string' || typeof d.seq !== 'number') return;
    this.applyDelta(d);
  }
}

export const decoders = new DecoderMirror();

/** Fetch the authoritative snapshot. Exposed for tests to stub. */
export async function fetchSnapshot(): Promise<Snapshot> {
  const r = await fetch('/api/state');
  if (!r.ok) throw new Error(`/api/state ${r.status}`);
  return (await r.json()) as Snapshot;
}

/** Wire the mirror to the event bus. On every (re)connect: subscribe
 *  first, buffer what arrives, pull `/api/state`, then replay the
 *  buffer — per-kind dedup drops anything the snapshot already covers.
 *  Call once at app start. */
export function connectDecoders(
  bus: EventBus = eventBus,
  snapshot: () => Promise<Snapshot> = fetchSnapshot,
  mirror: DecoderMirror = decoders,
): () => void {
  let pending: BusMessage[] | null = [];
  const unsub = bus.subscribe(`${STATE_TOPIC_PREFIX}#`, (m) => {
    if (pending) pending.push(m);
    else mirror.ingest(m);
  });
  let generation = 0;
  const offConnect = bus.onConnect(() => {
    const mine = ++generation;
    pending = [];
    void snapshot()
      .then((s) => {
        if (mine !== generation) return; // a newer connect superseded us
        mirror.replaceSnapshot(s);
        const buffered = pending ?? [];
        pending = null;
        for (const m of buffered) mirror.ingest(m);
      })
      .catch(() => {
        // Keep buffering until the next connect; the retry is the
        // next reconnect's snapshot (MQTT.js reconnects on its own).
        if (mine === generation) pending = pending ?? [];
      });
  });
  return () => {
    offConnect();
    unsub();
  };
}
