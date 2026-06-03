// ADS-B view store — now a thin adapter over the decoder mirror.
//
// `AdsbDemod`/aircraft_spot emit a ~1 Hz per-aircraft snapshot
// (aircraft_spot.rs), wired to `ui:adsb` → the server `DecoderStore`
// (kind `adsb`, Upsert-by-ICAO which also keeps a capped frame log in
// `recent`), mirrored over `/ws/decodes`. This keeps the original
// surface: `aircraft` (table), `stations` (map markers), `trails` (recent
// flight paths). Staleness pruning runs locally off each record's
// timestamp (the server's TTL expiry lands in P4).

import { decoders } from '$lib/decoders/store.svelte';
import type { MapStation, MapTrail } from '$lib/viz/StationMap.svelte';

export interface Aircraft {
  id: string;
  flight: string;
  lat: number | null;
  lon: number | null;
  alt: number;
  gs: number;
  trk: number;
  msgs: number;
  age: number;
  seenMs: number;
}

interface AdsbRec {
  icao?: string;
  flight?: string;
  lat?: number;
  lon?: number;
  alt?: number;
  gs?: number;
  trk?: number;
  msgs?: number;
  age?: number;
}

// Drop an aircraft absent from the snapshot this long; trail ring length.
const STALE_MS = 60_000;
const TRAIL_MAX = 60;

const aircraftD = $derived.by<Aircraft[]>(() => {
  const now = Date.now();
  return Object.values(decoders.kind('adsb').current)
    .filter((r) => now - r.at_ms < STALE_MS)
    .map((r) => {
      const o = r.data as AdsbRec;
      return {
        id: o.icao ?? r.key ?? '',
        flight: typeof o.flight === 'string' ? o.flight : '',
        lat: typeof o.lat === 'number' ? o.lat : null,
        lon: typeof o.lon === 'number' ? o.lon : null,
        alt: typeof o.alt === 'number' ? o.alt : 0,
        gs: typeof o.gs === 'number' ? o.gs : 0,
        trk: typeof o.trk === 'number' ? o.trk : 0,
        msgs: typeof o.msgs === 'number' ? o.msgs : 0,
        age: typeof o.age === 'number' ? o.age : 0,
        seenMs: r.at_ms,
      };
    });
});

const stationsD = $derived.by<MapStation[]>(() =>
  aircraftD
    .filter((a) => a.lat !== null && a.lon !== null)
    .map((a) => ({
      id: a.id,
      call: a.flight || a.id,
      lat: a.lat as number,
      lon: a.lon as number,
      kind: 'aircraft' as const,
      heading: a.trk,
    })),
);

// Flight paths for still-live aircraft, built from the `recent` frame log
// grouped by ICAO (consecutive-dupe-filtered, last TRAIL_MAX fixes).
const trailsD = $derived.by<MapTrail[]>(() => {
  const live = new Set(aircraftD.map((a) => a.id));
  const byId = new Map<string, [number, number][]>();
  for (const r of decoders.kind('adsb').recent) {
    const o = r.data as AdsbRec;
    const id = o.icao ?? r.key ?? '';
    if (!live.has(id) || typeof o.lat !== 'number' || typeof o.lon !== 'number') continue;
    const ring = byId.get(id) ?? [];
    const last = ring[ring.length - 1];
    if (!last || last[0] !== o.lon || last[1] !== o.lat) {
      ring.push([o.lon, o.lat]);
      if (ring.length > TRAIL_MAX) ring.shift();
    }
    byId.set(id, ring);
  }
  const out: MapTrail[] = [];
  for (const [id, p] of byId) if (p.length >= 2) out.push({ id, path: p });
  return out;
});

export const adsb = {
  get aircraft(): Aircraft[] {
    return aircraftD;
  },
  get stations(): MapStation[] {
    return stationsD;
  },
  get trails(): MapTrail[] {
    return trailsD;
  },
  attach(_client?: unknown, _streamId?: unknown): void {},
  detach(): void {},
  reset(): void {
    void fetch('/api/decodes/adsb/reset', { method: 'POST' }).catch(() => {});
  },
};
