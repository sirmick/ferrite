// Build-time validation for the SDR preset JSON files. Catches typos
// (`driver_keyt`) and schema drift (`bandwidth_hz` instead of the new
// `if_filter_ladder_hz`) at `pnpm test` rather than at runtime when a
// user opens the device dialog.

import { describe, expect, it } from 'vitest';

interface SettingOverride {
  label?: string;
  description?: string;
  hidden?: boolean;
  option_labels?: Record<string, string>;
}

interface AutoSetting {
  key: string;
  freq_bands_hz: Array<[number, number]>;
  value_in_band: string;
  value_out_of_band: string;
  rationale?: string;
}

interface Preset {
  driver_key: string;
  label?: string;
  sample_rate_hz: number;
  sample_rate_choices_hz?: number[];
  max_sample_rate_hz?: number;
  if_filter_ladder_hz?: number[];
  hidden_settings?: string[];
  setting_overrides?: Record<string, SettingOverride>;
  auto_settings?: AutoSetting[];
  gain_label?: string;
  gain_description?: string;
  gain_inverted?: boolean;
  notes?: string;
}

const KNOWN_FIELDS = new Set([
  'driver_key',
  'label',
  'sample_rate_hz',
  'sample_rate_choices_hz',
  'max_sample_rate_hz',
  'if_filter_ladder_hz',
  'hidden_settings',
  'setting_overrides',
  'auto_settings',
  'gain_label',
  'gain_description',
  'gain_inverted',
  'notes',
]);

const SETTING_OVERRIDE_FIELDS = new Set(['label', 'description', 'hidden', 'option_labels']);
const AUTO_SETTING_FIELDS = new Set([
  'key',
  'freq_bands_hz',
  'value_in_band',
  'value_out_of_band',
  'rationale',
]);

// Eager glob — same pattern optionsModel.ts uses so the test stays in
// sync with what the runtime actually loads.
const files = import.meta.glob<{ default: Preset }>('./*.json', { eager: true });

describe('sdr-presets schema', () => {
  it('discovers at least one preset file', () => {
    expect(Object.keys(files).length).toBeGreaterThan(0);
  });

  for (const [path, mod] of Object.entries(files)) {
    const name = path.replace(/^\.\//, '');
    const p = mod.default;

    describe(name, () => {
      it('has a non-empty string driver_key', () => {
        expect(typeof p.driver_key).toBe('string');
        expect(p.driver_key.length).toBeGreaterThan(0);
        // SoapySDR driver keys are lowercase short identifiers.
        expect(p.driver_key).toMatch(/^[a-z][a-z0-9]*$/);
      });

      it('filename matches driver_key', () => {
        expect(name).toBe(`${p.driver_key}.json`);
      });

      it('has a positive numeric sample_rate_hz', () => {
        expect(typeof p.sample_rate_hz).toBe('number');
        expect(p.sample_rate_hz).toBeGreaterThan(0);
      });

      it('has no unknown fields (catches typos)', () => {
        for (const key of Object.keys(p)) {
          expect(KNOWN_FIELDS, `unexpected field '${key}' in ${name}`).toContain(key);
        }
      });

      it('sample_rate_choices_hz (if present) contains positive numbers including sample_rate_hz', () => {
        if (p.sample_rate_choices_hz === undefined) return;
        expect(Array.isArray(p.sample_rate_choices_hz)).toBe(true);
        expect(p.sample_rate_choices_hz.length).toBeGreaterThan(0);
        for (const c of p.sample_rate_choices_hz) {
          expect(typeof c).toBe('number');
          expect(c).toBeGreaterThan(0);
        }
        expect(p.sample_rate_choices_hz).toContain(p.sample_rate_hz);
      });

      it('max_sample_rate_hz (if present) bounds both default + curated list', () => {
        if (p.max_sample_rate_hz === undefined) return;
        expect(p.sample_rate_hz).toBeLessThanOrEqual(p.max_sample_rate_hz);
        for (const c of p.sample_rate_choices_hz ?? []) {
          expect(c).toBeLessThanOrEqual(p.max_sample_rate_hz);
        }
      });

      it('hidden_settings (if present) is a list of non-empty strings', () => {
        if (p.hidden_settings === undefined) return;
        expect(Array.isArray(p.hidden_settings)).toBe(true);
        for (const k of p.hidden_settings) {
          expect(typeof k).toBe('string');
          expect(k.length).toBeGreaterThan(0);
        }
      });

      it('if_filter_ladder_hz (if present) is sorted-ascending unique positives', () => {
        if (p.if_filter_ladder_hz === undefined) return;
        expect(Array.isArray(p.if_filter_ladder_hz)).toBe(true);
        expect(p.if_filter_ladder_hz.length).toBeGreaterThan(0);
        for (let i = 0; i < p.if_filter_ladder_hz.length; i++) {
          expect(p.if_filter_ladder_hz[i]).toBeGreaterThan(0);
          if (i > 0) {
            expect(p.if_filter_ladder_hz[i]).toBeGreaterThan(p.if_filter_ladder_hz[i - 1]);
          }
        }
      });

      it('setting_overrides (if present) has only known per-key fields', () => {
        if (p.setting_overrides === undefined) return;
        expect(typeof p.setting_overrides).toBe('object');
        for (const [key, ov] of Object.entries(p.setting_overrides)) {
          expect(typeof key).toBe('string');
          expect(key.length, `${name}/setting_overrides: empty key`).toBeGreaterThan(0);
          for (const f of Object.keys(ov)) {
            expect(
              SETTING_OVERRIDE_FIELDS,
              `unexpected field '${f}' in ${name}.setting_overrides.${key}`,
            ).toContain(f);
          }
          if (ov.hidden !== undefined) expect(typeof ov.hidden).toBe('boolean');
          if (ov.option_labels !== undefined) {
            expect(typeof ov.option_labels).toBe('object');
            for (const [v, l] of Object.entries(ov.option_labels)) {
              expect(typeof v).toBe('string');
              expect(typeof l).toBe('string');
            }
          }
        }
      });

      it('auto_settings (if present) entries are well-formed', () => {
        if (p.auto_settings === undefined) return;
        expect(Array.isArray(p.auto_settings)).toBe(true);
        for (const rule of p.auto_settings) {
          for (const f of Object.keys(rule)) {
            expect(
              AUTO_SETTING_FIELDS,
              `unexpected field '${f}' in ${name}.auto_settings`,
            ).toContain(f);
          }
          expect(typeof rule.key).toBe('string');
          expect(rule.key.length).toBeGreaterThan(0);
          expect(typeof rule.value_in_band).toBe('string');
          expect(typeof rule.value_out_of_band).toBe('string');
          expect(Array.isArray(rule.freq_bands_hz)).toBe(true);
          expect(rule.freq_bands_hz.length).toBeGreaterThan(0);
          for (const band of rule.freq_bands_hz) {
            expect(Array.isArray(band)).toBe(true);
            expect(band.length).toBe(2);
            expect(band[0]).toBeGreaterThan(0);
            expect(band[1]).toBeGreaterThan(band[0]);
          }
        }
      });
    });
  }

  it('has no duplicate driver_keys across files', () => {
    const keys = Object.values(files).map((m) => m.default.driver_key);
    expect(new Set(keys).size).toBe(keys.length);
  });
});
