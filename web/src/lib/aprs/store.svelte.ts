// Singleton store for the APRS advanced view. Subscribes to the
// `ui:aprs` sink (wired by packet.json), parsing the per-frame JSON
// `PacketDemod` emits (see blocks/src/aprs.rs for the wire shape).
//
// APRS is a mix: position/object/item frames place a station on the
// map; status/message/mic-e/other frames are text. So the store keeps
// two views of the same stream:
//   - `stations` — latest frame per callsign (table + map markers)
//   - `lines`    — an append-only console log of every frame
//
// Same lifecycle as the FT8/ADS-B stores: long-lived per
// advanced-view type, cleared only by the Reset button. Data-only;
// selection (by callsign) lives in AprsView.

import { PayloadType, type ParsedFrame } from '$lib/ws/frame';
import type { FrameClient } from '$lib/ws/client';
import type { MapStation } from '$lib/viz/StationMap.svelte';
import type { ConsoleLine } from '$lib/viz/TextConsole.svelte';

export interface AprsStation {
  id: string; // callsign-SSID (table row id + map id + selection key)
  kind: string; // position | object | item | status | message | mic-e | other
  lat: number | null;
  lon: number | null;
  name: string | null; // object/item name or message addressee
  text: string; // comment / status / message body
  path: string;
  msgs: number; // frames heard from this call
  seenMs: number;
}

const MAX_LINES = 2000;

class AprsStore {
  private byCall = $state(new Map<string, AprsStation>());
  /** Append-only console log (all frames, newest last). */
  lines = $state<ConsoleLine[]>([]);
  private seq = 0;
  private unsubscribe?: () => void;
  private streamId?: number;

  attach(client: FrameClient, streamId: number): void {
    if (this.streamId === streamId && this.unsubscribe) return;
    this.unsubscribe?.();
    this.streamId = streamId;
    this.unsubscribe = client.subscribe(streamId, (f) => this.onFrame(f));
  }

  detach(): void {
    this.unsubscribe?.();
    this.unsubscribe = undefined;
  }

  reset(): void {
    this.byCall = new Map();
    this.lines = [];
  }

  /** Table rows — every station heard, latest frame. */
  stations = $derived([...this.byCall.values()]);

  /** Map markers — only frames that carried a position. Plain
   *  markers (kind defaults to 'station' → circle), same as FT8;
   *  APRS symbol glyphs are a later nicety. */
  mapStations = $derived.by<MapStation[]>(() =>
    [...this.byCall.values()]
      .filter((s) => s.lat !== null && s.lon !== null)
      .map((s) => ({
        id: s.id,
        call: s.name || s.id,
        lat: s.lat as number,
        lon: s.lon as number,
        kind: 'station' as const,
      })),
  );

  private onFrame(frame: ParsedFrame): void {
    if (frame.header.payloadType !== PayloadType.JsonEvent) return;
    const text = new TextDecoder().decode(frame.payload);
    const now = Date.now();
    let added: ConsoleLine[] | null = null;
    for (const line of text.split('\n')) {
      const t = line.trim();
      if (!t) continue;
      let o: Record<string, unknown>;
      try {
        o = JSON.parse(t);
      } catch {
        continue;
      }
      if (typeof o.call !== 'string') continue;
      const call = o.call;
      const kind = typeof o.kind === 'string' ? o.kind : 'other';
      const lat = typeof o.lat === 'number' ? o.lat : null;
      const lon = typeof o.lon === 'number' ? o.lon : null;
      const name = typeof o.name === 'string' ? o.name : null;
      const body = typeof o.text === 'string' ? o.text : '';
      const raw = typeof o.raw === 'string' ? o.raw : '';
      const prev = this.byCall.get(call);
      this.byCall.set(call, {
        id: call,
        kind,
        // Keep the last known position if this frame had none.
        lat: lat ?? prev?.lat ?? null,
        lon: lon ?? prev?.lon ?? null,
        name,
        text: body,
        path: typeof o.path === 'string' ? o.path : '',
        msgs: (prev?.msgs ?? 0) + 1,
        seenMs: now,
      });
      (added ??= []).push({
        id: `${this.seq++}`,
        ts: Math.floor(now / 1000),
        // Console shows the raw APRS info verbatim, prefixed by call.
        text: `${call}  ${raw || body}`,
        tone: kind === 'message' ? 'accent' : kind === 'other' ? 'muted' : 'normal',
      });
    }
    if (!added) return;
    // Reassign map so the $derived recomputes (mutation isn't tracked).
    this.byCall = new Map(this.byCall);
    const next = this.lines.concat(added);
    this.lines = next.length > MAX_LINES ? next.slice(next.length - MAX_LINES) : next;
  }
}

export const aprs = new AprsStore();
