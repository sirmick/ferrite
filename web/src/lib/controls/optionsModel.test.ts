import { describe, expect, it } from 'vitest';

import { defaultsFor, rangesToChoices, toOpenRequest, validate } from './optionsModel';
import { RTLSDR_CAPS, SDRPLAY_CAPS } from './__fixtures__/devices';

describe('rangesToChoices', () => {
  it('treats step=null ranges as their two endpoints', () => {
    const choices = rangesToChoices([{ min: 250_000, max: 3_200_000, step: null }]);
    expect(choices).toEqual([250_000, 3_200_000]);
  });

  it('expands stepped ranges into a sorted, deduped grid', () => {
    const choices = rangesToChoices([{ min: 100, max: 200, step: 50 }]);
    expect(choices).toEqual([100, 150, 200]);
  });

  it('caps stepped ranges at MAX_RATE_CHOICES (32) by striding', () => {
    const choices = rangesToChoices([{ min: 0, max: 1000, step: 1 }]);
    // 1001 candidates would be unmanageable; the projector strides them.
    expect(choices.length).toBeLessThanOrEqual(33);
    expect(choices[0]).toBe(0);
    expect(choices[choices.length - 1]).toBe(1000);
  });

  it('merges multiple input ranges into one sorted output', () => {
    const choices = rangesToChoices([
      { min: 1_024_000, max: 1_024_000, step: null },
      { min: 250_000, max: 250_000, step: null },
      { min: 2_048_000, max: 2_048_000, step: null },
    ]);
    expect(choices).toEqual([250_000, 1_024_000, 2_048_000]);
  });
});

describe('defaultsFor — RTL-SDR', () => {
  const state = defaultsFor(RTLSDR_CAPS)!;

  it('produces a non-null state', () => {
    expect(state).not.toBeNull();
  });

  it('prefers 2.048 MS/s when the device offers it', () => {
    expect(state.sample_rate_hz).toBe(2_048_000);
  });

  it('exposes every advertised sample rate as a choice', () => {
    expect(state.sample_rate_choices).toContain(250_000);
    expect(state.sample_rate_choices).toContain(2_048_000);
    expect(state.sample_rate_choices).toContain(3_200_000);
  });

  it('places centre frequency at the midpoint of the advertised range', () => {
    expect(state.center_freq_hz).toBe((24_000_000 + 1_766_000_000) / 2);
  });

  it('uses the only available antenna', () => {
    expect(state.antenna).toBe('RX');
    expect(state.antenna_choices).toEqual(['RX']);
  });

  it('exposes the tuner gain stage with the right range', () => {
    expect(state.gains).toHaveLength(1);
    expect(state.gains[0].name).toBe('TUNER');
    expect(state.gains[0].range).toEqual({ min: 0, max: 49.6, step: 0.1 });
  });

  it('reports AGC as available', () => {
    expect(state.has_agc).toBe(true);
    expect(state.agc).toBe(false);
  });

  it('omits bandwidth choices when the driver advertises none', () => {
    expect(state.bandwidth_choices).toEqual([]);
    expect(state.bandwidth_hz).toBeNull();
  });
});

describe('defaultsFor — SDRPlay', () => {
  const state = defaultsFor(SDRPLAY_CAPS)!;

  it('falls back to the smallest ≥ 1 MS/s when 2.048 MS/s is not exact', () => {
    // The SDRPlay range is continuous; its only "choices" are min and max.
    // Smallest ≥ 1 MS/s of {62500, 10_660_000} is 10_660_000.
    expect(state.sample_rate_hz).toBe(10_660_000);
  });

  it('uses the first antenna as default and keeps the full list', () => {
    expect(state.antenna).toBe('Tuner 1 50ohm');
    expect(state.antenna_choices).toHaveLength(3);
  });

  it('exposes both the IFGR and RFGR gain stages', () => {
    expect(state.gains.map((g) => g.name)).toEqual(['IFGR', 'RFGR']);
  });

  it('picks the smallest discrete bandwidth as the default', () => {
    expect(state.bandwidth_hz).toBe(200_000);
    expect(state.bandwidth_choices).toContain(8_000_000);
  });
});

describe('validate', () => {
  it('rejects out-of-range centre frequencies', () => {
    const state = defaultsFor(RTLSDR_CAPS)!;
    state.center_freq_hz = 12_000_000; // below 24 MHz
    const errors = validate(state);
    expect(errors.length).toBeGreaterThan(0);
    expect(errors[0]).toMatch(/centre frequency/i);
  });

  it('rejects out-of-range gain values', () => {
    const state = defaultsFor(RTLSDR_CAPS)!;
    state.gains[0].value_db = 100;
    expect(validate(state)).toEqual(expect.arrayContaining([expect.stringMatching(/TUNER gain/)]));
  });

  it('passes a fresh defaultsFor state', () => {
    expect(validate(defaultsFor(RTLSDR_CAPS)!)).toEqual([]);
    expect(validate(defaultsFor(SDRPLAY_CAPS)!)).toEqual([]);
  });
});

describe('toOpenRequest', () => {
  it('emits device_args + sample rate + centre + gain + AGC for RTL-SDR', () => {
    const state = defaultsFor(RTLSDR_CAPS)!;
    const req = toOpenRequest(RTLSDR_CAPS, state);
    expect(req.device_args).toBe('driver=rtlsdr,serial=00000001');
    expect(req.sample_rate_hz).toBe(2_048_000);
    expect(req.center_freq_hz).toBe(state.center_freq_hz);
    expect(req.antenna).toBe('RX');
    expect(req.agc).toBe(false);
    // RTL gain default sits at the midpoint of [0, 49.6] step 0.1 → 24.8.
    expect(req.gain_db).toBeCloseTo(24.8, 1);
  });

  it('sums per-element gains when the device exposes multiple stages', () => {
    const state = defaultsFor(SDRPLAY_CAPS)!;
    // IFGR midpoint (39 = 20 + floor(39/2)*1) + RFGR midpoint (4) = 43.
    expect(toOpenRequest(SDRPLAY_CAPS, state).gain_db).toBe(43);
  });

  it('omits bandwidth_hz when the device has no bandwidth ladder', () => {
    const state = defaultsFor(RTLSDR_CAPS)!;
    const req = toOpenRequest(RTLSDR_CAPS, state);
    expect(req.bandwidth_hz).toBeUndefined();
  });

  it('includes bandwidth_hz when the device offers one', () => {
    const state = defaultsFor(SDRPLAY_CAPS)!;
    const req = toOpenRequest(SDRPLAY_CAPS, state);
    expect(req.bandwidth_hz).toBe(200_000);
  });
});
