import { describe, it, expect } from 'vitest';
import { gridToLatLon, latLonToGrid } from './maidenhead';

describe('gridToLatLon', () => {
  it('places a 4-char square at its centroid', () => {
    // FN42 — eastern Massachusetts. Square spans lon -72..-70,
    // lat 42..43; centroid is (-71, 42.5).
    expect(gridToLatLon('FN42')).toEqual({ lat: 42.5, lon: -71 });
  });

  it('places IO91 (London) at its centroid', () => {
    expect(gridToLatLon('IO91')).toEqual({ lat: 51.5, lon: -1 });
  });

  it('is case-insensitive and trims', () => {
    expect(gridToLatLon('  fn42  ')).toEqual({ lat: 42.5, lon: -71 });
  });

  it('refines with a 6-char subsquare', () => {
    const p = gridToLatLon('FN42aa');
    // Sub-cell aa sits at the SW of the square; centroid shifts
    // down-left from the 4-char centroid but stays inside FN42.
    expect(p).not.toBeNull();
    expect(p!.lon).toBeGreaterThan(-72);
    expect(p!.lon).toBeLessThan(-71);
    expect(p!.lat).toBeGreaterThan(42);
    expect(p!.lat).toBeLessThan(42.5);
  });

  it('rejects malformed locators', () => {
    expect(gridToLatLon('')).toBeNull();
    expect(gridToLatLon('FN4')).toBeNull();
    expect(gridToLatLon('ZZ99')).toBeNull(); // field out of A–R
    expect(gridToLatLon('FN42a')).toBeNull(); // odd length
  });
});

describe('latLonToGrid', () => {
  it('round-trips a known location to a 6-char grid', () => {
    // Centroid of FN42 → should land back in FN42.
    expect(latLonToGrid(42.5, -71).startsWith('FN42')).toBe(true);
  });
});
