// `WsFrameSource` — boundary block: a network-fed frame stream surfaced to
// downstream code (waterfall, decoders, …) as a typed `payload` callback.
//
// Today this is a thin wrapper over `FrameClient.subscribe`. When the
// browser-side flowgraph runtime lands (Phase D) it will adopt this class
// as its concrete `WsFrameSource` block, and the same edge in a flowgraph
// JSON will resolve to one of these on browser side and a `WsFrameSink`
// on the server side.

import type { FrameClient, FrameHandler } from './client';
import type { ParsedFrame, PayloadTypeValue } from './frame';

export type FrameTypedHandler<P> = (payload: P, frame: ParsedFrame) => void;

export interface WsFrameSourceOptions<P> {
  client: FrameClient;
  streamId: number;
  /** Optional payload-type guard — frames with the wrong type are dropped. */
  expect?: PayloadTypeValue;
  /** Convert raw bytes into the consumer's preferred view. */
  view: (payload: Uint8Array) => P;
}

export class WsFrameSource<P> {
  private unsubscribe?: () => void;
  private handler?: FrameTypedHandler<P>;

  constructor(private readonly opts: WsFrameSourceOptions<P>) {}

  start(handler: FrameTypedHandler<P>): void {
    this.stop();
    this.handler = handler;
    const inner: FrameHandler = (frame) => {
      if (this.opts.expect !== undefined && frame.header.payloadType !== this.opts.expect) {
        return;
      }
      handler(this.opts.view(frame.payload), frame);
    };
    this.unsubscribe = this.opts.client.subscribe(this.opts.streamId, inner);
  }

  stop(): void {
    this.unsubscribe?.();
    this.unsubscribe = undefined;
    this.handler = undefined;
  }

  get isRunning(): boolean {
    return this.handler !== undefined;
  }
}

/** Convenience: zero-copy `Uint8Array` view of an `fft_u8` payload. */
export const viewU8 = (p: Uint8Array): Uint8Array => p;

/** Convenience: zero-copy interleaved I/Q `Float32Array` view (LE host). */
export function viewIqF32(p: Uint8Array): Float32Array {
  // Float32Array requires 4-byte-aligned offset; copy when we can't borrow.
  if ((p.byteOffset & 3) === 0) {
    return new Float32Array(p.buffer, p.byteOffset, p.byteLength >>> 2);
  }
  const copy = new Uint8Array(p);
  return new Float32Array(copy.buffer);
}
