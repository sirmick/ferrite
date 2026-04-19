// REST helper + types for `GET /api/devices`.
//
// Mirrors the `DeviceEntry` / `DeviceCapabilities` shapes from
// `server/src/{routes,device}.rs`. Snake-case wire names are kept so this
// file is the single conversion boundary.

export interface RangeSpec {
  min: number;
  max: number;
  /** `null` on the wire when the range is continuous (no step quantum). */
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

/** Tagged-union mirror of `routes.rs::DeviceEntry`. */
export type DeviceEntry =
  | { status: 'available'; capabilities: DeviceCapabilities }
  | { status: 'unavailable'; info: DeviceInfo; error: string };

/** Build `key=value,key=value` (Soapy device-args form) from a DeviceInfo. */
export function deviceArgsString(info: DeviceInfo): string {
  return Object.entries(info.args)
    .map(([k, v]) => `${k}=${v}`)
    .join(',');
}

/** Display label for a DeviceEntry, regardless of probe success. */
export function deviceLabel(entry: DeviceEntry): string {
  return entry.status === 'available' ? entry.capabilities.info.label : entry.info.label;
}

/**
 * Fetch the device list. Returns `null` when the server was built without
 * the `soapysdr` feature (the route returns 501 in that case) so callers
 * can render a "no hardware support" hint instead of an error toast.
 */
export async function fetchDevices(): Promise<DeviceEntry[] | null> {
  const r = await fetch('/api/devices');
  if (r.status === 501) return null;
  if (!r.ok) {
    throw new Error(`fetchDevices failed: ${r.status} ${r.statusText}`);
  }
  return (await r.json()) as DeviceEntry[];
}

/**
 * Decode the rust-side `serde(tag = "status")` representation into the
 * shape this module exports. Soapy entries marshal `Available(Box<Caps>)`
 * as `{ status: "available", ...DeviceCapabilities }` (flattened) and
 * `Unavailable { info, error }` as `{ status, info, error }`. Normalise
 * the available branch into `{ status, capabilities }` so the rest of
 * the UI doesn't need to know about the flatten quirk.
 */
export function normaliseEntries(raw: Array<Record<string, unknown>>): DeviceEntry[] {
  return raw.map((e) => {
    if (e.status === 'available') {
      const caps: Record<string, unknown> = { ...e };
      delete caps.status;
      return { status: 'available', capabilities: caps as unknown as DeviceCapabilities };
    }
    return e as unknown as DeviceEntry;
  });
}
