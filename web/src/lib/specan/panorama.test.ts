import { describe, expect, it } from 'vitest';
import {
  autoRange,
  decodePower,
  latestRows,
  reduceToColumns,
  RowCache,
  ticks,
  TraceHolds,
  type PanoramaRow,
} from './panorama';

function encode(db: number[]): string {
  const bytes = new Uint8Array(db.length * 2);
  db.forEach((v, i) => {
    const q = Number.isNaN(v) ? -32768 : Math.round(v * 100);
    const u = q < 0 ? q + 0x10000 : q;
    bytes[2 * i] = u & 0xff;
    bytes[2 * i + 1] = u >> 8;
  });
  return btoa(String.fromCharCode(...bytes));
}

function row(
  step: number,
  f_lo_hz: number,
  db: number[],
  extra: Partial<PanoramaRow> = {},
): PanoramaRow {
  return {
    cfg: 1,
    pass: 0,
    step,
    steps: 2,
    f_lo_hz,
    bin_hz: 1000,
    n: db.length,
    power: encode(db),
    center_hz: f_lo_hz,
    range: [0, 2000],
    rate_hz: 10e6,
    rbw_hz: 1000,
    fft_size: 4096,
    averages: 1,
    t_ms: 1,
    ...extra,
  };
}

const kind = (rows: PanoramaRow[]) => ({
  recent: [],
  current: Object.fromEntries(rows.map((r) => [String(r.step), { seq: 1, at_ms: 0, data: r }])),
});

describe('panorama', () => {
  it('decodes centi-dB with masks', () => {
    const d = decodePower(encode([-100, -12.34, NaN, 0]));
    expect(d[0]).toBeCloseTo(-100);
    expect(d[1]).toBeCloseTo(-12.34);
    expect(Number.isNaN(d[2])).toBe(true);
    expect(d[3]).toBe(0);
  });

  it('keeps only the newest sweep config, in step order', () => {
    const rows = latestRows(
      kind([row(1, 1000, [0]), row(0, 0, [0]), row(5, 9000, [0], { cfg: 0 })]),
    );
    expect(rows.map((r) => r.step)).toEqual([0, 1]);
  });

  it('reduction keeps a one-bin carrier when zoomed out (max, not mean)', () => {
    const db = new Array(1000).fill(-100);
    db[537] = -20;
    const cols = reduceToColumns(new RowCache().decode([row(0, 0, db)]), 0, 1_000_000, 10);
    expect(Math.max(...cols)).toBeCloseTo(-20);
    expect(cols[5]).toBeCloseTo(-20); // bin 537 → column 5
    expect(cols[0]).toBeCloseTo(-100);
  });

  it('upsamples when columns are finer than bins and leaves gaps NaN', () => {
    const cols = reduceToColumns(new RowCache().decode([row(0, 0, [-50, -60])]), -1000, 5000, 12);
    expect(Number.isNaN(cols[11])).toBe(true); // beyond the row
    expect(cols.some((v) => v === -50)).toBe(true);
  });

  it('holds track max/min and reset on geometry change', () => {
    const h = new TraceHolds();
    h.update(Float32Array.from([-10, -50]), 'a', 1);
    h.update(Float32Array.from([-20, -40]), 'a', 2);
    expect(Array.from(h.max)).toEqual([-10, -40]);
    expect(Array.from(h.min)).toEqual([-20, -50]);
    h.update(Float32Array.from([-30, -30]), 'b', 3);
    expect(Array.from(h.max)).toEqual([-30, -30]);
  });

  it('auto range brackets the data', () => {
    const [lo, hi] = autoRange(Float32Array.from([-100, -98, -99, -40]));
    expect(lo).toBeLessThan(-100);
    expect(hi).toBeGreaterThan(-40);
  });

  it('ticks are round numbers inside the range', () => {
    const t = ticks(88e6, 108e6, 8);
    expect(t[0]).toBeGreaterThanOrEqual(88e6);
    expect(t.at(-1)).toBeLessThanOrEqual(108e6);
    expect(t.every((x) => x % 500e3 === 0)).toBe(true);
    expect(t.length).toBeGreaterThanOrEqual(6);
  });
});
