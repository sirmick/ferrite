// FT8/FT4/WSPR view store — now a thin adapter over the decoder mirror.
//
// `Ft8Demod` (ft8/ft4 → kind `ft8`) and `WsprDemod` (→ kind `wspr`) emit
// newline-delimited spot records (digital_spot.rs). Both kinds are append
// logs in the server `DecoderStore`, mirrored over `/ws/state`. This
// adapter unions them (the shared view shows all three modes, keyed by
// `t`) and keeps the original surface:
//   - `decodes`  — raw spot rows, newest last, capped (table)
//   - `stations` — latest known position per callsign (map markers)
//   - `links`    — operator-QTH → each heard station (map lines)
// The operator's own grid isn't in the stream — Ft8View feeds it via
// `setHomeGrid` (held module-locally).

import { decoders, type DecoderRecord } from '$lib/decoders/store.svelte';
import { gridToLatLon, type LatLon } from '$lib/geo/maidenhead';

export type FtMode = 'ft8' | 'ft4' | 'wspr';

export interface FtDecode {
  id: string;
  t: FtMode;
  utc: number;
  de: string;
  dx: string | null;
  grid: string | null;
  snr: number;
  dt: number | null;
  freq: number;
  msg: string;
  pwr: number | null;
  drift: number | null;
}

export interface MapStation {
  id: string;
  call: string;
  lat: number;
  lon: number;
  kind: 'station';
}
export interface MapLink {
  id: string;
  from: [number, number];
  to: [number, number];
}

const MAX_DECODES = 1500;

let homeGrid = $state<string | null>(null);

function toDecode(r: DecoderRecord): FtDecode | null {
  const o = r.data as Partial<Record<keyof FtDecode, unknown>> & { t?: unknown };
  if (o.t !== 'ft8' && o.t !== 'ft4' && o.t !== 'wspr') return null;
  const de = typeof o.de === 'string' ? o.de : '';
  const utc = typeof o.utc === 'number' ? o.utc : 0;
  const freq = typeof o.freq === 'number' ? o.freq : 0;
  return {
    id: `${utc}:${de}:${freq}#${r.seq}`,
    t: o.t,
    utc,
    de,
    dx: typeof o.dx === 'string' ? o.dx : null,
    grid: typeof o.grid === 'string' ? o.grid : null,
    snr: typeof o.snr === 'number' ? o.snr : 0,
    dt: typeof o.dt === 'number' ? o.dt : null,
    freq,
    msg: typeof o.msg === 'string' ? o.msg : '',
    pwr: typeof o.pwr === 'number' ? o.pwr : null,
    drift: typeof o.drift === 'number' ? o.drift : null,
  };
}

// Union the ft8 + wspr append logs, ordered by store seq, capped.
const decodesD = $derived.by<FtDecode[]>(() => {
  const rows = [...decoders.kind('ft8').recent, ...decoders.kind('wspr').recent]
    .sort((a, b) => a.seq - b.seq)
    .map(toDecode)
    .filter((d): d is FtDecode => d !== null);
  return rows.length > MAX_DECODES ? rows.slice(rows.length - MAX_DECODES) : rows;
});

// One marker per callsign, from the best locator seen for it in the log.
const stationsD = $derived.by<MapStation[]>(() => {
  const grids = new Map<string, string>();
  for (const d of decodesD) if (d.de && d.grid) grids.set(d.de, d.grid);
  const out = new Map<string, MapStation>();
  for (const d of decodesD) {
    const grid = d.grid ?? grids.get(d.de);
    if (!d.de || !grid) continue;
    const p = gridToLatLon(grid);
    if (!p) continue;
    out.set(d.de, { id: d.de, call: d.de, lat: p.lat, lon: p.lon, kind: 'station' });
  }
  return [...out.values()];
});

export const ft8 = {
  get decodes(): FtDecode[] {
    return decodesD;
  },
  get homeGrid(): string | null {
    return homeGrid;
  },
  setHomeGrid(grid: string | null): void {
    homeGrid = grid && grid.trim() ? grid.trim().toUpperCase() : null;
  },
  get home(): (LatLon & { call: string }) | null {
    if (!homeGrid) return null;
    const p = gridToLatLon(homeGrid);
    return p ? { ...p, call: homeGrid } : null;
  },
  get stations(): MapStation[] {
    return stationsD;
  },
  get links(): MapLink[] {
    const h = this.home;
    if (!h) return [];
    return stationsD.map((s) => ({
      id: `${h.call}-${s.id}`,
      from: [h.lon, h.lat] as [number, number],
      to: [s.lon, s.lat] as [number, number],
    }));
  },

  attach(_client?: unknown, _streamId?: unknown): void {},
  detach(): void {},
  reset(): void {
    void fetch('/api/state/ft8/reset', { method: 'POST' }).catch(() => {});
    void fetch('/api/state/wspr/reset', { method: 'POST' }).catch(() => {});
  },
};
