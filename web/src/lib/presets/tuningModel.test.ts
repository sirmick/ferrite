import { describe, expect, it } from 'vitest';
import { CONTINUOUS_STEP_HZ, DEFAULT_STEP_HZ, rangeAt, resolveTuning, snapHz } from './tuningModel';

describe('rangeAt', () => {
  it('maps frequencies to channelization ranges', () => {
    expect(rangeAt(1_000_000)?.mode).toBe('AM'); // MW
    expect(rangeAt(98_500_000)?.mode).toBe('WFM'); // FM
    expect(rangeAt(162_500_000)?.mode).toBe('NBFM'); // NOAA WX
    expect(rangeAt(462_600_000)?.step).toBe(12_500); // FRS/GMRS
  });

  it('returns undefined outside any range', () => {
    expect(rangeAt(70_000_000)).toBeUndefined(); // between airband and FM-ish gap
    expect(rangeAt(3_000_000_000)).toBeUndefined();
  });
});

describe('resolveTuning — auto', () => {
  const auto = { stepMode: 'auto', continuous: false };

  it('FM: 200 kHz step, snaps on the 100 kHz-offset grid', () => {
    const t = resolveTuning(98_500_000, auto);
    expect(t.stepHz).toBe(200_000);
    expect(t.snapGridHz).toBe(200_000);
    expect(t.snapOffsetHz).toBe(100_000);
  });

  it('MW AM: 10 kHz step + snap', () => {
    const t = resolveTuning(1_010_000, auto);
    expect(t.stepHz).toBe(10_000);
    expect(t.snapGridHz).toBe(10_000);
  });

  it('unknown band: default step, no snap', () => {
    const t = resolveTuning(70_000_000, auto);
    expect(t.stepHz).toBe(DEFAULT_STEP_HZ);
    expect(t.snapGridHz).toBeNull();
  });
});

describe('resolveTuning — off (snap disabled, step still works)', () => {
  it('FM band, stepMode off: 200 kHz step for arrows but no snap', () => {
    const t = resolveTuning(98_500_000, { stepMode: 'off', continuous: false });
    expect(t.stepHz).toBe(200_000);
    expect(t.snapGridHz).toBeNull();
  });
});

describe('resolveTuning — continuous (SSB/CW) forces no snap', () => {
  it('uses the fine continuous step and never snaps, even in a snappable band', () => {
    const t = resolveTuning(1_010_000, { stepMode: 'auto', continuous: true });
    expect(t.stepHz).toBe(CONTINUOUS_STEP_HZ);
    expect(t.snapGridHz).toBeNull();
  });
});

describe('resolveTuning — manual step', () => {
  it('fixed step overrides the band and snaps to that raster', () => {
    const t = resolveTuning(98_500_000, { stepMode: '25000', continuous: false });
    expect(t.stepHz).toBe(25_000);
    expect(t.snapGridHz).toBe(25_000);
    expect(t.snapOffsetHz).toBe(0);
  });

  it('garbage step mode falls back to default, no snap', () => {
    const t = resolveTuning(70_000_000, { stepMode: 'banana', continuous: false });
    expect(t.stepHz).toBe(DEFAULT_STEP_HZ);
  });
});

describe('snapHz', () => {
  it('rounds to the nearest grid multiple', () => {
    expect(snapHz(1_007_300, 10_000)).toBe(1_010_000);
    expect(snapHz(1_002_300, 10_000)).toBe(1_000_000);
  });

  it('honours the phase offset (US FM grid)', () => {
    // 98.437 MHz → nearest 200 kHz channel on the 100 kHz-offset grid
    // is 98.5 MHz, not 98.4.
    expect(snapHz(98_437_000, 200_000, 100_000)).toBe(98_500_000);
    expect(snapHz(87_950_000, 200_000, 100_000)).toBe(87_900_000);
  });

  it('is a no-op for a non-positive grid', () => {
    expect(snapHz(123_456, 0)).toBe(123_456);
  });
});
