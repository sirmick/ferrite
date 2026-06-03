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

/** Body of `POST /api/tune` — see the server route for the dodge math.
 *  `freq_hz` is the target listen frequency; `span_hz` is an optional
 *  passband floor (band presets compute it from the band's range);
 *  `offset_ratio` is the per-driver DC-spike dodge fraction. */
export interface TuneRequest {
  freq_hz: number;
  span_hz?: number;
  offset_ratio?: number;
}

/** `POST /api/tune` — single tuning intent (UI tuner-Enter, ▲▼,
 *  spectrum click, band-preset rx/tune, ferrite-ctl tune, AI tune all
 *  end up here). Server handles the DC-spike dodge + the keep-or-snap
 *  decision against the live channelizer. Same response shape as
 *  `PATCH /api/source`. */
export async function tune(req: TuneRequest): Promise<ReconfigureResponse> {
  const r = await fetch('/api/tune', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(req),
  });
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `tune failed (${r.status}): ${text}`);
  }
  return (await r.json()) as ReconfigureResponse;
}
