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
  /** Best SNR ever seen — what the cutoff compares (older records: snr_db). */
  peak_snr_db?: number;
  band?: string | null;
  first_seen_ms: number;
  last_seen_ms: number;
  seen: number;
  /** Seen in two separate detector frames (two sweep passes / seconds);
   *  absent on records from older daemons. */
  confirmed?: boolean;
  sources: string[];
  status: 'new' | 'identified' | 'ignored' | string;
  label?: string | null;
  notes?: string | null;
  ident?: { by?: string; at_ms?: number; confidence?: number | null } | null;
  /** The last `inspect` (server/src/inspect_api.rs). */
  inspect?: { at_ms: number; analysis_id: string; hint: string; occupied_hz?: number } | null;
}

/** The list's hygiene policy (server `emitter_config` kind). */
export interface EmitterConfig {
  min_snr_db: number;
  tentative_ttl_ms: number;
  forget_after_ms: number;
}

export const DEFAULT_CONFIG: EmitterConfig = {
  min_snr_db: 15,
  tentative_ttl_ms: 10 * 60_000,
  forget_after_ms: 7 * 86_400_000,
};

/** Shown under the cutoff? Labelled emitters always are (knowledge);
 *  the rest need to have cleared it once and been confirmed — seen in two
 *  separate passes / seconds (until then it's tentative — mostly noise
 *  peaks, expiring). */
export function aboveCutoff(e: Emitter, minSnrDb: number): boolean {
  return !!e.label || ((e.peak_snr_db || e.snr_db) >= minSnrDb && (e.confirmed ?? e.seen >= 2));
}

async function post(url: string, body: unknown): Promise<Response> {
  const r = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
  if (!r.ok) throw new Error(`${url}: ${r.status} ${await r.text()}`);
  return r;
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

  config = $derived<EmitterConfig>({
    ...DEFAULT_CONFIG,
    ...((decoders.kind('emitter_config').current['current']?.data as
      | Partial<EmitterConfig>
      | undefined) ?? {}),
  });

  /** Emitters in [lo, hi] at or above the SNR cutoff; ignored ones hidden
   *  unless asked for. */
  inRange(lo: number, hi: number, includeIgnored = false): Emitter[] {
    const cut = this.config.min_snr_db;
    return this.all.filter(
      (e) =>
        e.freq_hz >= lo &&
        e.freq_hz <= hi &&
        (includeIgnored || e.status !== 'ignored') &&
        aboveCutoff(e, cut),
    );
  }

  /** Change the policy for every viewer (persisted server-side). */
  async setConfig(patch: Partial<EmitterConfig>): Promise<void> {
    await post('/api/emitters/config', patch);
  }

  /** Delete unlabelled emitters below the cutoff + stale / forgotten ones. */
  async prune(): Promise<{ removed: number; remaining: number }> {
    return (await post('/api/emitters/prune', {})).json();
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

  /** Characterise emitter `id` (tunes the receiver there). From the
   *  analyzer, `force` leaves it and `restore` comes back after. */
  async inspect(id: number, fromAnalyzer: boolean): Promise<void> {
    await post('/api/inspect', { emitter_id: id, force: fromAnalyzer, restore: fromAnalyzer });
  }

  /** Label (→ identified), or set the status (`ignored` hides it). The
   *  server publishes the change; every tab and the AI see it. */
  async edit(
    id: number,
    patch: { label?: string; status?: string; notes?: string },
  ): Promise<void> {
    await post(`/api/emitters/${id}`, patch);
  }
}

export const emitters = new EmitterStore();
