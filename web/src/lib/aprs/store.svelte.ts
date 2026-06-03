// APRS view store — now a thin adapter over the decoder mirror.
//
// `PacketDemod`→aprs emits per-frame JSON (aprs.rs), wired to `ui:aprs` →
// the server `DecoderStore` (kind `aprs`, Upsert-by-call which also keeps
// a capped frame log in `recent`), mirrored over `/ws/state`. This
// keeps the original surface:
//   - `stations`    — latest frame per callsign, with last-known position
//                     + a heard-frame count (table + selection)
//   - `mapStations` — only those with a position (map markers)
//   - `lines`       — append-only console of every frame

import { decoders } from '$lib/decoders/store.svelte';
import type { MapStation } from '$lib/viz/StationMap.svelte';
import type { ConsoleLine } from '$lib/viz/TextConsole.svelte';

export interface AprsStation {
  id: string;
  kind: string;
  lat: number | null;
  lon: number | null;
  name: string | null;
  text: string;
  path: string;
  msgs: number;
  seenMs: number;
}

interface AprsRec {
  call?: string;
  kind?: string;
  lat?: number;
  lon?: number;
  name?: string;
  text?: string;
  path?: string;
  raw?: string;
}

const stationsD = $derived.by<AprsStation[]>(() => {
  const recent = decoders.kind('aprs').recent;
  const current = decoders.kind('aprs').current;
  // Last-known position + heard count per call, from the frame log.
  const lastPos = new Map<string, { lat: number; lon: number }>();
  const counts = new Map<string, number>();
  for (const r of recent) {
    const o = r.data as AprsRec;
    if (typeof o.call !== 'string') continue;
    counts.set(o.call, (counts.get(o.call) ?? 0) + 1);
    if (typeof o.lat === 'number' && typeof o.lon === 'number') {
      lastPos.set(o.call, { lat: o.lat, lon: o.lon });
    }
  }
  return Object.values(current).map((r) => {
    const o = r.data as AprsRec;
    const call = o.call ?? r.key ?? '';
    const here = typeof o.lat === 'number' && typeof o.lon === 'number';
    const pos = here ? { lat: o.lat as number, lon: o.lon as number } : (lastPos.get(call) ?? null);
    return {
      id: call,
      kind: typeof o.kind === 'string' ? o.kind : 'other',
      lat: pos?.lat ?? null,
      lon: pos?.lon ?? null,
      name: typeof o.name === 'string' ? o.name : null,
      text: typeof o.text === 'string' ? o.text : '',
      path: typeof o.path === 'string' ? o.path : '',
      msgs: counts.get(call) ?? 0,
      seenMs: r.at_ms,
    };
  });
});

const mapStationsD = $derived.by<MapStation[]>(() =>
  stationsD
    .filter((s) => s.lat !== null && s.lon !== null)
    .map((s) => ({
      id: s.id,
      call: s.name || s.id,
      lat: s.lat as number,
      lon: s.lon as number,
      kind: 'station' as const,
    })),
);

const linesD = $derived.by<ConsoleLine[]>(() =>
  decoders.kind('aprs').recent.map((r) => {
    const o = r.data as AprsRec;
    const call = o.call ?? r.key ?? '';
    const tone: ConsoleLine['tone'] =
      o.kind === 'message' ? 'accent' : o.kind === 'other' ? 'muted' : 'normal';
    return {
      id: String(r.seq),
      ts: Math.floor(r.at_ms / 1000),
      text: `${call}  ${o.raw || o.text || ''}`,
      tone,
    };
  }),
);

export const aprs = {
  get stations(): AprsStation[] {
    return stationsD;
  },
  get mapStations(): MapStation[] {
    return mapStationsD;
  },
  get lines(): ConsoleLine[] {
    return linesD;
  },
  attach(_client?: unknown, _streamId?: unknown): void {},
  detach(): void {},
  reset(): void {
    void fetch('/api/state/aprs/reset', { method: 'POST' }).catch(() => {});
  },
};
