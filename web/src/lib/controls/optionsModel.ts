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

/** Reasonable starting form state for `caps` — used on dialog open. */
export function defaultsFor(caps: DeviceCapabilities): OptionsState | null {
  const ch = firstChannel(caps);
  if (!ch) return null;

  const preset = PRESETS[caps.driver_key];
  // Inject the per-driver sweet-spot rate as a choice when it falls
  // inside an advertised continuous range. SDRplay advertises only
  // `[62.5k, 10.66M]` endpoints; without injection 2 MS/s would never
  // appear in the dropdown even though the device fully supports it.
  const rateChoicesRaw = withPreferredInjected(
    rangesToChoices(ch.sample_rate_ranges_hz),
    ch.sample_rate_ranges_hz,
    PREFERRED_RATE_BY_DRIVER[caps.driver_key],
  );
  // Clamp to the preset's practical upper bound when set — SDRplay
  // advertises 10.66 MS/s but activateStream() fails for anything above
  // 10 MS/s.
  const rateChoices = preset?.max_sample_rate_hz
    ? rateChoicesRaw.filter((c) => c <= preset.max_sample_rate_hz!)
    : rateChoicesRaw;
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

  const bandwidthChoices = rangesToChoices(ch.bandwidth_ranges_hz);
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
 * Inject `target` into a sorted choice list when it falls inside one of
 * the advertised ranges. No-op when the target is undefined or already
 * present (within 1 sample). Used to make the per-driver preferred rate
 * land in the dropdown for devices that only advertise endpoints.
 */
function withPreferredInjected(
  choices: number[],
  ranges: { min: number; max: number; step: number | null }[],
  target: number | undefined,
): number[] {
  if (target === undefined) return choices;
  if (choices.some((c) => Math.abs(c - target) < 1)) return choices;
  const inRange = ranges.some((r) => target >= r.min && target <= r.max);
  if (!inRange) return choices;
  return [...choices, target].sort((a, b) => a - b);
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

const PRESETS: Record<string, SdrPreset> = (() => {
  const files = import.meta.glob<{ default: SdrPreset }>('./sdr-presets/*.json', {
    eager: true,
  });
  const out: Record<string, SdrPreset> = {};
  for (const mod of Object.values(files)) {
    const p = mod.default;
    if (p && typeof p.driver_key === 'string') out[p.driver_key] = p;
  }
  return out;
})();

const PREFERRED_RATE_BY_DRIVER: Record<string, number> = Object.fromEntries(
  Object.entries(PRESETS).map(([k, p]) => [k, p.sample_rate_hz]),
);

/**
 * Pick a sane default sample rate. Prefer the per-driver sweet spot
 * (`PREFERRED_RATE_BY_DRIVER`) when the device advertises it; fall
 * back to the closest choice ≥ that target, then to the smallest
 * choice ≥ 1 MS/s, then to the highest.
 */
function preferredRate(choices: number[], driverKey: string): number {
  if (choices.length === 0) return PREFERRED_RATE_BY_DRIVER[driverKey] ?? 2_048_000;
  const target = PREFERRED_RATE_BY_DRIVER[driverKey] ?? 2_048_000;
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
