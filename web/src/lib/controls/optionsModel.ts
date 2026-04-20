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
  /** Available discrete sample rates (or synthesised quantised ladder). */
  sample_rate_choices: number[];
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

function spanCenter(ranges: RangeSpec[], fallback: number): number {
  const r = ranges[0];
  if (!r) return fallback;
  return (r.min + r.max) / 2;
}

/** Reasonable starting form state for `caps` — used on dialog open. */
export function defaultsFor(caps: DeviceCapabilities): OptionsState | null {
  const ch = firstChannel(caps);
  if (!ch) return null;

  const rateChoices = rangesToChoices(ch.sample_rate_ranges_hz);
  const sample_rate_hz = preferredRate(rateChoices);

  const bandwidthChoices = rangesToChoices(ch.bandwidth_ranges_hz);
  const bandwidth_hz = bandwidthChoices.length ? bandwidthChoices[0] : null;

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
 * Pick a sane default sample rate when the device exposes a ladder.
 * 2.048 MS/s is the long-standing RTL-SDR sweet spot; otherwise the
 * smallest choice ≥ 1 MS/s, falling back to the highest.
 */
function preferredRate(choices: number[]): number {
  if (choices.length === 0) return 2_048_000;
  const target = 2_048_000;
  if (choices.some((c) => Math.abs(c - target) < 1)) return target;
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
