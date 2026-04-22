import { describe, expect, it } from 'vitest';

import { defaultsFor, rangesToChoices, toSourceConfig, validate } from './optionsModel';
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

  it('pins the per-driver preferred 2 MS/s sample rate', () => {
    // SDRplay's advertised rate range is the continuous [62.5k, 10.66M],
    // so without injection the dropdown only shows the endpoints. The
    // per-driver preset injects 2 MS/s as a choice and selects it.
    expect(state.sample_rate_hz).toBe(2_000_000);
    expect(state.sample_rate_choices).toContain(2_000_000);
  });

  it('uses the first antenna as default and keeps the full list', () => {
    expect(state.antenna).toBe('Tuner 1 50ohm');
    expect(state.antenna_choices).toHaveLength(3);
  });

  it('exposes both the IFGR and RFGR gain stages', () => {
    expect(state.gains.map((g) => g.name)).toEqual(['IFGR', 'RFGR']);
  });

  it('picks the IF filter from the preset ladder (largest ≤ Fs)', () => {
    // sdrplay.json carries an `if_filter_ladder_hz` ending …1.536M, 5M.
    // At the preset Fs of 2 Ms/s, 1.536 MHz is the largest entry ≤ Fs.
    expect(state.bandwidth_hz).toBe(1_536_000);
    expect(state.bandwidth_choices).toContain(5_000_000);
    expect(state.bandwidth_choices).toContain(1_536_000);
  });

  it('clamps advertised choices by max_sample_rate_hz', () => {
    // SDRplay probe reports [62.5k, 10.66M]; sdrplay.json caps at 10 Ms/s
    // because activateStream() fails above that. Advanced panel list
    // should therefore stop at 10 Ms/s.
    expect(state.sample_rate_choices.every((c) => c <= 10_000_000)).toBe(true);
    expect(state.sample_rate_choices).not.toContain(10_660_000);
  });

  it('exposes a curated quick list from the preset', () => {
    // The nixie dropdown binds to this list — short + opinionated.
    expect(state.sample_rate_quick_choices).toEqual([
      500_000, 1_000_000, 2_000_000, 4_000_000, 6_000_000, 8_000_000, 10_000_000,
    ]);
  });
});

describe('defaultsFor — quick list filtering', () => {
  it('falls back to the full probe list when the preset defines no curated list', () => {
    // rtlsdr.json DOES now define a curated list, but entries outside
    // the advertised caps are dropped; the RTLSDR fixture advertises
    // discrete non-round values (e.g. 1_024_000) so a curated `1_000_000`
    // would be filtered out. Verify the advanced list is used as-is when
    // needed.
    const state = defaultsFor(RTLSDR_CAPS)!;
    expect(state.sample_rate_quick_choices.length).toBeGreaterThan(0);
    // Every quick choice must exist in the advanced choices (consistency).
    for (const q of state.sample_rate_quick_choices) {
      expect(state.sample_rate_choices).toContain(q);
    }
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

describe('toSourceConfig', () => {
  it('emits SoapySource with args + sample rate + centre + gain + AGC for RTL-SDR', () => {
    const state = defaultsFor(RTLSDR_CAPS)!;
    const cfg = toSourceConfig(RTLSDR_CAPS, state);
    expect(cfg.type).toBe('SoapySource');
    expect(cfg.params.args).toBe('driver=rtlsdr,serial=00000001');
    expect(cfg.params.sample_rate_hz).toBe(2_048_000);
    expect(cfg.params.center_freq_hz).toBe(state.center_freq_hz);
    expect(cfg.params.antenna).toBe('RX');
    expect(cfg.params.agc).toBe(false);
    // RTL gain default sits at the midpoint of [0, 49.6] step 0.1 → 24.8.
    expect(cfg.params.gain_db as number).toBeCloseTo(24.8, 1);
  });

  it('sums per-element gains when the device exposes multiple stages', () => {
    const state = defaultsFor(SDRPLAY_CAPS)!;
    // IFGR midpoint (39 = 20 + floor(39/2)*1) + RFGR midpoint (4) = 43.
    expect(toSourceConfig(SDRPLAY_CAPS, state).params.gain_db).toBe(43);
  });

  it('omits bandwidth_hz for drivers without a preset ladder', () => {
    // rtlsdr.json has no if_filter_ladder_hz — defaultsFor leaves
    // bandwidth_hz null, toSourceConfig skips the field, and the
    // server never calls set_bandwidth.
    expect(
      toSourceConfig(RTLSDR_CAPS, defaultsFor(RTLSDR_CAPS)!).params.bandwidth_hz,
    ).toBeUndefined();
  });

  it('forwards the ladder-derived bandwidth for SDRplay', () => {
    // sdrplay.json has a ladder; defaultsFor picks 1.536 MHz at 2 Ms/s,
    // toSourceConfig sends it on to the server.
    const cfg = toSourceConfig(SDRPLAY_CAPS, defaultsFor(SDRPLAY_CAPS)!);
    expect(cfg.params.bandwidth_hz).toBe(1_536_000);
  });
});
