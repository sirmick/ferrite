// Singleton store for the strongest-signal watchlist streamed from the
// node-side `SignalList` block over its `ui:signals` sink. Subscribes to
// the sink's stream_id when the pipeline advertises one and exposes the
// current ranked list as Svelte `$state`.
//
// Unlike the decoder stores (RDS / ADS-B / APRS), each wire frame is a
// *full snapshot* of the whole watchlist, not an incremental patch — the
// block re-emits the entire ranked list every ~250 ms. So the store
// simply replaces its array with the newest complete object in the frame.
// Track `id`s are stable across emissions (the block keeps a track's id
// while it persists), which is what lets the view key its rows on `id`
// for in-place updates rather than a full re-render.
//
// Wire payload format — newline-delimited JSON, one snapshot per line:
//   {"signals":[{"id":3,"freq_hz":100100000,"power_db":-12.3,"bw_hz":180000,
//                "snr_db":42.1,"first_seen_frame":10,"last_seen_frame":120}],
//    "center_freq_hz":100000000,"span_hz":2400000,"frame":120}

import { PayloadType, type ParsedFrame } from '$lib/ws/frame';
import type { FrameClient } from '$lib/ws/client';

export interface Signal {
  /** Stable track id assigned by the block; survives across emissions
   *  while the signal persists. Used as the keyed-`{#each}` identity. */
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

interface Snapshot {
  signals?: Signal[];
  center_freq_hz?: number;
  span_hz?: number;
}

class SignalsStore {
  /** Current watchlist, strongest first. */
  signals = $state<Signal[]>([]);
  /** Tuned centre the list was detected against (Hz) — context for the
   *  view (e.g. an "offset from centre" column). */
  centerFreqHz = $state<number | null>(null);
  /** Detection span (source sample rate, Hz). */
  spanHz = $state<number | null>(null);

  private unsubscribe?: () => void;
  private streamId?: number;

  attach(client: FrameClient, streamId: number): void {
    if (this.streamId === streamId && this.unsubscribe) return;
    this.detach();
    this.streamId = streamId;
    this.unsubscribe = client.subscribe(streamId, (frame) => this.onFrame(frame));
  }

  detach(): void {
    this.unsubscribe?.();
    this.unsubscribe = undefined;
    this.streamId = undefined;
    this.signals = [];
    this.centerFreqHz = null;
    this.spanHz = null;
  }

  private onFrame(frame: ParsedFrame): void {
    if (frame.header.payloadType !== PayloadType.JsonEvent) return;
    const text = new TextDecoder().decode(frame.payload);
    // Each line is a complete snapshot; the newest one wins. Walk from
    // the end so we apply the last valid object and skip the rest.
    const lines = text.split('\n');
    for (let i = lines.length - 1; i >= 0; i--) {
      const trimmed = lines[i].trim();
      if (!trimmed) continue;
      try {
        const obj = JSON.parse(trimmed) as Snapshot;
        if (!Array.isArray(obj.signals)) continue;
        // Defensive re-sort: the block already ranks by power, but never
        // trust the wire — the view assumes strongest-first.
        this.signals = [...obj.signals].sort((a, b) => b.power_db - a.power_db);
        this.centerFreqHz = typeof obj.center_freq_hz === 'number' ? obj.center_freq_hz : null;
        this.spanHz = typeof obj.span_hz === 'number' ? obj.span_hz : null;
        return;
      } catch {
        // Malformed line — drop and try the previous one.
      }
    }
  }
}

export const signals = new SignalsStore();
