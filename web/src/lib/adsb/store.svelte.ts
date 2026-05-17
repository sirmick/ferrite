// Singleton store for the ADS-B advanced view. Subscribes to the
// `ui:adsb` sink (wired by adsb.json), consuming the ~1 Hz full
// aircraft snapshot `AdsbDemod` emits (see blocks/src/aircraft_spot.rs
// for the wire shape). Unlike the FT8 per-message log this is a
// keyed-by-ICAO live set: each snapshot upserts rows; aircraft that
// stop appearing age out.
//
// Same lifecycle contract as the FT8 store: long-lived per
// advanced-view type (survives view toggles / pipeline restarts),
// cleared only by the Reset button. Data-only — selection lives in
// AdsbView so the table and map stay coupled (keyed by ICAO).

import { PayloadType, type ParsedFrame } from '$lib/ws/frame';
import type { FrameClient } from '$lib/ws/client';
import type { MapStation, MapTrail } from '$lib/viz/StationMap.svelte';

export interface Aircraft {
  /** ICAO 24-bit, 6-hex upper — the stable per-airframe key + row id. */
  id: string;
  flight: string; // callsign, '' until an ID frame
  lat: number | null;
  lon: number | null;
  alt: number; // ft
  gs: number; // kt
  trk: number; // deg
  msgs: number;
  age: number; // s since last message (from the decoder)
  /** Wall-clock ms we last saw this aircraft in a snapshot — drives
   *  local staleness pruning (the stream has no snapshot delimiter). */
  seenMs: number;
}

// Drop an aircraft once it's been absent from snapshots this long.
// dump1090 keeps stale entries; the ~1 Hz snapshot re-lists live ones
// with a small age, so absence is the reliable "gone" signal.
const STALE_MS = 60_000;
// Trail ring length per aircraft (≈ last N fixes; ~1/s ⇒ ~1 min).
const TRAIL_MAX = 60;

class AdsbStore {
  private byIcao = $state(new Map<string, Aircraft>());
  private trail = new Map<string, [number, number][]>();
  private unsubscribe?: () => void;
  private streamId?: number;

  // Long-lived per advanced-view type — see the FT8 store for the
  // rationale. attach() never clears; only reset() does.
  attach(client: FrameClient, streamId: number): void {
    if (this.streamId === streamId && this.unsubscribe) return;
    this.unsubscribe?.();
    this.streamId = streamId;
    this.unsubscribe = client.subscribe(streamId, (frame) => this.onFrame(frame));
  }

  detach(): void {
    this.unsubscribe?.();
    this.unsubscribe = undefined;
  }

  reset(): void {
    this.byIcao = new Map();
    this.trail.clear();
  }

  /** Table rows. */
  aircraft = $derived([...this.byIcao.values()]);

  /** Map markers — only aircraft with a decoded position; the
   *  triangle points along `heading`. */
  stations = $derived.by<MapStation[]>(() =>
    [...this.byIcao.values()]
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

  /** Recent flight paths (only for aircraft still present). */
  trails = $derived.by<MapTrail[]>(() => {
    const out: MapTrail[] = [];
    for (const id of this.byIcao.keys()) {
      const p = this.trail.get(id);
      if (p && p.length >= 2) out.push({ id, path: p });
    }
    return out;
  });

  private onFrame(frame: ParsedFrame): void {
    if (frame.header.payloadType !== PayloadType.JsonEvent) return;
    const text = new TextDecoder().decode(frame.payload);
    const now = Date.now();
    let touched = false;
    for (const line of text.split('\n')) {
      const t = line.trim();
      if (!t) continue;
      let o: Record<string, unknown>;
      try {
        o = JSON.parse(t);
      } catch {
        continue;
      }
      if (typeof o.icao !== 'string') continue;
      const id = o.icao;
      const lat = typeof o.lat === 'number' ? o.lat : null;
      const lon = typeof o.lon === 'number' ? o.lon : null;
      this.byIcao.set(id, {
        id,
        flight: typeof o.flight === 'string' ? o.flight : '',
        lat,
        lon,
        alt: typeof o.alt === 'number' ? o.alt : 0,
        gs: typeof o.gs === 'number' ? o.gs : 0,
        trk: typeof o.trk === 'number' ? o.trk : 0,
        msgs: typeof o.msgs === 'number' ? o.msgs : 0,
        age: typeof o.age === 'number' ? o.age : 0,
        seenMs: now,
      });
      if (lat !== null && lon !== null) {
        const ring = this.trail.get(id) ?? [];
        const last = ring[ring.length - 1];
        if (!last || last[0] !== lon || last[1] !== lat) {
          ring.push([lon, lat]);
          if (ring.length > TRAIL_MAX) ring.shift();
          this.trail.set(id, ring);
        }
      }
      touched = true;
    }
    if (!touched) return;
    // Prune aircraft absent from snapshots for too long. Runs on the
    // ~1 Hz snapshot cadence, cheap for a few hundred entries.
    for (const [id, a] of this.byIcao) {
      if (now - a.seenMs > STALE_MS) {
        this.byIcao.delete(id);
        this.trail.delete(id);
      }
    }
    // Reassign to trigger the $derived recomputation (Map mutation
    // alone isn't tracked).
    this.byIcao = new Map(this.byIcao);
  }
}

export const adsb = new AdsbStore();
