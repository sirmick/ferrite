// AI session bus — publish this sidecar's conversations onto ferrited's
// event plane so every AI panel (any tab, any time later: the store
// persists) and any agent (`await_events(kind="ai")`) sees them, side by
// side with headless drivers (Claude Code hooks → `ferrite-ctl
// chat-post`).
//
// Records go to `ferrite/ingest/ai` (the store's `ai` kind — an append
// log); the reply forming in real time goes to `ferrite/ingest/ai_live`
// (keyed by session, ≤4 Hz). The vocabulary, shared with chat-post:
//
//   {type:"user",        session, source, mode, model, t, text}
//   {type:"text",        …, text}                       a finished text block
//   {type:"tool_use",    …, tool_id, name, input}
//   {type:"tool_result", …, tool_id, text, is_error, images?}
//   {type:"turn_end",    …, status: done|error|stopped, error?}
//   {type:"reset",       …, reason}
//
// The /ws/chat socket stays the control channel (prompts, stop, reset,
// hello). Publishing is best-effort: with the broker down the sidecar
// still serves its own socket; records are dropped, not queued forever.

import mqtt, { type MqttClient } from 'mqtt';

const SOURCE = 'ferrite-ai';
/** Tool input/result text cap per record. */
const TEXT_CAP = 4000;
/** Inline tool-result images: at most this many, each at most this big. */
const IMAGE_MAX = 2;
const IMAGE_BYTES = 400_000;
/** Throttle for `ai_live` partial-text updates. */
const LIVE_MS = 250;

let client: MqttClient | null = null;
let connecting = false;

/** Connect once, lazily: find the broker port via ferrited's hello. */
export async function connectBus(ferritedUrl: string): Promise<void> {
  if (client || connecting) return;
  connecting = true;
  try {
    for (;;) {
      try {
        const hello = (await (await fetch(`${ferritedUrl}/api/hello`)).json()) as {
          mqtt?: { tcp?: string };
        };
        const port = Number(hello.mqtt?.tcp?.split(':').pop());
        if (!Number.isFinite(port) || port <= 0) throw new Error('daemon has no event plane');
        const host = new URL(ferritedUrl).hostname;
        client = mqtt.connect(`mqtt://${host}:${port}`, {
          clientId: `ferrite-ai-${process.pid}`,
          protocolVersion: 4,
          reconnectPeriod: 2000,
        });
        client.on('connect', () => console.log(`[ferrite-ai] session bus connected (mqtt ${host}:${port})`));
        client.on('error', (e) => console.warn(`[ferrite-ai] session bus: ${e.message}`));
        return;
      } catch (e) {
        console.warn(`[ferrite-ai] session bus: ferrited not reachable yet (${String(e)}); retrying in 5 s`);
        await new Promise((r) => setTimeout(r, 5000));
      }
    }
  } finally {
    connecting = false;
  }
}

function publish(kind: 'ai' | 'ai_live', record: Record<string, unknown>): void {
  if (!client?.connected) return;
  client.publish(`ferrite/ingest/${kind}`, JSON.stringify(record), { qos: 0 });
}

function clip(s: string): string {
  return s.length > TEXT_CAP ? `${s.slice(0, TEXT_CAP)}… [${s.length - TEXT_CAP} more chars]` : s;
}

/** Text + inline images out of a tool_result `content`. */
function splitResult(content: unknown): { text: string; images: string[] } {
  if (typeof content === 'string') return { text: content, images: [] };
  if (!Array.isArray(content)) return { text: JSON.stringify(content ?? ''), images: [] };
  const texts: string[] = [];
  const images: string[] = [];
  for (const part of content) {
    const p = part as Record<string, unknown> | null;
    if (p && p['type'] === 'image') {
      const src = p['source'] as Record<string, unknown> | undefined;
      const data = typeof src?.['data'] === 'string' ? (src['data'] as string) : '';
      const media = typeof src?.['media_type'] === 'string' ? (src['media_type'] as string) : 'image/png';
      if (data && images.length < IMAGE_MAX && data.length <= IMAGE_BYTES) {
        images.push(`data:${media};base64,${data}`);
      }
      continue;
    }
    if (p && typeof p['text'] === 'string') texts.push(p['text'] as string);
    else if (typeof part === 'string') texts.push(part);
  }
  return { text: texts.join(''), images };
}

/** One turn's publisher. Records wait until the SDK names the session
 *  (`system/init`), so every record carries it; a turn that dies before
 *  init is published under a `pending-…` id at finish. */
export class TurnPublisher {
  private session: string | null;
  private queue: Record<string, unknown>[] = [];
  /** Open content blocks by stream index. */
  private blocks = new Map<number, { kind: 'text' | 'tool'; id?: string; name?: string; buf: string }>();
  private liveAt = 0;

  constructor(
    private readonly mode: string,
    private readonly model: string,
    session: string | null,
  ) {
    this.session = session;
  }

  private base(): Record<string, unknown> {
    return { session: this.session, source: SOURCE, mode: this.mode, model: this.model, t: Date.now() };
  }

  private emit(rec: Record<string, unknown>): void {
    const full = { ...this.base(), ...rec };
    if (this.session) publish('ai', full);
    else this.queue.push(full);
  }

  private flushQueue(): void {
    for (const rec of this.queue) publish('ai', { ...rec, session: this.session });
    this.queue = [];
  }

  user(text: string): void {
    this.emit({ type: 'user', text });
  }

  /** Feed every SDK event of the turn. */
  ingest(e: Record<string, unknown>): void {
    if (e.type === 'system' && e['subtype'] === 'init' && typeof e['session_id'] === 'string') {
      this.session = e['session_id'] as string;
      this.flushQueue();
      return;
    }
    if (e.type === 'stream_event') {
      const ev = (e['event'] ?? {}) as Record<string, unknown>;
      const index = typeof ev['index'] === 'number' ? (ev['index'] as number) : 0;
      if (ev['type'] === 'content_block_start') {
        const b = (ev['content_block'] ?? {}) as Record<string, unknown>;
        if (b['type'] === 'tool_use') {
          this.blocks.set(index, { kind: 'tool', id: String(b['id'] ?? ''), name: String(b['name'] ?? '?'), buf: '' });
        } else if (b['type'] === 'text') {
          this.blocks.set(index, { kind: 'text', buf: '' });
        }
        return;
      }
      if (ev['type'] === 'content_block_delta') {
        const d = (ev['delta'] ?? {}) as Record<string, unknown>;
        const blk = this.blocks.get(index);
        if (!blk) return;
        if (d['type'] === 'text_delta') {
          blk.buf += String(d['text'] ?? '');
          this.live(blk.buf);
        } else if (d['type'] === 'input_json_delta') {
          blk.buf += String(d['partial_json'] ?? '');
        }
        return;
      }
      if (ev['type'] === 'content_block_stop') {
        const blk = this.blocks.get(index);
        this.blocks.delete(index);
        if (!blk) return;
        if (blk.kind === 'text') {
          if (blk.buf.trim()) this.emit({ type: 'text', text: blk.buf });
          this.live('');
        } else {
          this.emit({ type: 'tool_use', tool_id: blk.id, name: blk.name, input: clip(blk.buf) });
        }
      }
      return;
    }
    // Tool results arrive as `user` messages carrying tool_result blocks.
    if (e.type === 'user') {
      const msg = (e['message'] ?? {}) as Record<string, unknown>;
      const content = msg['content'];
      if (!Array.isArray(content)) return;
      for (const block of content) {
        const b = block as Record<string, unknown>;
        if (b['type'] !== 'tool_result') continue;
        const { text, images } = splitResult(b['content']);
        this.emit({
          type: 'tool_result',
          tool_id: String(b['tool_use_id'] ?? ''),
          text: clip(text),
          is_error: Boolean(b['is_error']),
          ...(images.length ? { images } : {}),
        });
      }
    }
  }

  /** The reply forming right now (throttled; '' clears it). */
  private live(text: string): void {
    if (!this.session) return;
    const now = Date.now();
    if (text && now - this.liveAt < LIVE_MS) return;
    this.liveAt = now;
    publish('ai_live', { session: this.session, source: SOURCE, text, t: now });
  }

  finish(status: 'done' | 'error' | 'stopped', error?: string): void {
    if (!this.session) this.session = `pending-${Date.now()}`;
    this.flushQueue();
    this.live('');
    this.emit({ type: 'turn_end', status, ...(error ? { error } : {}) });
  }
}

/** A context reset (mode switch, /reset, setup edit) — a banner on every panel. */
export function publishReset(session: string | null, mode: string, reason: string): void {
  publish('ai', { type: 'reset', session, source: SOURCE, mode, reason, t: Date.now() });
}
