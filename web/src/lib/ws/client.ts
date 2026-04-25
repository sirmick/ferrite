// Multiplexed WebSocket frame client.
//
// One connection per session, decodes the 16-byte header on every binary
// frame, then dispatches the payload to subscribers keyed by `stream_id`.
// Decoupling the wire from per-stream consumers keeps the waterfall, VFO
// audio, and any future panes from each owning their own socket.

import { decodeFrame, type ParsedFrame } from './frame';

export type FrameHandler = (frame: ParsedFrame) => void;

export type ClientStatus = 'connecting' | 'open' | 'closing' | 'closed' | 'error';

export interface FrameClientOptions {
  url: string;
  onStatus?: (status: ClientStatus) => void;
  onDecodeError?: (err: Error) => void;
}

export class FrameClient {
  private readonly ws: WebSocket;
  private readonly subscribers = new Map<number, Set<FrameHandler>>();
  private readonly onDecodeError?: (err: Error) => void;

  /** Monotonically-increasing total of binary bytes pulled off the
   *  socket since this client was opened — header + payload, every
   *  stream summed together. Pollers compute KB/s by snapshotting on
   *  a timer and diffing. Reset only when a new client is constructed. */
  bytesReceived = 0;

  constructor(opts: FrameClientOptions) {
    this.onDecodeError = opts.onDecodeError;
    this.ws = new WebSocket(opts.url);
    this.ws.binaryType = 'arraybuffer';

    const status = opts.onStatus;
    status?.('connecting');
    this.ws.addEventListener('open', () => status?.('open'));
    this.ws.addEventListener('close', () => status?.('closed'));
    this.ws.addEventListener('error', () => status?.('error'));
    this.ws.addEventListener('message', (e) => this.onMessage(e));
  }

  private onMessage(event: MessageEvent): void {
    if (!(event.data instanceof ArrayBuffer)) {
      // Server pushes binary only; ignore stray text/control frames.
      return;
    }
    this.bytesReceived += event.data.byteLength;
    let frame: ParsedFrame;
    try {
      frame = decodeFrame(event.data);
    } catch (err) {
      this.onDecodeError?.(err as Error);
      return;
    }
    const subs = this.subscribers.get(frame.header.streamId);
    if (!subs) return;
    for (const handler of subs) handler(frame);
  }

  /** Subscribe to a stream. Returns an unsubscribe function. */
  subscribe(streamId: number, handler: FrameHandler): () => void {
    let set = this.subscribers.get(streamId);
    if (!set) {
      set = new Set();
      this.subscribers.set(streamId, set);
    }
    set.add(handler);
    return () => {
      set!.delete(handler);
      if (set!.size === 0) this.subscribers.delete(streamId);
    };
  }

  close(): void {
    if (this.ws.readyState === WebSocket.OPEN || this.ws.readyState === WebSocket.CONNECTING) {
      this.ws.close();
    }
  }

  get readyState(): number {
    return this.ws.readyState;
  }
}
