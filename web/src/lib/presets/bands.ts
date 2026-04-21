// Loader + types for the static presets file. `bands.json` is imported at
// build time by Vite; no fetch at runtime. Swap to a fetch of a user-
// writable JSON once we have a server-side config store.

import raw from './bands.json';

export interface BandEntry {
  label: string;
  hz: number;
  /** Advisory demod mode — used by the panel display until demod lands. */
  mode?: string;
  /** Preset basename to load before tuning. Names here must match a
   *  file the server exposes via `GET /api/presets`; the click handler
   *  swaps the preset first, then patches center_freq_hz. */
  preset?: string;
}

export interface BandGroup {
  name: string;
  entries: BandEntry[];
}

interface BandsFile {
  groups: BandGroup[];
}

export const bands: BandGroup[] = (raw as BandsFile).groups;
