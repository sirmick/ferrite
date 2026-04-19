import { describe, expect, it } from 'vitest';

import { pixelToFreqLinear } from './waterfall';

describe('pixelToFreqLinear', () => {
  it('maps centre pixel to centre frequency', () => {
    expect(pixelToFreqLinear(500, 1000, 100_000_000, 2_000_000)).toBe(100_000_000);
  });

  it('maps left edge to centre - rate/2', () => {
    expect(pixelToFreqLinear(0, 1000, 100_000_000, 2_000_000)).toBe(99_000_000);
  });

  it('maps right edge to centre + rate/2', () => {
    expect(pixelToFreqLinear(1000, 1000, 100_000_000, 2_000_000)).toBe(101_000_000);
  });

  it('returns null for out-of-range pixels', () => {
    expect(pixelToFreqLinear(-1, 1000, 100_000_000, 2_000_000)).toBeNull();
    expect(pixelToFreqLinear(1001, 1000, 100_000_000, 2_000_000)).toBeNull();
  });

  it('returns null for non-positive width', () => {
    expect(pixelToFreqLinear(10, 0, 100_000_000, 2_000_000)).toBeNull();
    expect(pixelToFreqLinear(10, -5, 100_000_000, 2_000_000)).toBeNull();
  });
});
