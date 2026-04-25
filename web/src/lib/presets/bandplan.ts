// US frequency-allocation overlay data for the spectrum view — every
// visible band labelled with its service name (FM Broadcast, Air Band,
// ADS-B 1090, marine VHF channels, etc.) so a tuner sees *what* they're
// looking at, not just where.
//
// Source: Arrin Clark (KN1E)'s SDR-Band-Plans project, CC0:
//   https://github.com/Arrin-KN1E/SDR-Band-Plans
// We import the SDR++ JSON verbatim (`bandplan-usa.json`); upstream
// updates are a one-file copy. The source colors are AARRGGBB hex
// (SDR++ convention); we strip the alpha channel here so each render
// site can pick its own opacity for its idiom (saturated strip, faint
// background tint, etc.) without re-parsing.

import raw from './bandplan-usa.json';

export interface Band {
  /** Human-readable band/service name, e.g. "FM Broadcast" or "ADSB 1090". */
  name: string;
  /** Inclusive lower edge in Hz. */
  startHz: number;
  /** Exclusive upper edge in Hz. */
  endHz: number;
  /** RGB triple parsed from the source's AARRGGBB. Alpha is left to the
   *  draw site so the same band data can power both a saturated label
   *  strip and a faint full-height tint. */
  rgb: readonly [number, number, number];
}

interface RawBandplanFile {
  name: string;
  country_code: string;
  bands: ReadonlyArray<{ name: string; type: string; start: number; end: number }>;
}

function parseArgb(argb: string): readonly [number, number, number] {
  if (argb.length !== 8) return [128, 128, 128];
  const r = parseInt(argb.slice(2, 4), 16);
  const g = parseInt(argb.slice(4, 6), 16);
  const b = parseInt(argb.slice(6, 8), 16);
  if (Number.isNaN(r) || Number.isNaN(g) || Number.isNaN(b)) return [128, 128, 128];
  return [r, g, b];
}

const file = raw as RawBandplanFile;

/** All US bands, in source order (sorted by `startHz`, no overlaps). */
export const bandplanUsa: ReadonlyArray<Band> = file.bands.map((b) => ({
  name: b.name,
  startHz: b.start,
  endHz: b.end,
  rgb: parseArgb(b.type),
}));

/** Bands overlapping `[loHz, hiHz]`, in ascending order. Linear scan —
 *  ~1.2k entries × ~60 Hz draw rate is negligible; if this ever shows
 *  up in a profile, swap to a lower-bound binary search on `startHz`. */
export function bandsInRange(loHz: number, hiHz: number): Band[] {
  const out: Band[] = [];
  for (const b of bandplanUsa) {
    if (b.endHz <= loHz) continue;
    if (b.startHz >= hiHz) break;
    out.push(b);
  }
  return out;
}
