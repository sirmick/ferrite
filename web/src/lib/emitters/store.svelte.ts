// The emitter list (server `emitter` store kind — see server/src/emitters.rs):
// every transmitter the station has seen, from the receiver's SignalList and
// the spectrum analyzer's sweeps alike, numbered and persisted. Both the
// receiver's Strongest Signals pane and the analyzer's emitter panel read
// it here, so a station has one number and one label everywhere.

import { decoders } from '$lib/decoders/store.svelte';

export interface Emitter {
  id: number;
  freq_hz: number;
  width_hz: number;
  level_dbfs: number;
  peak_dbfs: number;
  snr_db: number;
  band?: string | null;
  first_seen_ms: number;
  last_seen_ms: number;
  seen: number;
  sources: string[];
  status: 'new' | 'identified' | 'ignored' | string;
  label?: string | null;
  notes?: string | null;
  ident?: { by?: string; at_ms?: number; confidence?: number | null } | null;
}

// The server tracker's matching rules (server/src/emitters.rs) — kept in
// step so the receiver pane attributes a live detection to the same
// emitter the server folded it into.
const FM_LO_HZ = 87.8e6;
const FM_HI_HZ = 108.1e6;
const FM_FIRST_HZ = 87.9e6;
const FM_GRID_HZ = 200e3;
const SATELLITE_DB = 10;
const SATELLITE_MIN_WIDTH_HZ = 100e3;
const SATELLITE_REACH = 0.75;

/** Broadcast-FM channel cell, or null outside the band. */
export function fmCell(f: number): number | null {
  return f >= FM_LO_HZ && f < FM_HI_HZ ? Math.round((f - FM_FIRST_HZ) / FM_GRID_HZ) : null;
}

/** Does a detection (`freqHz`, `widthHz`, `levelDbfs`) belong to `e`? */
export function matches(e: Emitter, freqHz: number, widthHz = 0, levelDbfs = -Infinity): boolean {
  const a = fmCell(e.freq_hz);
  const b = fmCell(freqHz);
  let near: boolean;
  if (a !== null && b !== null) near = a === b;
  else if (a === null && b === null) {
    const lo = freqHz - widthHz / 2;
    const hi = freqHz + widthHz / 2;
    near = lo - (e.freq_hz + e.width_hz / 2) <= 5_000 && e.freq_hz - e.width_hz / 2 - hi <= 5_000;
  } else near = false;
  const satellite =
    e.width_hz >= SATELLITE_MIN_WIDTH_HZ &&
    e.peak_dbfs - levelDbfs >= SATELLITE_DB &&
    Math.abs(freqHz - e.freq_hz) <= e.width_hz / 2 + SATELLITE_REACH * e.width_hz;
  return near || satellite;
}

class EmitterStore {
  all = $derived<Emitter[]>(
    Object.values(decoders.kind('emitter').current)
      .map((r) => r.data as Emitter)
      .filter((e) => e && typeof e.id === 'number'),
  );

  /** Emitters in [lo, hi], ignored ones hidden unless asked for. */
  inRange(lo: number, hi: number, includeIgnored = false): Emitter[] {
    return this.all.filter(
      (e) => e.freq_hz >= lo && e.freq_hz <= hi && (includeIgnored || e.status !== 'ignored'),
    );
  }

  /** The emitter a live detection belongs to (nearest within tolerance). */
  match(freqHz: number, widthHz = 0, levelDbfs = -Infinity): Emitter | undefined {
    let best: Emitter | undefined;
    for (const e of this.all) {
      if (!matches(e, freqHz, widthHz, levelDbfs)) continue;
      if (!best || Math.abs(e.freq_hz - freqHz) < Math.abs(best.freq_hz - freqHz)) best = e;
    }
    return best;
  }

  /** Label (→ identified), or set the status (`ignored` hides it). The
   *  server publishes the change; every tab and the AI see it. */
  async edit(
    id: number,
    patch: { label?: string; status?: string; notes?: string },
  ): Promise<void> {
    const r = await fetch(`/api/emitters/${id}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify(patch),
    });
    if (!r.ok) throw new Error(`emitter #${id}: ${r.status} ${await r.text()}`);
  }
}

export const emitters = new EmitterStore();
