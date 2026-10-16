// Spectrum-analyzer display state. Per tab by design (like waterfall
// contrast): two tabs may hold different traces. Everything the radio
// does (range, RBW, averages, gain) is server state and goes through
// the `src` block params instead.

import { decoders } from '$lib/decoders/store.svelte';
import { pipeline } from '$lib/pipeline.svelte';
import { latestRows, type TraceMode } from './panorama';

class SpecAnState {
  traceMode = $state<TraceMode>('live');
  /** Bumped to reset the holds. */
  clearToken = $state(0);
  /** Manual scale by default: a fixed scale keeps passes and holds
   *  comparable at a glance; auto is one click away. */
  autoDb = $state(false);
  dbLo = $state(-100);
  dbHi = $state(-20);
  /** Show the emitter panel on the right. */
  showEmitters = $state(true);
  /** Marker frequency (click), Hz. */
  marker = $state<number | null>(null);

  /** Newest-config rows from the store mirror. */
  rows = $derived(latestRows(decoders.kind('panorama')));
  /** Swept range as the server reports it (falls back to the preset). */
  range = $derived.by<[number, number]>(() => {
    const r = this.rows[0]?.range;
    if (r) return [r[0], r[1]];
    const p = (pipeline.flowgraph?.blocks?.src?.params ?? {}) as Record<string, number>;
    return [p.sweep_start_hz ?? 88e6, p.sweep_stop_hz ?? 108e6];
  });

  /** Measured pass time (s) from consecutive rows' timestamps. */
  passSeconds = $derived.by<number | null>(() => {
    const rows = this.rows;
    if (rows.length === 0) return null;
    const steps = rows[0].steps;
    if (rows.length < steps) return null;
    const ts = rows.map((r) => r.t_ms).sort((a, b) => a - b);
    const gaps = ts
      .slice(1)
      .map((t, i) => t - ts[i])
      .filter((g) => g > 0 && g < 5_000);
    if (gaps.length === 0) return null;
    gaps.sort((a, b) => a - b);
    return (gaps[Math.floor(gaps.length / 2)] * steps) / 1000;
  });

  /** Sweep a new range (server-side; restarts the pass). */
  async setRange(lo: number, hi: number): Promise<void> {
    if (!(hi > lo)) [lo, hi] = [hi, lo];
    if (!(hi - lo >= 10e3)) return;
    this.marker = null;
    await pipeline.patchSourceParams({
      sweep_start_hz: Math.round(lo),
      sweep_stop_hz: Math.round(hi),
    });
  }

  async zoomOut(): Promise<void> {
    const [lo, hi] = this.range;
    const c = (lo + hi) / 2;
    const half = hi - lo;
    await this.setRange(Math.max(0, c - half), c + half);
  }
}

export const specan = new SpecAnState();
