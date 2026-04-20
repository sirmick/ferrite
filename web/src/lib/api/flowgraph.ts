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
} & ParamKind;

export interface BlockSchema {
  type_name: string;
  placement: BlockPlacement;
  inputs: PortSchema[];
  outputs: PortSchema[];
  params: ParamSchema[];
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
}

/** GET /api/flowgraph — currently-applied preset doc. */
export async function fetchFlowgraph(): Promise<FlowgraphDoc> {
  const r = await fetch('/api/flowgraph');
  if (!r.ok) throw new ApiError(r.status, `flowgraph fetch failed (${r.status})`);
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
