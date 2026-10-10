// Browser → store: anything a browser-placed block produces (decoder
// events from env_split's `ui:<kind>` sinks, in-browser transcription)
// is published to `ferrite/ingest/<kind>`; the server folds it into the
// store and republishes on `ferrite/state/<kind>`, so this tab, every
// other tab, `ferrite-ctl await`, and the AI all see it the same way a
// node-side decode is seen. Each event is stamped with the graph epoch
// the browser is running so a stale half's output is dropped
// server-side rather than mis-attributed.

import { eventBus, type EventBus } from './bus';

export const INGEST_PREFIX = 'ferrite/ingest/';

export function ingestTopic(kind: string): string {
  return `${INGEST_PREFIX}${kind.replace(/^\/+|\/+$/g, '')}`;
}

/** The store kind a browser-side `ui:<kind>` events sink feeds, looked
 *  up by the stream id the runner reported it on. `null` if unknown. */
export function kindForStream(
  uiSinks: ReadonlyArray<{ name: string; stream_id: number }>,
  streamId: number,
): string | null {
  return uiSinks.find((s) => s.stream_id === streamId)?.name ?? null;
}

/** Publish newline-delimited JSON decoder events for `kind`. Non-JSON
 *  or non-object lines are skipped (the store wants a record, not
 *  text). Returns how many were published; 0 when the bus is down. */
export function publishDecoderLines(
  kind: string,
  lines: ReadonlyArray<string>,
  epoch: number,
  bus: EventBus = eventBus,
): number {
  if (!bus.connected) return 0;
  const topic = ingestTopic(kind);
  let n = 0;
  for (const line of lines) {
    const text = line.trim();
    if (!text) continue;
    let v: unknown;
    try {
      v = JSON.parse(text);
    } catch {
      continue;
    }
    if (!v || typeof v !== 'object' || Array.isArray(v)) continue;
    const obj = v as Record<string, unknown>;
    if (typeof obj.epoch !== 'number' && epoch > 0) obj.epoch = epoch;
    if (bus.publish(topic, JSON.stringify(obj))) n += 1;
  }
  return n;
}

/** The server's `transcribe` record shape (camelCase, what
 *  `TranscriptView` ingests from the mirror). */
export interface TranscriptRecord {
  atMs: number;
  vfoHz: number | null;
  t0: number;
  t1: number;
  text: string;
  rawText?: string;
  confidence: number;
  noSpeechProb: number;
  cont: boolean;
  gapMs: number;
  speakerTurn: boolean;
}

/** Publish one in-browser transcription segment as a `transcribe`
 *  record. Returns false when the bus is down (caller keeps it local). */
export function publishTranscriptSegment(
  seg: TranscriptRecord,
  epoch: number,
  bus: EventBus = eventBus,
): boolean {
  if (!bus.connected) return false;
  const payload: Record<string, unknown> = { ...seg, source: 'browser' };
  if (epoch > 0) payload.epoch = epoch;
  return bus.publish(ingestTopic('transcribe'), JSON.stringify(payload));
}
