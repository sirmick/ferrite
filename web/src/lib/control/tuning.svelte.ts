// Central VFO tuning helpers — the single path every retune goes
// through (Nixie commit, ▲▼ buttons, Up/Down keys, spectrum click).
//
// Before this the VFO split (`abs → channelizer freq_shift`) was
// copy-pasted in RfQuickBar and Spectrum; both now call `tuneVfoTo`.
// Snap is applied to the *user-facing* absolute frequency here, then
// the abs→shift split runs — so snapping never fights the DC-spike
// off-tune dodge, which lives in the split layer.

import { pipeline, currentAxes } from '$lib/pipeline.svelte';
import { clientControls } from '$lib/control/clientStore.svelte';
import { resolveTuning, snapHz, type Tuning } from '$lib/presets/tuningModel';

/** Demods with no channel grid — continuous tuning, snap force-off. */
const CONTINUOUS_DEMODS = new Set(['SsbDemod', 'MorseDemod']);

interface VfoState {
  absHz: number;
  shiftHz: number;
  centerHz: number;
  halfSpanHz: number;
  vfoBlockId: string | null;
}

/** Resolve the live VFO state from the pipeline. Returns null when no
 *  axes yet (pipeline not running). */
export function vfoState(): VfoState | null {
  const axes = currentAxes(pipeline);
  if (!axes) return null;
  const vfoBlock = Object.values(pipeline.blocks).find((b) =>
    b.spec.params.some((p) => p.key === 'freq_shift_hz'),
  );
  const values = (vfoBlock?.values as Record<string, unknown> | undefined) ?? undefined;
  const shiftHz = typeof values?.freq_shift_hz === 'number' ? values.freq_shift_hz : 0;
  return {
    absHz: axes.center_freq_hz + shiftHz,
    shiftHz,
    centerHz: axes.center_freq_hz,
    halfSpanHz: axes.sample_rate_hz / 2,
    vfoBlockId: vfoBlock?.id ?? null,
  };
}

/** True when the active demod tunes continuously (SSB/CW) — disables
 *  snap regardless of the band. */
export function presetContinuous(): boolean {
  return Object.values(pipeline.blocks).some((b) => CONTINUOUS_DEMODS.has(b.spec.type_name));
}

/** Current `{ stepHz, snapGridHz, … }` for the live VFO. Safe to call
 *  from a `$derived` (reads reactive pipeline + clientControls). */
export function currentTuning(): Tuning {
  const v = vfoState();
  const absHz = v?.absHz ?? 0;
  return resolveTuning(absHz, {
    stepMode: clientControls.get('client.tuning.stepMode'),
    continuous: presetContinuous(),
  });
}

/** Move the VFO to `absHz` (snapped if a grid is resolved). Routes
 *  through `pipeline.tune` → `POST /api/tune`, so the server makes the
 *  single decision about whether to retune the source centre or just
 *  shift the channelizer, applies the per-driver DC-spike dodge, and
 *  reports a unified ReconfigureResponse. The old two-step
 *  `applyControl(center_freq_hz)` / `applyControl(freq_shift_hz)`
 *  split lived here before — server now owns the math. */
export function tuneVfoTo(absHz: number): void {
  if (!vfoState()) return;
  const t = currentTuning();
  const target =
    t.snapGridHz !== null ? snapHz(absHz, t.snapGridHz, t.snapOffsetHz) : Math.round(absHz);
  void pipeline.tune(target);
}

/** Step the VFO one increment up (`+1`) or down (`-1`). */
export function stepVfo(dir: 1 | -1): void {
  const v = vfoState();
  if (!v) return;
  const { stepHz } = currentTuning();
  tuneVfoTo(v.absHz + dir * stepHz);
}
