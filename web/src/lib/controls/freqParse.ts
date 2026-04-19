// Freeform frequency parser.
//
// Accepts a user-typed string and returns an integer Hz value, or `null`
// when the string is not a valid frequency.
//
// Supported forms (case-insensitive, spaces/commas ignored):
//   "1.25m"    → 1_250_000 Hz     (m = MHz)
//   "2.54g"    → 2_540_000_000 Hz (g = GHz)
//   "144.390"  → 144_390_000 Hz   (bare number defaults to MHz)
//   "100.1MHz" → 100_100_000 Hz
//   "446000k"  → 446_000_000 Hz   (k = kHz)
//   "162550h"  → 162_550 Hz       (h/hz = Hz literal)
//   "88100000" → 88_100_000 Hz    (7+ digits w/ no unit → bare Hz)
//
// The "7+ digits → Hz" heuristic keeps copy-pasting from logs painless:
// long integers couldn't plausibly be MHz.

export interface ParseResult {
  hz: number;
  /** What the unit resolved to — useful for tests and diagnostics. */
  unit: 'hz' | 'khz' | 'mhz' | 'ghz';
}

const UNIT_MULT: Record<string, { mult: number; unit: ParseResult['unit'] }> = {
  h: { mult: 1, unit: 'hz' },
  hz: { mult: 1, unit: 'hz' },
  k: { mult: 1e3, unit: 'khz' },
  khz: { mult: 1e3, unit: 'khz' },
  m: { mult: 1e6, unit: 'mhz' },
  mhz: { mult: 1e6, unit: 'mhz' },
  g: { mult: 1e9, unit: 'ghz' },
  ghz: { mult: 1e9, unit: 'ghz' },
};

export function parseFreq(input: string): ParseResult | null {
  const s = input
    .trim()
    .toLowerCase()
    .replace(/[\s,_]/g, '');
  if (!s) return null;
  const m = /^([+-]?\d*\.?\d+)(?:e([+-]?\d+))?([a-z]+)?$/.exec(s);
  if (!m) return null;

  const base = Number(m[2] ? `${m[1]}e${m[2]}` : m[1]);
  if (!Number.isFinite(base) || base < 0) return null;

  const unitRaw = m[3];
  let entry = unitRaw ? UNIT_MULT[unitRaw] : undefined;
  if (unitRaw && !entry) return null;

  if (!entry) {
    // No unit → guess. Long integer bare numbers (7+ digits and no
    // fractional part) are almost certainly Hz; everything else is MHz.
    const rawDigitsOnly = /^\d+$/.test(m[1] ?? '');
    const digits = (m[1] ?? '').length;
    if (rawDigitsOnly && digits >= 7) entry = { mult: 1, unit: 'hz' };
    else entry = { mult: 1e6, unit: 'mhz' };
  }

  const hz = Math.round(base * entry.mult);
  if (!Number.isFinite(hz) || hz < 0) return null;
  return { hz, unit: entry.unit };
}

/**
 * Split an Hz integer into the 10 decimal digits used by the Nixie
 * display, from GHz (index 0) down to 1 Hz (index 9). Values above
 * 9.999 999 999 GHz saturate — fine for any real SDR.
 */
export function digitsOfHz(hz: number): number[] {
  const clamped = Math.max(0, Math.min(9_999_999_999, Math.round(hz)));
  const out: number[] = [];
  let v = clamped;
  for (let i = 0; i < 10; i++) {
    out.unshift(v % 10);
    v = Math.floor(v / 10);
  }
  return out;
}
