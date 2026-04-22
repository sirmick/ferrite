// GET /api/source/capabilities — probe the active source. Hardware
// sources return the full DeviceCapabilities blob; software sources
// (SineSource, FileIqSource) just echo their type_name so the UI can
// hide hardware-only controls.

import { ApiError } from '$lib/api/errors';
import type { DeviceCapabilities } from '$lib/api/devices';

export type { DeviceCapabilities };

export type SourceCapabilitiesResponse =
  | { kind: 'hardware'; type_name: string; capabilities: DeviceCapabilities }
  | { kind: 'software'; type_name: string }
  | { kind: 'unavailable'; type_name: string; error: string };

export async function fetchSourceCapabilities(): Promise<SourceCapabilitiesResponse> {
  const r = await fetch('/api/source/capabilities');
  if (!r.ok) {
    const text = await r.text();
    throw new ApiError(r.status, `source capabilities fetch failed (${r.status}): ${text}`);
  }
  return (await r.json()) as SourceCapabilitiesResponse;
}
