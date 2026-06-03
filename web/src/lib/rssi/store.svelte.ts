// RSSI readout — a thin adapter over the decoder mirror (kind `rssi`).
//
// The server-side `RssiProbe` block emits `{"rssi_dbfs": …}` on its
// `events` port, wired to `ui:rssi` → an `EventStore` (kind `rssi`,
// Replace policy: the probe re-emits continuously, the store keeps the
// single current record), mirrored to the browser over `/ws/state`.
// This keeps the original `rssi.dbfs` surface so RssiMeter is unchanged.

import { decoders } from '$lib/decoders/store.svelte';

export const rssi = {
  /** Latest smoothed RSSI in dBFS, or `null` when no events have been
   *  seen / the preset has no `RssiProbe`. `-Infinity` is not surfaced —
   *  the probe clamps to −300 dBFS which renders as "—". */
  get dbfs(): number | null {
    const d = decoders.kind('rssi').current['current']?.data as { rssi_dbfs?: number } | undefined;
    const v = d?.rssi_dbfs;
    return typeof v === 'number' && Number.isFinite(v) ? v : null;
  },
};
