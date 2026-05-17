import { describe, expect, it } from 'vitest';

import { EnergyVadSegmenter } from './vad';

const RATE = 16_000;

function silence(ms: number): Float32Array {
  return new Float32Array(Math.round((ms / 1000) * RATE));
}
function tone(ms: number, amp = 0.3): Float32Array {
  const n = Math.round((ms / 1000) * RATE);
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) out[i] = amp * Math.sin((2 * Math.PI * 350 * i) / RATE);
  return out;
}

describe('EnergyVadSegmenter', () => {
  it('emits exactly one segment for silence → speech → silence', () => {
    const segs: Float32Array[] = [];
    const v = new EnergyVadSegmenter({ minSpeechMs: 200, hangoverMs: 300 }, (s) => segs.push(s));
    // Prime the noise floor on real silence first, then an utterance,
    // then enough trailing silence to exceed the hangover and flush.
    v.feed(silence(800));
    v.feed(tone(1200));
    v.feed(silence(800));
    expect(segs.length).toBe(1);
    // The captured utterance is roughly the tone length (+ hangover
    // tail, − the frames before the gate opened).
    const ms = (segs[0].length / RATE) * 1000;
    expect(ms).toBeGreaterThan(900);
    expect(ms).toBeLessThan(2000);
  });

  it('drops a sub-minimum blip (key click / splatter)', () => {
    const segs: Float32Array[] = [];
    const v = new EnergyVadSegmenter({ minSpeechMs: 400, hangoverMs: 250 }, (s) => segs.push(s));
    v.feed(silence(600));
    v.feed(tone(120)); // shorter than minSpeechMs
    v.feed(silence(600));
    expect(segs.length).toBe(0);
  });

  it('does not fire on pure silence', () => {
    const segs: Float32Array[] = [];
    const v = new EnergyVadSegmenter({}, (s) => segs.push(s));
    v.feed(silence(3000));
    expect(segs.length).toBe(0);
  });
});
