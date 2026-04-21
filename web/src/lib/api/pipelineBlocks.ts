// REST helpers for the `/api/pipeline/blocks` family. Mirrors the wire
// shapes emitted by `PipelineBlock` and `ReconfigureResponse` in
// `server/src/app_state.rs` + `server/src/routes.rs`.
//
// `setBlockParam(id, key, value)` is the single entry point the UI uses
// to move a DSP knob — every Range / Toggle / Enum control wires to it.
// Today every call routes through REST; when the browser-half runtime
// is wired in a follow-up commit, the dispatcher picks the WASM path
// for `placement: "browser"` blocks by calling
// `RuntimeHandle.reconfigureBlock(id, deltaJson)` instead.

import { ApiError } from '$lib/api/errors';
import type { BlockSchema, ReconfigureResponse } from '$lib/api/flowgraph';

export type BlockPlacement = 'node' | 'browser';

/** One entry from `GET /api/pipeline/blocks` — a live composed block
 *  with its spec and current param values. */
export interface PipelineBlock {
  id: string;
  type_name: string;
  placement: BlockPlacement;
  spec: BlockSchema;
  /** `null` when the preset omits params entirely. */
  values: Record<string, unknown> | null;
}

export async function fetchPipelineBlocks(): Promise<PipelineBlock[]> {
  const r = await fetch('/api/pipeline/blocks');
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `pipeline blocks fetch failed (${r.status}): ${text}`);
  }
  return (await r.json()) as PipelineBlock[];
}

/** POST /api/pipeline/blocks/:id/params — merge `delta` into one block's
 *  params and reconfigure the running pipeline. */
export async function patchBlockParams(
  id: string,
  delta: Record<string, unknown>,
): Promise<ReconfigureResponse> {
  const r = await fetch(`/api/pipeline/blocks/${encodeURIComponent(id)}/params`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(delta),
  });
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `block params patch failed (${r.status}): ${text}`);
  }
  return (await r.json()) as ReconfigureResponse;
}
