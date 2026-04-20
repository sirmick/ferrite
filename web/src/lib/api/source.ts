// REST helpers for `GET/PATCH /api/source`. Mirrors
// `ferrite_runtime::SourceConfig` — a pair of `{ type, params }` that
// the server composes with the flowgraph preset before loading the
// pipeline. The `params` object is source-type-specific (SineSource,
// SoapySource, FileSource).

import type { ReconfigureResponse } from '$lib/api/flowgraph';
import { ApiError } from '$lib/api/errors';

export interface SourceConfig {
  /** Source block type (`SineSource`, `SoapySource`, `FileSource`, …). */
  type: string;
  /** Source-specific params; merged into the preset's `src` block. */
  params: Record<string, unknown>;
}

export async function fetchSource(): Promise<SourceConfig> {
  const r = await fetch('/api/source');
  if (!r.ok) throw new ApiError(r.status, `source fetch failed (${r.status})`);
  return (await r.json()) as SourceConfig;
}

export async function patchSource(cfg: SourceConfig): Promise<ReconfigureResponse> {
  const r = await fetch('/api/source', {
    method: 'PATCH',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(cfg),
  });
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `source patch failed (${r.status}): ${text}`);
  }
  return (await r.json()) as ReconfigureResponse;
}
