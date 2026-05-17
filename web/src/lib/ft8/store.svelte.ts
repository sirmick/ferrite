// Singleton store for the FT8/FT4/WSPR advanced view. Subscribes to
// the `ui:ft8` sink's stream_id (wired by ft8.json / ft4.json /
// wspr.json), parses the newline-delimited spot records emitted by
// `Ft8Demod` / `WsprDemod` (see blocks/src/digital_spot.rs for the
// authoritative wire shape), and exposes:
//
//   - `decodes`  — the raw spot rows, newest last, capped (table)
//   - `stations` — latest known position per callsign (map markers)
//   - `links`    — operator-QTH → each heard station (map lines)
//
// Data-only: selection/coupling state lives in Ft8View so the table
// and map can share it. The operator's own grid is not in the
// stream — Ft8View feeds it in from the `client.station.grid`
// setting via `setHomeGrid`; until then `home`/`links` stay empty
// and the map just shows decoded stations (still useful).

import { PayloadType, type ParsedFrame } from '$lib/ws/frame';
import type { FrameClient } from '$lib/ws/client';
import { gridToLatLon, type LatLon } from '$lib/geo/maidenhead';

export type FtMode = 'ft8' | 'ft4' | 'wspr';

export interface FtDecode {
  /** Stable, globally-unique per-row id (`utc:de:freq#seq`). The seq
   *  suffix guarantees uniqueness across the long-lived log — natural
   *  `(utc,de,freq)` tuples repeat on sample replay / pipeline
   *  restart. Stable per row, so the table keeps selection across
   *  re-sorts. */
  id: string;
  t: FtMode;
  utc: number; // slot epoch seconds
  de: string; // transmitting callsign
  dx: string | null; // addressed callsign (null on CQ / WSPR)
  grid: string | null; // de's locator when the message carried one
  snr: number; // dB
  dt: number | null; // s (null when absent)
  freq: number; // audio Hz offset
  msg: string; // raw decoded text
  pwr: number | null; // dBm (WSPR)
  drift: number | null; // Hz (WSPR)
}

export interface MapStation {
  id: string; // callsign
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

// Keep memory bounded — a busy 20 m FT8 slot is ~30–40 decodes; an
// hour is a few thousand. Cap to the most recent N and let the
// operator rely on the live view, not infinite scrollback.
const MAX_DECODES = 1500;

class Ft8Store {
  decodes = $state<FtDecode[]>([]);
  /** Operator's own Maidenhead grid (from client settings), or null
   *  when unset — fed by Ft8View, not the stream. */
  homeGrid = $state<string | null>(null);

  // Last locator seen for a callsign, so a later report-only decode
  // for the same station doesn't drop it off the map.
  private gridByCall = new Map<string, string>();
  private unsubscribe?: () => void;
  private streamId?: number;
  // Monotonic row counter. `(utc,de,freq)` is NOT unique across the
  // long-lived log — replaying a sample or restarting the pipeline
  // re-emits identical tuples, which would collide as `{#each}` keys
  // (Svelte `each_key_duplicate`). A sequence makes every row id
  // unique and stable for the store's lifetime.
  private seq = 0;

  // The store is long-lived per advanced-view *type* (this singleton
  // backs the shared FT8/FT4/WSPR view). The accumulated log survives:
  //   - toggling the advanced view off and back (Ft8View unmounts →
  //     detach, but rows are kept),
  //   - switching ft8 ↔ ft4 ↔ wspr (the stream_id changes but it's
  //     the same view type; rows from each mode coexist, the `t`
  //     field distinguishes them).
  // It is cleared *only* by the Reset button (reset()) — never
  // automatically — so the operator never loses a long FT8 session by
  // glancing at the spectrum.
  attach(client: FrameClient, streamId: number): void {
    if (this.streamId === streamId && this.unsubscribe) return;
    // Re-subscribe (first attach, return-from-toggle, or a mode
    // switch that handed us a new stream_id). Drop any stale
    // subscription but keep the accumulated decodes.
    this.unsubscribe?.();
    this.streamId = streamId;
    this.unsubscribe = client.subscribe(streamId, (frame) => this.onFrame(frame));
  }

  /** Stop receiving but KEEP the log — the advanced view unmounting
   *  (operator toggled back to the spectrum) or the pipeline stopping
   *  must not lose accumulated decodes. */
  detach(): void {
    this.unsubscribe?.();
    this.unsubscribe = undefined;
  }

  /** Explicit operator action (Reset button) — the only thing that
   *  clears the accumulated decodes / station map. */
  reset(): void {
    this.decodes = [];
    this.gridByCall.clear();
  }

  setHomeGrid(grid: string | null): void {
    this.homeGrid = grid && grid.trim() ? grid.trim().toUpperCase() : null;
  }

  get home(): (LatLon & { call: string }) | null {
    if (!this.homeGrid) return null;
    const p = gridToLatLon(this.homeGrid);
    return p ? { ...p, call: this.homeGrid } : null;
  }

  // One marker per callsign, positioned from the best locator we've
  // seen for it. Stations whose grid we never learned are simply not
  // mapped (they still appear in the decode table).
  stations = $derived.by<MapStation[]>(() => {
    const out = new Map<string, MapStation>();
    for (const d of this.decodes) {
      const grid = d.grid ?? this.gridByCall.get(d.de);
      if (!d.de || !grid) continue;
      const p = gridToLatLon(grid);
      if (!p) continue;
      out.set(d.de, { id: d.de, call: d.de, lat: p.lat, lon: p.lon, kind: 'station' });
    }
    return [...out.values()];
  });

  links = $derived.by<MapLink[]>(() => {
    const h = this.home;
    if (!h) return [];
    return this.stations.map((s) => ({
      id: `${h.call}-${s.id}`,
      from: [h.lon, h.lat] as [number, number],
      to: [s.lon, s.lat] as [number, number],
    }));
  });

  private onFrame(frame: ParsedFrame): void {
    if (frame.header.payloadType !== PayloadType.JsonEvent) return;
    const text = new TextDecoder().decode(frame.payload);
    let appended: FtDecode[] | null = null;
    for (const line of text.split('\n')) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      let obj: Partial<Record<keyof FtDecode, unknown>> & { t?: unknown };
      try {
        obj = JSON.parse(trimmed);
      } catch {
        continue; // malformed line — drop, same as the RDS store
      }
      if (obj.t !== 'ft8' && obj.t !== 'ft4' && obj.t !== 'wspr') continue;
      const de = typeof obj.de === 'string' ? obj.de : '';
      const utc = typeof obj.utc === 'number' ? obj.utc : 0;
      const freq = typeof obj.freq === 'number' ? obj.freq : 0;
      const grid = typeof obj.grid === 'string' ? obj.grid : null;
      if (grid && de) this.gridByCall.set(de, grid);
      const row: FtDecode = {
        id: `${utc}:${de}:${freq}#${this.seq++}`,
        t: obj.t,
        utc,
        de,
        dx: typeof obj.dx === 'string' ? obj.dx : null,
        grid,
        snr: typeof obj.snr === 'number' ? obj.snr : 0,
        dt: typeof obj.dt === 'number' ? obj.dt : null,
        freq,
        msg: typeof obj.msg === 'string' ? obj.msg : '',
        pwr: typeof obj.pwr === 'number' ? obj.pwr : null,
        drift: typeof obj.drift === 'number' ? obj.drift : null,
      };
      (appended ??= []).push(row);
    }
    if (!appended) return;
    // Single reactive write per frame (a frame may carry several
    // newline-delimited spots) keeps the table from thrashing.
    const next = this.decodes.concat(appended);
    this.decodes = next.length > MAX_DECODES ? next.slice(next.length - MAX_DECODES) : next;
  }
}

export const ft8 = new Ft8Store();
