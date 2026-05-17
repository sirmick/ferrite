import { describe, expect, it } from 'vitest';

import { LinearResampler, WHISPER_RATE } from './resample';

function sine(n: number, freqHz: number, rate: number): Float32Array {
  const out = new Float32Array(n);
  for (let i = 0; i < n; i++) out[i] = Math.sin((2 * Math.PI * freqHz * i) / rate);
  return out;
}

describe('LinearResampler', () => {
  it('passes 16 kHz through unchanged (identity)', () => {
    const r = new LinearResampler(WHISPER_RATE);
    expect(r.isIdentity).toBe(true);
    const x = sine(1000, 440, WHISPER_RATE);
    const y = r.feed(x);
    expect(y.length).toBe(x.length);
    expect(y[123]).toBeCloseTo(x[123], 6);
  });

  it('downsamples 48 kHz → 16 kHz at ~1/3 the length', () => {
    const r = new LinearResampler(48_000);
    const x = sine(48_000, 300, 48_000); // 1 s
    const y = r.feed(x);
    // ~16000 output samples for 1 s of audio at 16 kHz.
    expect(y.length).toBeGreaterThan(15_900);
    expect(y.length).toBeLessThan(16_100);
  });

  it('preserves a tone across chunk seams (no click/gap)', () => {
    // Feeding in two halves must match feeding the whole buffer — the
    // fractional phase + left-neighbour carry has to survive the seam.
    const src = 44_100;
    const whole = sine(src, 220, src);
    const a = new LinearResampler(src).feed(whole);

    const r = new LinearResampler(src);
    const half = whole.length >> 1;
    const b0 = r.feed(whole.subarray(0, half));
    const b1 = r.feed(whole.subarray(half));
    const b = Float32Array.from([...b0, ...b1]);

    // Lengths within a sample of each other …
    expect(Math.abs(a.length - b.length)).toBeLessThanOrEqual(1);
    // … and the seam region is continuous (no discontinuity spike).
    const seam = b0.length;
    let maxJump = 0;
    for (let i = seam - 4; i < seam + 4 && i + 1 < b.length; i++) {
      maxJump = Math.max(maxJump, Math.abs(b[i + 1] - b[i]));
    }
    // A 220 Hz tone at 16 kHz moves <0.1 per sample; a seam glitch
    // would be a large jump toward ±1.
    expect(maxJump).toBeLessThan(0.15);
  });
});
