// GET /api/source/capabilities — probe the active source. Hardware
// sources return the full DeviceCapabilities blob; software sources
// (SineSource, FileIqSource) just echo their type_name so the UI can
// hide hardware-only controls.

import { ApiError } from '$lib/api/errors';

export interface RangeSpec {
  min: number;
  max: number;
  step: number | null;
}

export interface GainElement {
  name: string;
  range_db: RangeSpec;
}

export interface FrequencyComponent {
  name: string;
  ranges_hz: RangeSpec[];
}

export interface RxChannelCapabilities {
  index: number;
  antennas: string[];
  sample_rate_ranges_hz: RangeSpec[];
  bandwidth_ranges_hz: RangeSpec[];
  frequency_ranges_hz: RangeSpec[];
  frequency_components: FrequencyComponent[];
  gains: GainElement[];
  overall_gain_range_db: RangeSpec | null;
  has_agc: boolean;
}

export interface DeviceInfo {
  driver: string;
  label: string;
  serial: string | null;
  args: Record<string, string>;
}

export interface DeviceCapabilities {
  info: DeviceInfo;
  driver_key: string;
  hardware_key: string;
  hardware_info: Record<string, string>;
  rx_channels: RxChannelCapabilities[];
}

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
