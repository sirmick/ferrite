// Form state + helpers for the per-device options dialog (#64).
//
// Pure functions over `DeviceCapabilities` — no Svelte runtime dependency
// so it stays unit-testable (#69).

import type { SourceConfig } from '$lib/api/source';
import {
  deviceArgsString,
  type DeviceCapabilities,
  type RangeSpec,
  type RxChannelCapabilities,
} from '$lib/api/devices';

/**
 * One slider-shaped numeric knob. Pre-resolved range + step so the UI
 * doesn't have to reason about Soapy's `step: null = continuous` quirk.
 */
export interface NumericKnob {
  min: number;
  max: number;
  /** Always > 0; `1` for continuous ranges so HTML inputs accept any value. */
  step: number;
}

/** Per-element gain slider state. */
export interface GainState {
  name: string;
  value_db: number;
  range: NumericKnob;
}

/**
 * Editable form state for one Rx channel. Defaults come from
 * [`defaultsFor`]; user edits replace fields in-place.
 */
export interface OptionsState {
  channel: number;
  sample_rate_hz: number;
  /**
   * Full discrete list from the device probe, clamped by the preset's
   * `max_sample_rate_hz` where set. Bound by the advanced panel.
   */
  sample_rate_choices: number[];
  /**
   * Short curated list from the preset — the headline dropdown next to
   * the main tuning display shows just these. Empty when the preset
   * defines no curated list (falls back to `sample_rate_choices`).
   */
  sample_rate_quick_choices: number[];
  center_freq_hz: number;
  freq_range: NumericKnob;
  bandwidth_hz: number | null;
  bandwidth_choices: number[];
  antenna: string | null;
  antenna_choices: string[];
  agc: boolean;
  has_agc: boolean;
  gains: GainState[];
}

const MAX_RATE_CHOICES = 32;

/** Resolve `RangeSpec` into a usable NumericKnob (continuous → step 1). */
export function knobFromRange(r: RangeSpec): NumericKnob {
  return { min: r.min, max: r.max, step: r.step && r.step > 0 ? r.step : 1 };
}

/**
 * Project a list of `RangeSpec` into a flat list of discrete choices for a
 * select. Stepped ranges expand to their grid (capped at MAX_RATE_CHOICES);
 * continuous ranges contribute their two endpoints. The result is sorted +
 * deduped.
 */
export function rangesToChoices(ranges: RangeSpec[]): number[] {
  const set = new Set<number>();
  for (const r of ranges) {
    if (r.step && r.step > 0) {
      const count = Math.floor((r.max - r.min) / r.step) + 1;
      const stride = Math.max(1, Math.ceil(count / MAX_RATE_CHOICES));
      for (let i = 0; i < count; i += stride) {
        set.add(r.min + i * r.step);
      }
      set.add(r.max);
    } else {
      set.add(r.min);
      set.add(r.max);
    }
  }
  return [...set].sort((a, b) => a - b);
}

/** Pick the first channel; Ferrite is single-channel for v0.1. */
export function firstChannel(caps: DeviceCapabilities): RxChannelCapabilities | null {
  return caps.rx_channels[0] ?? null;
}

/**
 * True when `target` is reachable from the advertised ranges. Continuous
 * ranges (step null) count any value in `[min, max]`; stepped ranges are
 * expanded via `rangesToChoices`. Used to filter curated preset rates
 * against whatever the device probe actually reports.
 */
function rateContains(ranges: RangeSpec[], target: number): boolean {
  for (const r of ranges) {
    if (!r.step || r.step <= 0) {
      if (target >= r.min && target <= r.max) return true;
    } else {
      const offset = target - r.min;
      if (offset < 0 || target > r.max) continue;
      const grid = offset % r.step;
      if (grid < 1 || r.step - grid < 1) return true;
    }
  }
  return false;
}

function spanCenter(ranges: RangeSpec[], fallback: number): number {
  const r = ranges[0];
  if (!r) return fallback;
  return (r.min + r.max) / 2;
}

/**
 * Full sample-rate choices for the advanced panel: the device probe's
 * advertised values, plus the per-driver preferred rate injected when
 * it's inside a continuous range, clamped by the preset's
 * `max_sample_rate_hz` where set (SDRplay advertises 10.66 MS/s but
 * activateStream fails above 10 MS/s, so we clamp).
 *
 * Exported so the live-reconfig dropdown in `InputControls.svelte`
 * uses the same rule as the dialog's `defaultsFor`; without this the
 * live panel rendered the raw two-endpoint probe list for a continuous
 * range — e.g. `[62_500, 10_660_000]` on SDRplay — and any user click
 * on that dropdown landed at the unreachable ceiling.
 */
export function fullRateChoices(caps: DeviceCapabilities): number[] {
  const ch = firstChannel(caps);
  if (!ch) return [];
  const preset = lookupPreset(caps.driver_key);
  // Start from the probe — the authoritative capability list. For
  // continuous ranges (SDRplay: 62.5k–10.66M) this is just two endpoints;
  // for stepped ranges (HackRF: 1M–20M step 1M) it's the full grid.
  const set = new Set<number>(rangesToChoices(ch.sample_rate_ranges_hz));
  // Inject the curated list so the advanced panel is a superset of the
  // header's "quick" list. Without this step SDRplay's advanced dropdown
  // had just [62.5k, 2M] (continuous-range endpoints + the preset's
  // preferred default) — picking 6/8/10 MS/s in the header left the
  // advanced <select> with no matching <option>, and the browser fell
  // back to showing the first entry. Every curated rate also has to be
  // reachable from the probe (continuous ranges count anything in
  // [min, max]; stepped ranges need an exact match) so a stale preset
  // can't offer an unreachable rate.
  for (const c of preset?.sample_rate_choices_hz ?? []) {
    if (rateContains(ch.sample_rate_ranges_hz, c)) set.add(c);
  }
  // Inject the per-driver preferred rate when it falls inside a range —
  // harmless if it's already in the curated list, important on drivers
  // that curate no list at all.
  const preferred = preferredRateFor(caps.driver_key);
  if (preferred !== undefined && rateContains(ch.sample_rate_ranges_hz, preferred)) {
    set.add(preferred);
  }
  const maxRate = preset?.max_sample_rate_hz ?? Number.POSITIVE_INFINITY;
  return [...set].filter((c) => c <= maxRate).sort((a, b) => a - b);
}

/**
 * Curated short list for the **headline** sample-rate dropdown (the
 * one next to the nixies). When the preset defines
 * `sample_rate_choices_hz` we keep only entries that are both:
 * - reachable from the device's advertised ranges, and
 * - at or below the preset's `max_sample_rate_hz` clamp.
 *
 * When the preset has no curated list we fall back to the full probe
 * list so the UI still offers *something* — but that path should be
 * rare since every shipped preset has a curated list.
 */
export function quickRateChoices(caps: DeviceCapabilities): number[] {
  const ch = firstChannel(caps);
  if (!ch) return [];
  const preset = lookupPreset(caps.driver_key);
  const full = fullRateChoices(caps);
  if (!preset?.sample_rate_choices_hz || preset.sample_rate_choices_hz.length === 0) {
    return full;
  }
  const maxRate = preset.max_sample_rate_hz ?? Number.POSITIVE_INFINITY;
  return preset.sample_rate_choices_hz.filter(
    (c) => c <= maxRate && rateContains(ch.sample_rate_ranges_hz, c),
  );
}

/**
 * Settings keys the advanced panel should suppress for this driver.
 * Returns an empty list when the preset has nothing hidden (or no
 * preset exists). See `SdrPreset.hidden_settings`.
 */
export function hiddenSettingsFor(caps: DeviceCapabilities): string[] {
  return lookupPreset(caps.driver_key)?.hidden_settings ?? [];
}

/**
 * Ladder-derived bandwidth for this driver given a sample rate. Returns
 * `null` when the preset has no ladder (RTL-SDR, HackRF, …) — caller
 * should omit `bandwidth_hz` from the patch and let the driver keep its
 * own behaviour. Used by the header's quick sample-rate dropdown to
 * apply `{sample_rate_hz, bandwidth_hz}` atomically, matching the rule
 * `defaultsFor` uses on device-open.
 */
export function bandwidthForRate(caps: DeviceCapabilities, rateHz: number): number | null {
  const ladder = lookupPreset(caps.driver_key)?.if_filter_ladder_hz;
  return pickFromLadder(ladder, rateHz);
}

/**
 * Bandwidth choices for the advanced panel. When the preset carries an
 * `if_filter_ladder_hz`, use it (that's the authoritative list of
 * filters the hardware actually has). Otherwise fall back to the
 * device probe. Result is sorted ascending, no duplicates.
 */
export function fullBandwidthChoices(caps: DeviceCapabilities): number[] {
  const ch = firstChannel(caps);
  if (!ch) return [];
  const preset = lookupPreset(caps.driver_key);
  if (preset?.if_filter_ladder_hz && preset.if_filter_ladder_hz.length > 0) {
    return [...preset.if_filter_ladder_hz].sort((a, b) => a - b);
  }
  return rangesToChoices(ch.bandwidth_ranges_hz);
}

/** Reasonable starting form state for `caps` — used on dialog open. */
export function defaultsFor(caps: DeviceCapabilities): OptionsState | null {
  const ch = firstChannel(caps);
  if (!ch) return null;

  const preset = lookupPreset(caps.driver_key);
  const rateChoices = fullRateChoices(caps);
  const sample_rate_hz = preferredRate(rateChoices, caps.driver_key);
  // Curated quick list — nixie dropdown binds here. Falls back to the
  // full probe list when the preset doesn't curate. Entries must be
  // reachable from the advertised ranges (continuous ranges count
  // anything inside [min, max], discrete ranges need an exact match)
  // and under `max_sample_rate_hz` if set — so a stale preset can't
  // offer an unreachable rate.
  const maxRate = preset?.max_sample_rate_hz ?? Number.POSITIVE_INFINITY;
  const sample_rate_quick_choices =
    preset?.sample_rate_choices_hz?.filter(
      (c) => c <= maxRate && rateContains(ch.sample_rate_ranges_hz, c),
    ) ?? rateChoices;

  const bandwidthChoices = fullBandwidthChoices(caps);
  // Driver-specific: some drivers (SDRplay) need an explicit IF filter
  // picked for them because the driver default is either too narrow
  // (brick-walls the waterfall) or too wide (silent upclock of Fs).
  // Others (HackRF, RTL-SDR) do the right thing without a
  // `set_bandwidth` call. The ladder lives in the per-driver preset
  // file so the rule is "has a ladder? derive; missing? skip", with no
  // per-driver code paths.
  const bandwidth_hz = pickFromLadder(preset?.if_filter_ladder_hz, sample_rate_hz);

  const freqRange = ch.frequency_ranges_hz[0] ?? { min: 0, max: 6e9, step: null };

  const gains: GainState[] = ch.gains.map((g) => ({
    name: g.name,
    value_db: midOfRange(g.range_db),
    range: knobFromRange(g.range_db),
  }));

  return {
    channel: ch.index,
    sample_rate_hz,
    sample_rate_choices: rateChoices,
    sample_rate_quick_choices,
    center_freq_hz: spanCenter(ch.frequency_ranges_hz, 100e6),
    freq_range: knobFromRange(freqRange),
    bandwidth_hz,
    bandwidth_choices: bandwidthChoices,
    antenna: ch.antennas[0] ?? null,
    antenna_choices: ch.antennas,
    agc: false,
    has_agc: ch.has_agc,
    gains,
  };
}

function midOfRange(r: RangeSpec): number {
  if (r.step && r.step > 0) {
    const count = Math.floor((r.max - r.min) / r.step);
    return r.min + Math.floor(count / 2) * r.step;
  }
  return (r.min + r.max) / 2;
}

/**
 * Per-driver opening presets, loaded eagerly from `sdr-presets/*.json`.
 * One file per driver pins a sweet-spot sample rate and, when the
 * driver's IF filter behaviour warrants it, an ordered ladder of filter
 * widths. See `sdr-presets/README.md`.
 *
 * Vite's `import.meta.glob({ eager: true })` inlines the JSON at build
 * time, so this is just a static lookup at runtime.
 */
interface SdrPreset {
  driver_key: string;
  /** Default sample rate on dialog-open. Must be in the advertised caps. */
  sample_rate_hz: number;
  /**
   * Curated short list for the main "quick" dropdown. Every entry must
   * also be reachable from the device probe — entries outside the
   * advertised ranges are filtered out so a stale preset can't offer an
   * unreachable rate.
   */
  sample_rate_choices_hz?: number[];
  /**
   * Practical upper bound, tighter than the device's advertised max.
   * SDRplay's `[2M, 10.66M]` range is clamped here because the driver
   * rejects activateStream() above ~10 MS/s.
   */
  max_sample_rate_hz?: number;
  /** IF filter ladder for drivers where BW must track Fs — see `pickFromLadder`. */
  if_filter_ladder_hz?: number[];
  /**
   * `getSettingInfo` keys to suppress in the advanced panel. Used when a
   * driver surfaces the same underlying knob twice — e.g. SDRplay's
   * `rfgain_sel` setting duplicates the `RFGR` gain element. Keeps the
   * "one control per capability" rule without adding device-specific code
   * to the frontend: the JSON names the redundant keys and TS filters.
   */
  hidden_settings?: string[];
}

/**
 * Pick the largest ladder entry that does not exceed `fs`. A filter
 * wider than Fs either aliases (HackRF) or makes the driver silently
 * raise Fs to match (SDRplay); staying ≤ Fs avoids both. Returns `null`
 * when the ladder is absent or empty, which the source block treats as
 * "skip `set_bandwidth`" (rtlsdr, hackrf).
 */
function pickFromLadder(ladder: number[] | undefined, fs: number): number | null {
  if (!ladder || ladder.length === 0) return null;
  let best: number | null = null;
  for (const bw of ladder) {
    if (bw <= fs && (best === null || bw > best)) best = bw;
  }
  return best;
}

/**
 * Preset map keyed by **lowercased** `driver_key` — SoapySDR drivers
 * are inconsistent about casing (`SDRplay`, `HackRF`, `RTLSDR` come
 * out capitalised while `airspy`, `bladerf`, `lime`, `plutosdr` are
 * lowercase) but our preset filenames are uniformly lowercase for
 * sanity. All lookups go through [`lookupPreset`] which normalises.
 */
const PRESETS: Record<string, SdrPreset> = (() => {
  const files = import.meta.glob<{ default: SdrPreset }>('./sdr-presets/*.json', {
    eager: true,
  });
  const out: Record<string, SdrPreset> = {};
  for (const mod of Object.values(files)) {
    const p = mod.default;
    if (p && typeof p.driver_key === 'string') out[p.driver_key.toLowerCase()] = p;
  }
  return out;
})();

function lookupPreset(driverKey: string): SdrPreset | undefined {
  return PRESETS[driverKey.toLowerCase()];
}

const PREFERRED_RATE_BY_DRIVER: Record<string, number> = Object.fromEntries(
  Object.entries(PRESETS).map(([k, p]) => [k, p.sample_rate_hz]),
);

function preferredRateFor(driverKey: string): number | undefined {
  return PREFERRED_RATE_BY_DRIVER[driverKey.toLowerCase()];
}

/**
 * Pick a sane default sample rate. Prefer the per-driver sweet spot
 * (`PREFERRED_RATE_BY_DRIVER`) when the device advertises it; fall
 * back to the closest choice ≥ that target, then to the smallest
 * choice ≥ 1 MS/s, then to the highest.
 */
function preferredRate(choices: number[], driverKey: string): number {
  const target = preferredRateFor(driverKey) ?? 2_048_000;
  if (choices.length === 0) return target;
  const exact = choices.find((c) => Math.abs(c - target) < 1);
  if (exact !== undefined) return exact;
  const above = choices.find((c) => c >= target);
  if (above !== undefined) return above;
  const above1m = choices.find((c) => c >= 1_000_000);
  return above1m ?? choices[choices.length - 1];
}

/** True when `state` is internally consistent (freq in range, gains in range). */
export function validate(state: OptionsState): string[] {
  const errors: string[] = [];
  if (state.center_freq_hz < state.freq_range.min || state.center_freq_hz > state.freq_range.max) {
    errors.push(
      `centre frequency ${state.center_freq_hz.toFixed(0)} Hz outside [${state.freq_range.min}, ${state.freq_range.max}]`,
    );
  }
  for (const g of state.gains) {
    if (g.value_db < g.range.min || g.value_db > g.range.max) {
      errors.push(`${g.name} gain ${g.value_db} dB outside [${g.range.min}, ${g.range.max}]`);
    }
  }
  return errors;
}

/**
 * Sum the per-element gains into a single overall gain figure (Soapy
 * accepts a per-element map but the open API is single-valued for now).
 * Returns `null` when the device has no gains exposed.
 */
function summarisedGain(state: OptionsState): number | null {
  if (state.gains.length === 0) return null;
  return state.gains.reduce((acc, g) => acc + g.value_db, 0);
}

/** Build a SourceConfig targeting `SoapySource` for the PATCH /api/source
 * path. The preset's `src` hints (center_freq_hz, sample_rate_hz) are
 * overridden by this config via `compose_source`. */
export function toSourceConfig(caps: DeviceCapabilities, state: OptionsState): SourceConfig {
  const params: Record<string, unknown> = {
    sample_rate_hz: state.sample_rate_hz,
    center_freq_hz: state.center_freq_hz,
    args: deviceArgsString(caps.info),
  };
  if (state.bandwidth_hz !== null) params.bandwidth_hz = state.bandwidth_hz;
  if (state.antenna) params.antenna = state.antenna;
  if (state.has_agc) params.agc = state.agc;
  const gain = summarisedGain(state);
  if (gain !== null) params.gain_db = gain;
  return { type: 'SoapySource', params };
}
