// REST helpers for the `/api/flowgraph` and `/api/blocks` endpoints.
// Mirrors the wire shapes in `server/src/routes.rs` and
// `server/src/block_schema.rs`.

import type { FlowgraphDoc } from '$lib/flowgraph';
import { ApiError } from '$lib/api/device';

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

/** GET /api/flowgraph — currently-applied preset doc, or `null` if the
 * server isn't in preset mode. */
export async function fetchFlowgraph(): Promise<FlowgraphDoc | null> {
  const r = await fetch('/api/flowgraph');
  if (r.status === 409) return null;
  if (!r.ok) throw new ApiError(r.status, `flowgraph fetch failed (${r.status})`);
  return (await r.json()) as FlowgraphDoc;
}

/** GET /api/blocks — sorted capability schema for every registered block. */
export async function fetchBlockSchemas(): Promise<BlockSchema[]> {
  const r = await fetch('/api/blocks');
  if (!r.ok) throw new ApiError(r.status, `blocks fetch failed (${r.status})`);
  return (await r.json()) as BlockSchema[];
}

/** PATCH /api/flowgraph — swap the running preset. Returns the reconfigure
 * plan the server applied; throws on 400 (rollback) or 409 (not preset mode). */
export async function patchFlowgraph(doc: FlowgraphDoc): Promise<ReconfigurePlan> {
  const r = await fetch('/api/flowgraph', {
    method: 'PATCH',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(doc),
  });
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `flowgraph patch failed (${r.status}): ${text}`);
  }
  return (await r.json()) as ReconfigurePlan;
}
