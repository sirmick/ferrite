// Singleton store for RSSI events streamed from the server-side
// `RssiProbe` block. Subscribes to the `ui:rssi` sink's stream_id when
// the pipeline advertises one, parses each `{"rssi_dbfs": …}` line, and
// exposes the current value as a Svelte $state reactive.
//
// Only one subscriber per session — the component renders from this
// shared state so the meter and any future overlays stay in sync.

import { PayloadType, type ParsedFrame } from '$lib/ws/frame';
import type { FrameClient } from '$lib/ws/client';

class RssiStore {
  /** Latest smoothed RSSI in dBFS, or `null` when no events have been
   * seen since the last reset / preset change. `-Infinity` is not
   * surfaced — the probe clamps to −300 dBFS which renders as "—". */
  dbfs = $state<number | null>(null);

  private unsubscribe?: () => void;
  private streamId?: number;

  /** Attach to a running FrameClient + stream_id. Safe to call when
   * already attached — a re-attach replaces the old subscription. */
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
    this.dbfs = null;
  }

  private onFrame(frame: ParsedFrame): void {
    if (frame.header.payloadType !== PayloadType.JsonEvent) return;
    try {
      const text = new TextDecoder().decode(frame.payload);
      const obj = JSON.parse(text) as { rssi_dbfs?: number };
      if (typeof obj.rssi_dbfs === 'number' && Number.isFinite(obj.rssi_dbfs)) {
        this.dbfs = obj.rssi_dbfs;
      }
    } catch {
      // Malformed event line — drop silently. A log here would fire
      // tens of times per second on a broken producer; the server-side
      // `WsBridgeTxEvents` already strips partials at its own layer.
    }
  }
}

export const rssi = new RssiStore();
