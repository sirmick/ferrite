// REST helpers for `/api/pipeline`. The server holds at most one live
// pipeline instance composed from the current flowgraph + source; these
// helpers drive its lifecycle.

import { ApiError } from '$lib/api/errors';

export type PipelineStatus = 'running' | 'stopped';

export interface PipelineStatusResponse {
  status: PipelineStatus;
}

export async function fetchPipelineStatus(): Promise<PipelineStatus> {
  const r = await fetch('/api/pipeline');
  if (!r.ok) throw new ApiError(r.status, `pipeline status failed (${r.status})`);
  const body = (await r.json()) as PipelineStatusResponse;
  return body.status;
}

export async function startPipeline(): Promise<PipelineStatus> {
  const r = await fetch('/api/pipeline/start', { method: 'POST' });
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `pipeline start failed (${r.status}): ${text}`);
  }
  const body = (await r.json()) as PipelineStatusResponse;
  return body.status;
}

export async function stopPipeline(): Promise<PipelineStatus> {
  const r = await fetch('/api/pipeline/stop', { method: 'POST' });
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `pipeline stop failed (${r.status}): ${text}`);
  }
  const body = (await r.json()) as PipelineStatusResponse;
  return body.status;
}
