// Loader + types for the static presets file. `bands.json` is imported at
// build time by Vite; no fetch at runtime. Swap to a fetch of a user-
// writable JSON once we have a server-side config store.

import raw from './bands.json';

export interface BandEntry {
  label: string;
  /** Desired listen frequency in Hz — what the user thinks of as the
   *  station's frequency. */
  hz: number;
  /** Optional offset to push the SDR centre off the signal of interest,
   *  used to dodge the SDR's DC spike on weak NBFM signals (APRS being
   *  the canonical case). When set, the panel patches
   *  center_freq_hz = hz + vfo_offset_hz and freq_shift_hz = -vfo_offset_hz,
   *  so the channelizer pulls the signal back to baseband. Default 0. */
  vfo_offset_hz?: number;
  /** Decorative chip text. Selecting a band does NOT swap the demod
   *  chain — pick that from the Signal Catalog. */
  mode?: string;
}

export interface BandGroup {
  name: string;
  entries: BandEntry[];
}

interface BandsFile {
  groups: BandGroup[];
}

export const bands: BandGroup[] = (raw as BandsFile).groups;
