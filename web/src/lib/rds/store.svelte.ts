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

import { PayloadType, type ParsedFrame } from '$lib/ws/frame';
import type { FrameClient } from '$lib/ws/client';

class RdsStore {
  pi = $state<number | null>(null);
  ps = $state<string | null>(null);
  pty = $state<number | null>(null);
  tp = $state<boolean | null>(null);
  ta = $state<boolean | null>(null);

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
    this.pi = null;
    this.ps = null;
    this.pty = null;
    this.tp = null;
    this.ta = null;
  }

  private onFrame(frame: ParsedFrame): void {
    if (frame.header.payloadType !== PayloadType.JsonEvent) return;
    const text = new TextDecoder().decode(frame.payload);
    // One frame may carry multiple newline-delimited events.
    for (const line of text.split('\n')) {
      const trimmed = line.trim();
      if (!trimmed) continue;
      try {
        const obj = JSON.parse(trimmed) as {
          pi?: number;
          ps?: string;
          pty?: number;
          tp?: boolean;
          ta?: boolean;
        };
        if (typeof obj.pi === 'number') this.pi = obj.pi;
        if (typeof obj.ps === 'string') this.ps = obj.ps;
        if (typeof obj.pty === 'number') this.pty = obj.pty;
        if (typeof obj.tp === 'boolean') this.tp = obj.tp;
        if (typeof obj.ta === 'boolean') this.ta = obj.ta;
      } catch {
        // Malformed line — drop. Block-level sync hysteresis normally
        // filters these; nothing to do here.
      }
    }
  }
}

export const rds = new RdsStore();

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
