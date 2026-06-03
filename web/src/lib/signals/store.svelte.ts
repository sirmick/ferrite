// Strongest-signal watchlist — now a thin adapter over the unified
// decoder mirror (`$lib/decoders/store`). The node-side `SignalList`
// block's `ui:signals` events terminus feeds the server `DecoderStore`
// (kind `signals`, Replace-fold: one record holding the whole snapshot),
// which is mirrored to the browser over `/ws/state`. This file keeps
// the original `signals.signals` / `centerFreqHz` / `spanHz` surface so
// the spectrum overlays + the Signals panel read it unchanged.
//
// Snapshot shape (the record's `data`, emitted by SignalList):
//   {"signals":[{"id":3,"freq_hz":…,"power_db":…,"bw_hz":…,"snr_db":…,
//                "first_seen_frame":…,"last_seen_frame":…}],
//    "center_freq_hz":…,"span_hz":…,"noise_floor_dbfs":…,"frame":…}

import { decoders } from '$lib/decoders/store.svelte';

export interface Signal {
  /** Stable track id assigned by the block; survives across emissions. */
  id: number;
  /** Absolute centre frequency of the detected signal (Hz). */
  freq_hz: number;
  /** Peak power of the signal (dBFS). */
  power_db: number;
  /** Estimated occupied bandwidth (Hz). */
  bw_hz: number;
  /** Peak above the estimated noise floor (dB). */
  snr_db: number;
  first_seen_frame: number;
  last_seen_frame: number;
}

interface SignalsSnapshot {
  signals?: Signal[];
  center_freq_hz?: number;
  span_hz?: number;
}

function snapshot(): SignalsSnapshot {
  const rec = decoders.kind('signals').current.current;
  return (rec?.data as SignalsSnapshot | undefined) ?? {};
}

/** Reactive view over the mirror's `signals` kind. The getters read
 *  `decoders.kinds` ($state), so Svelte components stay reactive. */
export const signals = {
  /** Current watchlist, strongest first. */
  get signals(): Signal[] {
    const s = snapshot().signals;
    // Defensive re-sort: the block ranks by power, but never trust the
    // wire — consumers assume strongest-first.
    return Array.isArray(s) ? [...s].sort((a, b) => b.power_db - a.power_db) : [];
  },
  /** Tuned centre the list was detected against (Hz). */
  get centerFreqHz(): number | null {
    const c = snapshot().center_freq_hz;
    return typeof c === 'number' ? c : null;
  },
  /** Detection span (source sample rate, Hz). */
  get spanHz(): number | null {
    const s = snapshot().span_hz;
    return typeof s === 'number' ? s : null;
  },

  // Back-compat no-ops: the mirror is always connected, so the old
  // per-consumer WS attach/release (and its refcount) is gone. Callers
  // can drop these at their leisure.
  attach(_client?: unknown, _streamId?: unknown): void {},
  release(): void {},
  detach(): void {},
};
