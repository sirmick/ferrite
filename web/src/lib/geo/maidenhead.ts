// Maidenhead locator ↔ geographic coordinate conversion.
//
// The locator is the grid-square system every weak-signal mode uses
// to carry a station's approximate location in a handful of
// characters (FT8/FT4 messages, WSPR's compressed payload). We only
// ever need locator → point (to place a station on the map); the
// reverse is handy for turning the operator's own configured grid
// into a "home" marker.
//
// Encoding, most-significant pair first:
//   field      chars 0–1  A–R (18)   20° lon × 10° lat
//   square     chars 2–3  0–9 (10)    2° lon ×  1° lat
//   subsquare  chars 4–5  a–x (24)    5′ lon ×  2.5′ lat
//
// A 4-char locator resolves a 2°×1° cell; 6-char resolves
// 5′×2.5′. We return the **centroid** of whatever cell the given
// precision resolves, so a 4-char grid lands in the middle of its
// square rather than its SW corner (which would bias every line and
// marker down-left).

const A = 'A'.charCodeAt(0);
const ZERO = '0'.charCodeAt(0);
const LOWER_A = 'a'.charCodeAt(0);

export interface LatLon {
  lat: number;
  lon: number;
}

/** Parse a 4- or 6-char Maidenhead locator to the centroid lat/lon
 *  of the cell it resolves. Returns null for malformed input — the
 *  caller (store) should drop stations whose grid we can't place
 *  rather than render them at (0,0). */
export function gridToLatLon(grid: string): LatLon | null {
  if (!grid) return null;
  const g = grid.trim().toUpperCase();
  // Accept 4 or 6 chars. Longer (8-char) locators exist but no mode
  // we decode emits them; truncate defensively.
  if (g.length !== 4 && g.length !== 6) return null;

  const f1 = g.charCodeAt(0) - A; // lon field  0..17
  const f2 = g.charCodeAt(1) - A; // lat field  0..17
  const s1 = g.charCodeAt(2) - ZERO; // lon square 0..9
  const s2 = g.charCodeAt(3) - ZERO; // lat square 0..9
  if (f1 < 0 || f1 > 17 || f2 < 0 || f2 > 17) return null;
  if (s1 < 0 || s1 > 9 || s2 < 0 || s2 > 9) return null;

  let lon = -180 + f1 * 20 + s1 * 2;
  let lat = -90 + f2 * 10 + s2 * 1;

  if (g.length === 6) {
    // Subsquare uses the lowercase a–x alphabet in the spec; we
    // upper-cased above, so index off 'A' here.
    const u1 = g.charCodeAt(4) - A; // lon subsquare 0..23
    const u2 = g.charCodeAt(5) - A; // lat subsquare 0..23
    if (u1 < 0 || u1 > 23 || u2 < 0 || u2 > 23) return null;
    lon += u1 * (5 / 60); // 5′ of longitude
    lat += u2 * (2.5 / 60); // 2.5′ of latitude
    // Centroid of a subsquare cell.
    lon += 2.5 / 60;
    lat += 1.25 / 60;
  } else {
    // Centroid of a 2°×1° square cell.
    lon += 1;
    lat += 0.5;
  }

  return { lat, lon };
}

/** Encode a lat/lon to a 6-char locator. Only used to derive the
 *  operator's home marker from a configured grid round-trip / tests;
 *  not on any hot path. */
export function latLonToGrid(lat: number, lon: number): string {
  const adjLon = Math.min(179.999999, Math.max(-180, lon)) + 180;
  const adjLat = Math.min(89.999999, Math.max(-90, lat)) + 90;

  const f1 = Math.floor(adjLon / 20);
  const f2 = Math.floor(adjLat / 10);
  const s1 = Math.floor((adjLon % 20) / 2);
  const s2 = Math.floor(adjLat % 10);
  const u1 = Math.floor(((adjLon % 2) / 2) * 24);
  const u2 = Math.floor(((adjLat % 1) / 1) * 24);

  return (
    String.fromCharCode(A + f1) +
    String.fromCharCode(A + f2) +
    String.fromCharCode(ZERO + s1) +
    String.fromCharCode(ZERO + s2) +
    String.fromCharCode(LOWER_A + u1) +
    String.fromCharCode(LOWER_A + u2)
  );
}
