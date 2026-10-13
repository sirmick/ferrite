// REST helpers for the `/api/flowgraph` and `/api/blocks` endpoints.
// Mirrors the wire shapes in `server/src/routes.rs` and
// `server/src/block_schema.rs`.

import type { FlowgraphDoc } from '$lib/flowgraph';
import { ApiError } from '$lib/api/errors';

export type ReconfigScope = 'self' | 'downstream' | 'sourceRestart';
export type BlockPlacement = 'native' | 'browser' | 'either';

export interface PortSchema {
  name: string;
  port_type: string;
}

export type ParamKind =
  | { kind: 'range'; min: number; max: number; step: number; default: number; unit: string }
  | { kind: 'enum_numeric'; values: number[]; default: number; unit: string }
  | { kind: 'enum_string'; values: string[]; default: string }
  | { kind: 'toggle'; default: boolean }
  | { kind: 'text'; default: string };

export type ParamSchema = {
  key: string;
  label: string;
  reconfig_scope: ReconfigScope;
  /** AI-prompt + UI-tooltip prose. Empty during schema rollout. */
  ai_notes: string;
} & ParamKind;

export interface BlockSchema {
  type_name: string;
  placement: BlockPlacement;
  inputs: PortSchema[];
  outputs: PortSchema[];
  params: ParamSchema[];
  /** AI-prompt + UI-tooltip prose. Empty during schema rollout. */
  ai_notes: string;
}

export interface ParamChange {
  block_id: string;
  param_key: string;
  old_value: unknown;
  new_value: unknown;
  scope: ReconfigScope;
}

export interface ReconfigurePlan {
  overall: ReconfigScope;
  changes: ParamChange[];
  structural_count: number;
  noop: boolean;
}

/**
 * Post-apply snapshot of the driver state returned by `PATCH /api/source`
 * against a running SoapySource. Every field is optional — drivers vary
 * in what they implement, and the server omits fields the getter
 * refused. Used by the store to reconcile the user's optimistic write
 * against what the hardware actually accepted (AGC varying IFGR, driver
 * clamping BW to the ladder, tune-step rounding, etc).
 */
export interface SoapyReadback {
  sample_rate_hz?: number;
  center_freq_hz?: number;
  bandwidth_hz?: number;
  /** Realised gain in true dB. On SDRplay the LNA stage is counted at
   *  its per-band attenuation, so this is below `dial_db` once an LNA
   *  state is in. */
  gain_db?: number;
  /** The `gain_db` *param* value the hardware sits at (the dial) — what
   *  the slider follows. Same as `gain_db` on non-SDRplay drivers. */
  dial_db?: number;
  /** SDRplay LNA state, reported separately from the dB figure. */
  lna?: {
    hardware: string;
    state: number;
    states?: number;
    atten_db: number | null;
    band_hz?: [number, number];
  };
  agc?: boolean;
  antenna?: string;
  gain_elements?: Record<string, number>;
}

import type { SourceConfig } from './source';

/** Readback keys that are also `src` params — the ones `applyReadback`
 *  may overlay. The rest (`lna`, `dial_db`) are display-only and must not
 *  land in the params the next PATCH sends back (unknown keys are 400). */
const READBACK_PARAM_KEYS = [
  'sample_rate_hz',
  'center_freq_hz',
  'bandwidth_hz',
  'agc',
  'antenna',
  'gain_elements',
] as const;

/**
 * Overlay the driver readback on a source config: only keys the server
 * actually returned are overwritten — undefined fields mean "driver
 * didn't report this, keep the optimistic value." The gain slider is a
 * *dial*, so it follows `dial_db` (the position the `gain_db` param maps
 * to), not the true-dB `gain_db`. Leaves `type` alone; readback is
 * always about params.
 */
export function applyReadback(
  next: SourceConfig,
  readback: SoapyReadback | undefined,
): SourceConfig {
  if (!readback) return next;
  const merged: Record<string, unknown> = { ...next.params };
  for (const k of READBACK_PARAM_KEYS) {
    const v = readback[k];
    if (v !== undefined) merged[k] = v;
  }
  const dial = readback.dial_db ?? readback.gain_db;
  if (dial !== undefined) merged.gain_db = dial;
  return { type: next.type, params: merged };
}

/**
 * Wire shape of the PATCH /api/flowgraph and /api/source responses.
 * `applied: false` means the patch was stored but no pipeline was
 * running — the stored doc will take effect on the next start. The
 * remaining fields mirror `ReconfigurePlan` when `applied: true`.
 */
export interface ReconfigureResponse {
  applied: boolean;
  overall: ReconfigScope | null;
  changes: ParamChange[];
  structural_count: number;
  noop: boolean;
  /** Present on `PATCH /api/source` when the source is a SoapySource
   *  and the pipeline is running. `undefined` otherwise. */
  source_readback?: SoapyReadback;
}

/** GET /api/flowgraph — currently-applied preset doc. */
export async function fetchFlowgraph(): Promise<FlowgraphDoc> {
  const r = await fetch('/api/flowgraph');
  if (!r.ok) throw new ApiError(r.status, `flowgraph fetch failed (${r.status})`);
  return (await r.json()) as FlowgraphDoc;
}

/** GET /api/flowgraph/browser-half — the composed doc carved for the
 *  browser environment, for the in-browser runtime to run verbatim.
 *  Authoritative: derived from the *same* `compose_full` + split that
 *  produced the running node half, so the auto-inserted `WsBridge`
 *  stream IDs line up. Replaces the old client-side `composeSource` +
 *  `injectVoiceTranscribe` re-derivation, which diverged from the node
 *  under any non-`balanced` split. */
export async function fetchBrowserHalf(): Promise<FlowgraphDoc> {
  const r = await fetch('/api/flowgraph/browser-half');
  if (!r.ok) throw new ApiError(r.status, `browser-half fetch failed (${r.status})`);
  return (await r.json()) as FlowgraphDoc;
}

/** GET /api/blocks — sorted capability schema for every registered block. */
export async function fetchBlockSchemas(): Promise<BlockSchema[]> {
  const r = await fetch('/api/blocks');
  if (!r.ok) throw new ApiError(r.status, `blocks fetch failed (${r.status})`);
  return (await r.json()) as BlockSchema[];
}

const SCOPE_COST: Record<ReconfigScope, number> = {
  self: 0,
  downstream: 1,
  sourceRestart: 2,
};

function maxScope(a: ReconfigScope, b: ReconfigScope): ReconfigScope {
  return SCOPE_COST[a] >= SCOPE_COST[b] ? a : b;
}

export interface PredictedChange {
  block_id: string;
  param_key: string;
  old_value: unknown;
  new_value: unknown;
  scope: ReconfigScope;
}

export interface PredictedPlan {
  changes: PredictedChange[];
  structural_count: number;
  overall: ReconfigScope;
  noop: boolean;
}

/** Predict the reconfigure plan the server would produce for a doc swap,
 * using block schemas to look up each touched param's scope. Mirrors
 * `runtime/src/reconfigure.rs::diff_presets` — any structural change
 * forces `sourceRestart`, otherwise `overall` is the max scope across
 * changed params. Used by tests and the dialog's plan preview. */
export function predictScope(
  oldDoc: FlowgraphDoc,
  newDoc: FlowgraphDoc,
  schemas: BlockSchema[],
): PredictedPlan {
  const schemaByType = new Map(schemas.map((s) => [s.type_name, s]));
  const oldBlocks = oldDoc.blocks ?? {};
  const newBlocks = newDoc.blocks ?? {};

  const allIds = new Set<string>([...Object.keys(oldBlocks), ...Object.keys(newBlocks)]);
  let structural = 0;
  const changes: PredictedChange[] = [];

  for (const id of allIds) {
    const o = oldBlocks[id];
    const n = newBlocks[id];
    if (!o || !n || o.type !== n.type) {
      structural += 1;
      continue;
    }
    const spec = schemaByType.get(o.type);
    if (!spec) continue;
    const oParams = (o.params ?? {}) as Record<string, unknown>;
    const nParams = (n.params ?? {}) as Record<string, unknown>;
    for (const p of spec.params) {
      const ov = oParams[p.key];
      const nv = nParams[p.key];
      if (JSON.stringify(ov) === JSON.stringify(nv)) continue;
      changes.push({
        block_id: id,
        param_key: p.key,
        old_value: ov,
        new_value: nv,
        scope: p.reconfig_scope,
      });
    }
  }

  let overall: ReconfigScope = structural > 0 ? 'sourceRestart' : 'self';
  for (const c of changes) overall = maxScope(overall, c.scope);

  return {
    changes,
    structural_count: structural,
    overall,
    noop: structural === 0 && changes.length === 0,
  };
}

/** PATCH /api/flowgraph — store a new preset. Reconfigures the running
 * pipeline if there is one; otherwise the doc is queued for the next
 * start. Throws on 400 (reconfigure rollback). */
export async function patchFlowgraph(doc: FlowgraphDoc): Promise<ReconfigureResponse> {
  const r = await fetch('/api/flowgraph', {
    method: 'PATCH',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(doc),
  });
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `flowgraph patch failed (${r.status}): ${text}`);
  }
  return (await r.json()) as ReconfigureResponse;
}
