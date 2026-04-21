// REST helpers for `/api/presets` (browse) and `/api/preset` (swap).
// Mirrors `PresetEntry` and `LoadPresetResponse` on the wire.

import { ApiError } from '$lib/api/errors';
import type { ReconfigureResponse } from '$lib/api/flowgraph';

export interface PresetEntry {
  name: string;
  label: string | null;
  description: string | null;
}

export interface LoadPresetResponse {
  name: string;
  reconfigure: ReconfigureResponse;
}

export async function fetchPresets(): Promise<PresetEntry[]> {
  const r = await fetch('/api/presets');
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `presets fetch failed (${r.status}): ${text}`);
  }
  return (await r.json()) as PresetEntry[];
}

export async function loadPreset(name: string): Promise<LoadPresetResponse> {
  const r = await fetch('/api/preset', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ name }),
  });
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `load preset ${name} failed (${r.status}): ${text}`);
  }
  return (await r.json()) as LoadPresetResponse;
}
