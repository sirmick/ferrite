// Browser mirror of the server-side `DecoderStore` — a 1:1 replica kept
// in sync over `/ws/decodes` (snapshot on connect, then deltas). Every
// decoder UI (APRS, ADS-B, FT8, signals, …) paints out of this one store
// instead of its own bespoke WS subscription. REST `/api/decodes` is the
// same data for MCP/headless.
//
// Wire protocol (text JSON, one message per line):
//   {"snapshot": { seq, kinds: { <kind>: { recent[], current{} } } }}
//   {"delta":    { kind, seq, op: "add"|"upsert"|"expire"|"reset", record?, key? }}
//   {"resync": true}   // broadcast lagged — re-fetch the REST snapshot

import { wsUrlFor } from '../api/errors';

/** One decoder record. `data` is the emitting block's JSON verbatim. */
export interface DecoderRecord {
  seq: number;
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

interface Snapshot {
  seq: number;
  kinds: Record<string, KindState>;
}
interface Delta {
  kind: string;
  seq: number;
  op: 'add' | 'upsert' | 'expire' | 'reset';
  record?: DecoderRecord;
  key?: string;
}

class DecoderMirror {
  /** Global monotonic store cursor (the dedup watermark). */
  seq = $state(0);
  /** All kinds, mirrored from the server. */
  kinds = $state<Record<string, KindState>>({});

  /** A kind's state, or an empty one — callers read `.recent`/`.current`. */
  kind(name: string): KindState {
    return this.kinds[name] ?? { recent: [], current: {} };
  }

  replaceSnapshot(snap: Snapshot): void {
    this.seq = snap.seq ?? 0;
    this.kinds = snap.kinds ?? {};
  }

  applyDelta(d: Delta): void {
    // Skip anything already covered by the snapshot / an older delta.
    if (d.seq <= this.seq) return;
    this.seq = d.seq;
    const k: KindState = this.kinds[d.kind]
      ? { recent: [...this.kinds[d.kind].recent], current: { ...this.kinds[d.kind].current } }
      : { recent: [], current: {} };
    switch (d.op) {
      case 'add':
        if (d.record) {
          k.recent.push(d.record);
          if (k.recent.length > LOG_CAP) k.recent.splice(0, k.recent.length - LOG_CAP);
        }
        break;
      case 'upsert':
        if (d.record) k.current[d.record.key ?? REPLACE_KEY] = d.record;
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
}

export const decoders = new DecoderMirror();

/** Connect the mirror to `/ws/decodes` with linear-backoff reconnect.
 *  Call once at app start (alongside the log stream). */
export function connectDecoders(): () => void {
  let ws: WebSocket | null = null;
  let closed = false;
  let retry = 0;
  const url = wsUrlFor('/ws/decodes');

  const open = () => {
    if (closed) return;
    ws = new WebSocket(url);
    ws.addEventListener('message', (e) => {
      if (typeof e.data !== 'string') return;
      let msg: { snapshot?: Snapshot; delta?: Delta; resync?: boolean };
      try {
        msg = JSON.parse(e.data);
      } catch {
        return;
      }
      if (msg.snapshot) decoders.replaceSnapshot(msg.snapshot);
      else if (msg.delta) decoders.applyDelta(msg.delta);
      else if (msg.resync) {
        // Broadcast lagged — re-fetch the authoritative snapshot.
        void fetch('/api/decodes')
          .then((r) => r.json())
          .then((s: Snapshot) => decoders.replaceSnapshot(s))
          .catch(() => {});
      }
    });
    ws.addEventListener('close', () => {
      if (closed) return;
      const delay = Math.min(1000 + retry * 500, 5000);
      retry += 1;
      setTimeout(open, delay);
    });
    ws.addEventListener('error', () => {
      // `close` fires next — reconnect there.
    });
    ws.addEventListener('open', () => {
      retry = 0;
    });
  };

  open();
  return () => {
    closed = true;
    ws?.close();
  };
}
