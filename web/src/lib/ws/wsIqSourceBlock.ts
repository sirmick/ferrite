// WsIqSource — source block that drains an IQ_F32 stream off the
// multiplexed WebSocket into the flowgraph as `iq_f32` samples.
//
// The block subscribes to a stream_id on a shared `FrameClient` (one
// connection per session, see `ws/client.ts`) and accumulates incoming
// frames into an internal ring. Each `process(io)` tick copies as many
// samples as will fit into the scheduler's output buffer. Ring overflow
// — flowgraph running slower than samples arrive — drops the surplus
// so the network pacing isn't held hostage to the scheduler's.
//
// Units throughout this file are **float elements**: an iq_f32 payload
// carries interleaved I/Q pairs, so 2 floats == 1 complex sample. The
// Work struct counts floats because BlockIo handles array-view
// elements, matching the convention used by other real/iq blocks.
//
// This block is web-only (no WebSocket in the Node sidecar's flowgraphs
// today; when a Node variant lands it'll use a different transport
// block and share `WsIqSource` only in name).

import type { BlockInstance, BlockIo, Work } from '@ferrite/flowgraph-runtime/types';

import type { FrameClient } from './client.js';
import { PayloadType } from './frame.js';
import { viewIqF32 } from './source.js';

export interface WsIqSourceParams {
  /** Shared multiplexed WebSocket client. Injected by the runner. */
  client: FrameClient;
  /** VFO stream_id to subscribe to (>= VFO_STREAM_BASE on the server). */
  streamId: number;
  /**
   * Ring capacity in float elements (I/Q pairs × 2). Power of two.
   * Defaults to 131 072 — ~650 ms at 100 kS/s narrowband, lots of
   * headroom to absorb scheduler hiccups without adding meaningful
   * audio-path latency.
   */
  bufferFloats?: number;
}

const DEFAULT_BUFFER_FLOATS = 131072;

function isPow2(n: number): boolean {
  return Number.isInteger(n) && n > 0 && (n & (n - 1)) === 0;
}

export class WsIqSource implements BlockInstance {
  private readonly client: FrameClient;
  private readonly streamId: number;
  private readonly ring: Float32Array;
  private readonly mask: number;
  private writeIdx = 0;
  private readIdx = 0;
  private _droppedFloats = 0;
  private unsubscribe?: () => void;

  constructor(params: WsIqSourceParams) {
    this.client = params.client;
    this.streamId = params.streamId;
    const cap = params.bufferFloats ?? DEFAULT_BUFFER_FLOATS;
    if (!isPow2(cap)) {
      throw new RangeError(`WsIqSource: bufferFloats must be a power of two, got ${cap}`);
    }
    this.ring = new Float32Array(cap);
    this.mask = cap - 1;
  }

  /** Cumulative count of IQ floats dropped due to ring overflow. */
  get droppedFloats(): number {
    return this._droppedFloats;
  }

  /** Floats currently buffered (written but not yet produced downstream). */
  get bufferedFloats(): number {
    return (this.writeIdx - this.readIdx) >>> 0;
  }

  init(): void {
    this.writeIdx = 0;
    this.readIdx = 0;
    this._droppedFloats = 0;
    this.unsubscribe = this.client.subscribe(this.streamId, (frame) => {
      if (frame.header.payloadType !== PayloadType.IqF32) return;
      const samples = viewIqF32(frame.payload);
      this.push(samples);
    });
  }

  process(io: BlockIo): Work {
    const out = io.output('out');
    if (!out) return { consumed: [], produced: [0] };
    const dst = out.data as Float32Array;
    const avail = (this.writeIdx - this.readIdx) >>> 0;
    const toRead = Math.min(avail, dst.length);
    if (toRead === 0) return { consumed: [], produced: [0] };
    const readPos = this.readIdx & this.mask;
    const firstSpan = Math.min(toRead, this.ring.length - readPos);
    dst.set(this.ring.subarray(readPos, readPos + firstSpan), 0);
    if (toRead > firstSpan) {
      dst.set(this.ring.subarray(0, toRead - firstSpan), firstSpan);
    }
    this.readIdx = (this.readIdx + toRead) >>> 0;
    return { consumed: [], produced: [toRead] };
  }

  stop(): void {
    this.unsubscribe?.();
    this.unsubscribe = undefined;
  }

  private push(samples: Float32Array): void {
    const used = (this.writeIdx - this.readIdx) >>> 0;
    const free = this.ring.length - used;
    if (free === 0) {
      this._droppedFloats += samples.length;
      return;
    }
    const toWrite = Math.min(free, samples.length);
    const writePos = this.writeIdx & this.mask;
    const firstSpan = Math.min(toWrite, this.ring.length - writePos);
    this.ring.set(samples.subarray(0, firstSpan), writePos);
    if (toWrite > firstSpan) {
      this.ring.set(samples.subarray(firstSpan, toWrite), 0);
    }
    this.writeIdx = (this.writeIdx + toWrite) >>> 0;
    if (toWrite < samples.length) {
      this._droppedFloats += samples.length - toWrite;
    }
  }
}
