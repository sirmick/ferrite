// fldigi decode store — backs the FldigiView advanced panel.
//
// fldigi-family decoders (RttyDemod / Psk31Demod / CwDemod / Mt63Demod
// / FldigiAuto / Olivia / Contestia / DominoEX / Throb / Navtex) emit
// one newline-delimited JSON record per decoded chunk on their
// `events` port — `{"t":"fldigi","mode":"<label>","text":"<decoded>"}`
// (blocks/src/fldigi_modes.rs). Wired to `ui:fldigi`, those records
// arrive here as `PayloadType.JsonEvent` frames (node: over the WS;
// browser-side decode reaches the same store once the runner's
// browser EventsSink dispatch lands — tracked separately).
//
// fldigi is a continuous text stream (no slots, no station spots), so
// this is just an append-only console — same shape as the APRS store's
// `lines`, rendered by the shared TextConsole primitive.

import type { FrameClient } from '$lib/ws/client';
import { PayloadType, type ParsedFrame } from '$lib/ws/frame';
import type { ConsoleLine } from '$lib/viz/TextConsole.svelte';

const MAX_LINES = 4000;

class FldigiStore {
  /** Append-only decoded-text console (newest last). */
  lines = $state<ConsoleLine[]>([]);
  /** Most-recent mode label seen (e.g. `rtty45`, `BPSK31`) — for the
   *  view header; FldigiAuto changes this live on an RSID hit. */
  mode = $state<string>('');
  private seq = 0;
  private unsubscribe?: () => void;
  private streamId?: number;

  attach(client: FrameClient, streamId: number): void {
    if (this.streamId === streamId && this.unsubscribe) return;
    this.unsubscribe?.();
    this.streamId = streamId;
    this.unsubscribe = client.subscribe(streamId, (f) => this.onFrame(f));
  }

  /** Stop receiving but keep the log (view toggled off / pipeline
   *  stopped must not lose an accumulated session). */
  detach(): void {
    this.unsubscribe?.();
    this.unsubscribe = undefined;
  }

  /** Operator Reset — the only thing that clears the log. */
  reset(): void {
    this.lines = [];
    this.mode = '';
  }

  private onFrame(frame: ParsedFrame): void {
    if (frame.header.payloadType !== PayloadType.JsonEvent) return;
    const text = new TextDecoder().decode(frame.payload);
    const now = Math.floor(Date.now() / 1000);
    let added: ConsoleLine[] | null = null;
    let lastMode: string | null = null;
    for (const line of text.split('\n')) {
      const t = line.trim();
      if (!t) continue;
      let o: Record<string, unknown>;
      try {
        o = JSON.parse(t);
      } catch {
        continue; // malformed — drop, same as the ft8/aprs stores
      }
      if (o.t !== 'fldigi') continue;
      const body = typeof o.text === 'string' ? o.text : '';
      if (!body) continue;
      if (typeof o.mode === 'string') lastMode = o.mode;
      (added ??= []).push({
        id: `${this.seq++}`,
        ts: now,
        text: body,
      });
    }
    if (!added) return;
    if (lastMode !== null) this.mode = lastMode;
    const next = this.lines.concat(added);
    this.lines = next.length > MAX_LINES ? next.slice(next.length - MAX_LINES) : next;
  }
}

export const fldigi = new FldigiStore();
