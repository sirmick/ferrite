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

/** One entry from `SoapySDRDevice_getSettingInfo` — driver-specific
 *  knobs that don't fit the standard Soapy surface (e.g. SDRplay's
 *  `rfgain_sel`, `agc_setpoint`, `biasT_ctrl`; HackRF's `amp_ctrl`).
 *  The frontend renders these generically by `data_type` + range/options,
 *  so adding a new driver requires no UI code. */
export interface SettingInfo {
  key: string;
  label: string;
  description: string | null;
  units: string | null;
  data_type: SettingType;
  /** Soapy reports default + current as the same string; we keep the
   *  raw form rather than parsing per-type so writes round-trip. */
  default: string;
  range: RangeSpec | null;
  options: SettingOption[];
}

export type SettingType = 'bool' | 'int' | 'float' | 'string';

export interface SettingOption {
  value: string;
  label: string | null;
}

export interface DeviceCapabilities {
  info: DeviceInfo;
  driver_key: string;
  hardware_key: string;
  hardware_info: Record<string, string>;
  rx_channels: RxChannelCapabilities[];
  settings: SettingInfo[];
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

/** Fetch the device list. SoapySDR is a hard dep on the server side. */
export async function fetchDevices(): Promise<DeviceEntry[]> {
  const r = await fetch('/api/devices');
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
