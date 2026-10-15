// Panorama rows → display columns. Pure (no DOM), unit-tested.
//
// The server's `panorama` store kind holds one record per sweep step
// (keyed by step) — the current set *is* the last full pass. Each row:
//   { cfg, pass, step, steps, f_lo_hz, bin_hz, n, power (b64 i16 centi-dBFS),
//     center_hz, range: [lo, hi], rate_hz, rbw_hz, fft_size, averages, t_ms }
// Rows from an older sweep config (`cfg`) linger until they age out, so
// consumers keep only the newest cfg.

import type { KindState } from '$lib/decoders/store.svelte';

export interface PanoramaRow {
  cfg: number;
  pass: number;
  step: number;
  steps: number;
  f_lo_hz: number;
  bin_hz: number;
  n: number;
  power: string;
  center_hz: number;
  range: [number, number];
  rate_hz: number;
  rbw_hz: number;
  fft_size: number;
  averages: number;
  t_ms: number;
}

export interface DecodedRow extends PanoramaRow {
  db: Float32Array;
}

const MASKED = -32768;

/** base64 little-endian i16 centi-dB → dBFS (NaN = masked). */
export function decodePower(b64: string): Float32Array {
  const bin = atob(b64);
  const n = bin.length >> 1;
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    let q = bin.charCodeAt(2 * i) | (bin.charCodeAt(2 * i + 1) << 8);
    if (q & 0x8000) q -= 0x10000;
    out[i] = q === MASKED ? NaN : q / 100;
  }
  return out;
}

/** Rows of the newest sweep config, sorted by step. */
export function latestRows(kind: KindState): PanoramaRow[] {
  const rows = Object.values(kind.current)
    .map((r) => r.data as PanoramaRow)
    .filter((r) => r && typeof r.step === 'number' && typeof r.power === 'string');
  if (rows.length === 0) return [];
  const cfg = Math.max(...rows.map((r) => r.cfg));
  return rows.filter((r) => r.cfg === cfg).sort((a, b) => a.step - b.step);
}

/** Decode with a per-row cache keyed by (step, t_ms) — rows are ~3 k
 *  bins and the store re-emits the whole kind on every upsert. */
export class RowCache {
  private cache = new Map<number, { t: number; cfg: number; row: DecodedRow }>();

  decode(rows: PanoramaRow[]): DecodedRow[] {
    return rows.map((r) => {
      const hit = this.cache.get(r.step);
      if (hit && hit.t === r.t_ms && hit.cfg === r.cfg) return hit.row;
      const row = { ...r, db: decodePower(r.power) };
      this.cache.set(r.step, { t: r.t_ms, cfg: r.cfg, row });
      return row;
    });
  }
}

/** Peak-preserving reduction of rows onto `width` columns spanning
 *  [lo, hi): each column holds the **max** of the bins that fall in it
 *  (so a one-bin carrier survives a 1000:1 zoom-out); columns no bin
 *  reaches are NaN. When columns are finer than bins, each column takes
 *  the bin under its centre. */
export function reduceToColumns(
  rows: DecodedRow[],
  lo: number,
  hi: number,
  width: number,
): Float32Array {
  const out = new Float32Array(width).fill(NaN);
  if (width <= 0 || !(hi > lo)) return out;
  const colHz = (hi - lo) / width;
  for (const r of rows) {
    const { db, f_lo_hz: f0, bin_hz: bin } = r;
    if (bin >= colHz) {
      // Upsample: each column samples its centre bin.
      const c0 = Math.max(0, Math.floor((f0 - bin / 2 - lo) / colHz));
      const c1 = Math.min(width - 1, Math.ceil((f0 + (db.length - 0.5) * bin - lo) / colHz));
      for (let c = c0; c <= c1; c++) {
        const f = lo + (c + 0.5) * colHz;
        const k = Math.round((f - f0) / bin);
        if (k >= 0 && k < db.length) {
          const v = db[k];
          if (!Number.isNaN(v) && !(out[c] >= v)) out[c] = v;
        }
      }
    } else {
      for (let k = 0; k < db.length; k++) {
        const v = db[k];
        if (Number.isNaN(v)) continue;
        const c = Math.floor((f0 + k * bin - lo) / colHz);
        if (c < 0 || c >= width) continue;
        if (!(out[c] >= v)) out[c] = v;
      }
    }
  }
  return out;
}

export type TraceMode = 'live' | 'max' | 'min' | 'avg';

/** Per-tab trace holds over display columns. Reset whenever the column
 *  geometry (range / width) changes. */
export class TraceHolds {
  max: Float32Array = new Float32Array(0);
  min: Float32Array = new Float32Array(0);
  avg: Float32Array = new Float32Array(0);
  private key = '';
  private lastT = new Map<number, number>();

  /** Fold a fresh live trace in. `stamp` identifies new data (e.g. the
   *  newest row t_ms) so re-renders without new rows don't skew avg. */
  update(live: Float32Array, geomKey: string, stamp: number): void {
    if (geomKey !== this.key || live.length !== this.max.length) {
      this.key = geomKey;
      this.max = Float32Array.from(live);
      this.min = Float32Array.from(live);
      this.avg = Float32Array.from(live);
      this.lastT.clear();
      this.lastT.set(0, stamp);
      return;
    }
    if (this.lastT.get(0) === stamp) return;
    this.lastT.set(0, stamp);
    for (let i = 0; i < live.length; i++) {
      const v = live[i];
      if (Number.isNaN(v)) continue;
      if (!(this.max[i] >= v)) this.max[i] = v;
      if (!(this.min[i] <= v)) this.min[i] = v;
      // EMA on dB (display smoothing, not a power average).
      this.avg[i] = Number.isNaN(this.avg[i]) ? v : this.avg[i] * 0.85 + v * 0.15;
    }
  }

  clear(): void {
    this.key = '';
  }

  pick(mode: TraceMode, live: Float32Array): Float32Array {
    switch (mode) {
      case 'max':
        return this.max;
      case 'min':
        return this.min;
      case 'avg':
        return this.avg;
      default:
        return live;
    }
  }
}

/** Robust display range: [p5 − 5 dB, max + 3 dB] of the finite values. */
export function autoRange(cols: Float32Array): [number, number] {
  const v = Array.from(cols).filter((x) => Number.isFinite(x));
  if (v.length === 0) return [-120, -20];
  v.sort((a, b) => a - b);
  const p5 = v[Math.floor(v.length * 0.05)];
  const top = v[v.length - 1];
  return [Math.floor(p5 - 5), Math.ceil(Math.max(top + 3, p5 + 20))];
}

/** "123.456 MHz"-style label with sensible precision for a span. */
export function fmtHz(hz: number, spanHz: number): string {
  if (Math.abs(hz) >= 1e9) return `${(hz / 1e9).toFixed(spanHz < 1e7 ? 4 : 3)} GHz`;
  if (Math.abs(hz) >= 1e6)
    return `${(hz / 1e6).toFixed(spanHz < 2e6 ? 3 : spanHz < 2e7 ? 2 : 1)} MHz`;
  if (Math.abs(hz) >= 1e3) return `${(hz / 1e3).toFixed(1)} kHz`;
  return `${hz.toFixed(0)} Hz`;
}

/** "Nice" tick positions across [lo, hi] — ~`target` ticks. */
export function ticks(lo: number, hi: number, target = 8): number[] {
  const span = hi - lo;
  if (!(span > 0)) return [];
  const raw = span / target;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 2.5, 5, 10].map((m) => m * mag).find((s) => s >= raw) ?? 10 * mag;
  const out: number[] = [];
  const first = Math.ceil(lo / step);
  for (let i = first; i * step <= hi; i++) out.push(Math.round(i * step * 1e3) / 1e3);
  return out;
}
