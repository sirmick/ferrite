// Pure tuning-step / snap resolver. No Svelte, no pipeline — just data
// in, `{ step, snap-grid }` out, so it unit-tests cleanly.
//
// Inputs that decide the step/grid:
//  - the VFO's absolute frequency, mapped to a channelization range in
//    `bandplan-steps.json` (our clean range table; the vendored
//    `bandplan-usa.json` is used elsewhere only for display labels);
//  - the user's step-mode pick ('auto' or a fixed Hz string);
//  - whether the active demod is *continuous* (SSB/CW) — those modes
//    have no channel grid, so snapping is force-disabled regardless of
//    the band.

import raw from './bandplan-steps.json';

export interface StepRange {
  from: number;
  to: number;
  step: number;
  snap: boolean;
  gridOffsetHz: number;
  mode?: string;
}

interface RawFile {
  default_step_hz: number;
  continuous_step_hz: number;
  ranges: ReadonlyArray<{
    from: number;
    to: number;
    step: number;
    snap: boolean;
    grid_offset_hz?: number;
    mode?: string;
  }>;
}

const file = raw as RawFile;

export const DEFAULT_STEP_HZ = file.default_step_hz;
export const CONTINUOUS_STEP_HZ = file.continuous_step_hz;

/** Ranges sorted low→high (authoring order is already sorted, but don't
 *  rely on it). */
export const stepRanges: ReadonlyArray<StepRange> = file.ranges
  .map((r) => ({
    from: r.from,
    to: r.to,
    step: r.step,
    snap: r.snap,
    gridOffsetHz: r.grid_offset_hz ?? 0,
    mode: r.mode,
  }))
  .slice()
  .sort((a, b) => a.from - b.from);

/** First channelization range containing `hz`, or undefined. */
export function rangeAt(hz: number): StepRange | undefined {
  for (const r of stepRanges) {
    if (hz < r.from) break;
    if (hz <= r.to) return r;
  }
  return undefined;
}

export interface TuningOpts {
  /** `'off'` (free tuning, no snap), `'auto'` (band-derived step +
   *  snap), or a fixed step in Hz as a string ('1000', '12500', …).
   *  `'off'` replaces the old separate snap toggle: arrows still step
   *  by the band-natural amount, but the VFO is never grid-snapped. */
  stepMode: string;
  /** Active demod is SSB/CW or otherwise has no channel grid. */
  continuous: boolean;
}

export interface Tuning {
  /** VFO increment for one step (Up/Down, ▲▼ buttons). */
  stepHz: number;
  /** Grid to round the VFO onto, or null = don't snap. */
  snapGridHz: number | null;
  /** Phase offset for the snap grid (e.g. US FM 100 kHz). */
  snapOffsetHz: number;
  /** Band mode hint for a UI chip, if known. */
  mode?: string;
}

function parseFixed(stepMode: string): number | null {
  if (stepMode === 'auto' || stepMode === 'off') return null;
  const n = Number(stepMode);
  return Number.isFinite(n) && n > 0 ? n : null;
}

/** Resolve the step + snap grid for a VFO frequency. */
export function resolveTuning(hz: number, opts: TuningOpts): Tuning {
  const rule = rangeAt(hz);
  const fixed = parseFixed(opts.stepMode);
  const snapEnabled = opts.stepMode !== 'off' && !opts.continuous;

  let stepHz: number;
  if (fixed !== null) {
    stepHz = fixed;
  } else if (opts.continuous) {
    stepHz = CONTINUOUS_STEP_HZ;
  } else {
    // 'off' and 'auto' both step by the band-natural amount; 'off'
    // just doesn't snap.
    stepHz = rule?.step ?? DEFAULT_STEP_HZ;
  }

  let snapGridHz: number | null;
  let snapOffsetHz = 0;
  if (!snapEnabled) {
    snapGridHz = null;
  } else if (fixed !== null) {
    // Manual step: snap to that step's grid (no band phase offset —
    // the user picked an explicit raster).
    snapGridHz = fixed;
  } else if (rule && rule.snap) {
    snapGridHz = rule.step;
    snapOffsetHz = rule.gridOffsetHz;
  } else {
    snapGridHz = null;
  }

  return { stepHz, snapGridHz, snapOffsetHz, mode: rule?.mode };
}

/** Round `hz` to the nearest `grid` multiple, phased by `offset`. */
export function snapHz(hz: number, grid: number, offset = 0): number {
  if (!(grid > 0)) return hz;
  return Math.round((hz - offset) / grid) * grid + offset;
}
