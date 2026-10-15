import { describe, expect, it } from 'vitest';
import { fmCell, matches, type Emitter } from './store.svelte';

const em = (freq_hz: number, width_hz: number, peak_dbfs = -40): Emitter => ({
  id: 1,
  freq_hz,
  width_hz,
  level_dbfs: peak_dbfs,
  peak_dbfs,
  snr_db: 20,
  first_seen_ms: 0,
  last_seen_ms: 0,
  seen: 1,
  sources: [],
  status: 'new',
});

// Mirrors server/src/emitters.rs — if these drift, the receiver pane
// attributes detections to a different emitter than the server did.
describe('emitter matching (server rules)', () => {
  it('broadcast FM groups by 200 kHz channel cell', () => {
    expect(fmCell(99.1e6)).toBe(fmCell(99.18e6));
    expect(fmCell(99.1e6)).not.toBe(fmCell(99.3e6));
    expect(fmCell(150e6)).toBeNull();
    const station = em(99.1e6, 60e3);
    expect(matches(station, 99.17e6, 5e3, -70)).toBe(true); // a fragment
    expect(matches(station, 99.3e6, 5e3, -45)).toBe(false); // the next station
  });

  it('a weak HD sideband belongs to its wideband carrier', () => {
    expect(matches(em(98.5e6, 180e3, -40), 98.5e6 + 165e3, 70e3, -62)).toBe(true);
  });

  it('narrowband neighbours 25 kHz apart stay separate', () => {
    expect(matches(em(462.5625e6, 11e3), 462.5875e6, 11e3, -60)).toBe(false);
    expect(matches(em(462.5625e6, 11e3), 462.564e6, 11e3, -45)).toBe(true);
  });
});
