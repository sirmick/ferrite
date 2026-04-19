import { describe, expect, it } from 'vitest';
import { bumpDigit, digitsOfHz, parseFreq, placeValueAt } from './freqParse';

describe('parseFreq', () => {
  it('parses MHz suffix', () => {
    expect(parseFreq('1.25m')).toEqual({ hz: 1_250_000, unit: 'mhz' });
    expect(parseFreq('100.1MHz')).toEqual({ hz: 100_100_000, unit: 'mhz' });
  });

  it('parses GHz suffix', () => {
    expect(parseFreq('2.54g')).toEqual({ hz: 2_540_000_000, unit: 'ghz' });
    expect(parseFreq('1 GHz')).toEqual({ hz: 1_000_000_000, unit: 'ghz' });
  });

  it('parses kHz suffix', () => {
    expect(parseFreq('446000k')).toEqual({ hz: 446_000_000, unit: 'khz' });
    expect(parseFreq('1040 kHz')).toEqual({ hz: 1_040_000, unit: 'khz' });
  });

  it('parses Hz suffix', () => {
    expect(parseFreq('162550h')).toEqual({ hz: 162_550, unit: 'hz' });
    expect(parseFreq('60Hz')).toEqual({ hz: 60, unit: 'hz' });
  });

  it('defaults bare decimals to MHz', () => {
    expect(parseFreq('144.390')).toEqual({ hz: 144_390_000, unit: 'mhz' });
    expect(parseFreq('96.9')).toEqual({ hz: 96_900_000, unit: 'mhz' });
  });

  it('treats long bare integers as Hz', () => {
    expect(parseFreq('88100000')).toEqual({ hz: 88_100_000, unit: 'hz' });
    expect(parseFreq('1,040,000')).toEqual({ hz: 1_040_000, unit: 'hz' });
  });

  it('treats short bare integers as MHz', () => {
    expect(parseFreq('100')).toEqual({ hz: 100_000_000, unit: 'mhz' });
    expect(parseFreq('446')).toEqual({ hz: 446_000_000, unit: 'mhz' });
  });

  it('strips whitespace, commas, and underscores', () => {
    expect(parseFreq(' 1,234.567 m ')).toEqual({ hz: 1_234_567_000, unit: 'mhz' });
    expect(parseFreq('1_250_000h')).toEqual({ hz: 1_250_000, unit: 'hz' });
  });

  it('rejects garbage', () => {
    expect(parseFreq('')).toBeNull();
    expect(parseFreq('abc')).toBeNull();
    expect(parseFreq('1.2.3m')).toBeNull();
    expect(parseFreq('-5m')).toBeNull();
    expect(parseFreq('5xyz')).toBeNull();
  });
});

describe('digitsOfHz', () => {
  it('splits 10 decimal digits, GHz-first', () => {
    expect(digitsOfHz(1_234_567_890)).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9, 0]);
    expect(digitsOfHz(100_100_000)).toEqual([0, 1, 0, 0, 1, 0, 0, 0, 0, 0]);
    expect(digitsOfHz(0)).toEqual([0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
  });

  it('saturates at 9.999 GHz', () => {
    expect(digitsOfHz(99_999_999_999)).toEqual([9, 9, 9, 9, 9, 9, 9, 9, 9, 9]);
  });
});

describe('placeValueAt', () => {
  it('maps index 0 to 1 GHz and index 9 to 1 Hz', () => {
    expect(placeValueAt(0)).toBe(1_000_000_000);
    expect(placeValueAt(3)).toBe(1_000_000);
    expect(placeValueAt(6)).toBe(1_000);
    expect(placeValueAt(9)).toBe(1);
  });
});

describe('bumpDigit', () => {
  it('adds the place value of the target digit', () => {
    // 100.100 MHz → +1 at the 100-kHz position (index 4) → 100.200 MHz
    expect(bumpDigit(100_100_000, 4, +1)).toBe(100_200_000);
    // −1 at the MHz position (index 3)
    expect(bumpDigit(100_100_000, 3, -1)).toBe(99_100_000);
  });

  it('carries across higher digits', () => {
    // 99 + 1 at the tens digit (index 8) rolls to 109
    expect(bumpDigit(99, 8, +1)).toBe(109);
  });

  it('clamps at zero', () => {
    expect(bumpDigit(500, 7, -1)).toBe(400);
    expect(bumpDigit(50, 7, -1)).toBe(0);
  });

  it('clamps at 9_999_999_999', () => {
    expect(bumpDigit(9_999_999_990, 9, +1)).toBe(9_999_999_991);
    expect(bumpDigit(9_999_999_999, 9, +1)).toBe(9_999_999_999);
    expect(bumpDigit(9_000_000_000, 0, +1)).toBe(9_999_999_999);
  });

  it('accepts multi-step deltas', () => {
    // +5 at the 10-kHz position (index 5) → +50 000 Hz
    expect(bumpDigit(0, 5, +5)).toBe(50_000);
  });
});
