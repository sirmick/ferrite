// Singleton store for RDS events streamed from the server/wasm-side
// `RdsDemod` block. Subscribes to the `ui:rds` sink's stream_id when
// the pipeline advertises one, merges the JSON patches sent by the
// block into a single authoritative snapshot, and exposes the current
// station state as Svelte `$state` reactives.
//
// Wire payload format — newline-delimited JSON, one object per
// state-change event:
//   {"t":"rds","pi":4660,"pty":22,"tp":true,"ta":false,"ps":"KEXP 90 "}
// Only changed fields are present on any given line; the store keeps
// the last-seen value for each field across lines.

// Now a thin adapter over the decoder mirror. `RdsDemod` emits partial
// JSON patches; the server store's `Merge` fold accumulates them into one
// authoritative `rds` record, so here we just read the merged current.
import { decoders } from '$lib/decoders/store.svelte';

interface RdsState {
  pi?: number;
  ps?: string;
  pty?: number;
  tp?: boolean;
  ta?: boolean;
}

function rdsState(): RdsState {
  return (decoders.kind('rds').current.current?.data as RdsState | undefined) ?? {};
}

export const rds = {
  get pi(): number | null {
    const v = rdsState().pi;
    return typeof v === 'number' ? v : null;
  },
  get ps(): string | null {
    const v = rdsState().ps;
    return typeof v === 'string' ? v : null;
  },
  get pty(): number | null {
    const v = rdsState().pty;
    return typeof v === 'number' ? v : null;
  },
  get tp(): boolean | null {
    const v = rdsState().tp;
    return typeof v === 'boolean' ? v : null;
  },
  get ta(): boolean | null {
    const v = rdsState().ta;
    return typeof v === 'boolean' ? v : null;
  },
  // Mirror is always connected — attach/detach are no-ops.
  attach(_client?: unknown, _streamId?: unknown): void {},
  detach(): void {},
};

/** RBDS 1998 Annex D programme-type table — used to render the PTY
 *  numeric code as a human label. US (RBDS) naming; European (RDS)
 *  names for the same codes differ — 22 is "Jazz" in RBDS and "Jazz
 *  Music" in RDS, etc. Good enough for a one-word hint in the UI. */
export const PTY_NAMES: Record<number, string> = {
  0: 'None',
  1: 'News',
  2: 'Information',
  3: 'Sports',
  4: 'Talk',
  5: 'Rock',
  6: 'Classic Rock',
  7: 'Adult Hits',
  8: 'Soft Rock',
  9: 'Top 40',
  10: 'Country',
  11: 'Oldies',
  12: 'Soft',
  13: 'Nostalgia',
  14: 'Jazz',
  15: 'Classical',
  16: 'R&B',
  17: 'Soft R&B',
  18: 'Language',
  19: 'Religious Music',
  20: 'Religious Talk',
  21: 'Personality',
  22: 'Public',
  23: 'College',
  24: 'Spanish Talk',
  25: 'Spanish Music',
  26: 'Hip Hop',
  29: 'Weather',
  30: 'Emergency Test',
  31: 'Emergency',
};
