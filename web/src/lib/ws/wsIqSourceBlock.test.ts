import { describe, expect, it } from 'vitest';

import type { BlockIo, PortBuf } from '@ferrite/flowgraph-runtime/types';

import type { FrameClient, FrameHandler } from './client.js';
import { HEADER_LEN, PROTOCOL_VERSION, PayloadType } from './frame.js';
import { WsIqSource } from './wsIqSourceBlock.js';

// ---------------------------------------------------------------------------
// FrameClient test double — captures subscribers and lets tests synthesise
// frames directly. Only the shape `WsIqSource` uses is implemented.
// ---------------------------------------------------------------------------

class FakeFrameClient {
  readonly handlers = new Map<number, Set<FrameHandler>>();

  subscribe(streamId: number, handler: FrameHandler): () => void {
    let set = this.handlers.get(streamId);
    if (!set) {
      set = new Set();
      this.handlers.set(streamId, set);
    }
    set.add(handler);
    return () => {
      set!.delete(handler);
      if (set!.size === 0) this.handlers.delete(streamId);
    };
  }

  close(): void {
    this.handlers.clear();
  }

  /** Deliver an iq_f32 payload on the given stream_id to any subscribers. */
  deliverIqF32(streamId: number, floats: Float32Array): void {
    const subs = this.handlers.get(streamId);
    if (!subs) return;
    const buf = new Uint8Array(HEADER_LEN + floats.byteLength);
    const dv = new DataView(buf.buffer);
    dv.setUint8(0, PROTOCOL_VERSION);
    dv.setUint8(1, PayloadType.IqF32);
    dv.setUint16(2, streamId, false);
    dv.setUint32(4, 0, false);
    dv.setBigUint64(8, 0n, false);
    buf.set(new Uint8Array(floats.buffer, floats.byteOffset, floats.byteLength), HEADER_LEN);
    for (const h of subs) {
      h({
        header: {
          version: PROTOCOL_VERSION,
          payloadType: PayloadType.IqF32,
          streamId,
          seq: 0,
          timestampNs: 0n,
        },
        payload: buf.subarray(HEADER_LEN),
      });
    }
  }

  /** Deliver a frame with a non-IQ payload type — used to test filtering. */
  deliverOther(streamId: number, payloadType: number): void {
    const subs = this.handlers.get(streamId);
    if (!subs) return;
    for (const h of subs) {
      h({
        header: {
          version: PROTOCOL_VERSION,
          payloadType: payloadType as never,
          streamId,
          seq: 0,
          timestampNs: 0n,
        },
        payload: new Uint8Array(0),
      });
    }
  }
}

function asFrameClient(f: FakeFrameClient): FrameClient {
  return f as unknown as FrameClient;
}

function portBuf(name: string, data: Float32Array): PortBuf {
  return {
    name,
    portType: 'iq_f32',
    meta: { sampleRateHz: 100_000, centerFreqHz: 0 },
    data,
  };
}

function io(outputs: PortBuf[]): BlockIo {
  return {
    inputs: [],
    outputs,
    input: () => undefined,
    output: (n) => outputs.find((p) => p.name === n),
  };
}

// ---------------------------------------------------------------------------

describe('WsIqSource', () => {
  it('subscribes to the configured stream_id on init()', () => {
    const fake = new FakeFrameClient();
    const src = new WsIqSource({ client: asFrameClient(fake), streamId: 2 });
    expect(fake.handlers.has(2)).toBe(false);
    src.init();
    expect(fake.handlers.get(2)?.size).toBe(1);
  });

  it('unsubscribes on stop()', () => {
    const fake = new FakeFrameClient();
    const src = new WsIqSource({ client: asFrameClient(fake), streamId: 2 });
    src.init();
    src.stop();
    expect(fake.handlers.has(2)).toBe(false);
  });

  it('buffers incoming iq_f32 samples and emits them on process()', () => {
    const fake = new FakeFrameClient();
    const src = new WsIqSource({
      client: asFrameClient(fake),
      streamId: 2,
      bufferFloats: 64,
    });
    src.init();
    fake.deliverIqF32(2, Float32Array.from([1, 2, 3, 4, 5, 6]));
    expect(src.bufferedFloats).toBe(6);

    const out = new Float32Array(6);
    const work = src.process(io([portBuf('out', out)]));
    expect(work.produced).toEqual([6]);
    expect(work.consumed).toEqual([]);
    expect(Array.from(out)).toEqual([1, 2, 3, 4, 5, 6]);
    expect(src.bufferedFloats).toBe(0);
  });

  it('emits a partial chunk when downstream buffer is larger than buffered', () => {
    const fake = new FakeFrameClient();
    const src = new WsIqSource({
      client: asFrameClient(fake),
      streamId: 2,
      bufferFloats: 64,
    });
    src.init();
    fake.deliverIqF32(2, Float32Array.from([10, 20]));

    const out = new Float32Array(8);
    const work = src.process(io([portBuf('out', out)]));
    expect(work.produced).toEqual([2]);
    expect(Array.from(out.subarray(0, 2))).toEqual([10, 20]);
    // Trailing slots untouched — downstream reads only the produced count.
    expect(out[2]).toBe(0);
  });

  it('chunks a large buffer across multiple process() calls', () => {
    const fake = new FakeFrameClient();
    const src = new WsIqSource({
      client: asFrameClient(fake),
      streamId: 2,
      bufferFloats: 64,
    });
    src.init();
    const samples = Float32Array.from({ length: 16 }, (_, i) => i);
    fake.deliverIqF32(2, samples);

    const out = new Float32Array(8);
    let work = src.process(io([portBuf('out', out)]));
    expect(work.produced).toEqual([8]);
    expect(Array.from(out)).toEqual([0, 1, 2, 3, 4, 5, 6, 7]);

    work = src.process(io([portBuf('out', out)]));
    expect(work.produced).toEqual([8]);
    expect(Array.from(out)).toEqual([8, 9, 10, 11, 12, 13, 14, 15]);
  });

  it('unwraps samples that straddle the ring boundary', () => {
    const fake = new FakeFrameClient();
    const src = new WsIqSource({
      client: asFrameClient(fake),
      streamId: 2,
      bufferFloats: 8,
    });
    src.init();
    // Fill to 6, drain 6 so the next write wraps.
    fake.deliverIqF32(2, Float32Array.from([1, 2, 3, 4, 5, 6]));
    src.process(io([portBuf('out', new Float32Array(6))]));
    fake.deliverIqF32(2, Float32Array.from([10, 20, 30, 40, 50, 60, 70, 80]));

    const out = new Float32Array(8);
    const work = src.process(io([portBuf('out', out)]));
    expect(work.produced).toEqual([8]);
    expect(Array.from(out)).toEqual([10, 20, 30, 40, 50, 60, 70, 80]);
  });

  it('drops overflow when the ring is full', () => {
    const fake = new FakeFrameClient();
    const src = new WsIqSource({
      client: asFrameClient(fake),
      streamId: 2,
      bufferFloats: 8,
    });
    src.init();
    fake.deliverIqF32(2, Float32Array.from([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]));
    expect(src.bufferedFloats).toBe(8);
    expect(src.droppedFloats).toBe(2);
    fake.deliverIqF32(2, Float32Array.from([100, 200]));
    expect(src.droppedFloats).toBe(4);
  });

  it('ignores frames with a non-IqF32 payload type on the same stream', () => {
    const fake = new FakeFrameClient();
    const src = new WsIqSource({ client: asFrameClient(fake), streamId: 2 });
    src.init();
    fake.deliverOther(2, PayloadType.FftU8);
    expect(src.bufferedFloats).toBe(0);
  });

  it('resets buffer and counters on re-init', () => {
    const fake = new FakeFrameClient();
    const src = new WsIqSource({
      client: asFrameClient(fake),
      streamId: 2,
      bufferFloats: 8,
    });
    src.init();
    fake.deliverIqF32(2, Float32Array.from([1, 2, 3, 4, 5, 6, 7, 8, 9]));
    expect(src.droppedFloats).toBe(1);
    src.stop();
    src.init();
    expect(src.droppedFloats).toBe(0);
    expect(src.bufferedFloats).toBe(0);
  });

  it('process() with no "out" port returns zero produced', () => {
    const fake = new FakeFrameClient();
    const src = new WsIqSource({ client: asFrameClient(fake), streamId: 2 });
    src.init();
    fake.deliverIqF32(2, Float32Array.from([1, 2]));
    const work = src.process(io([]));
    expect(work).toEqual({ consumed: [], produced: [0] });
  });

  it('rejects non-power-of-two bufferFloats', () => {
    const fake = new FakeFrameClient();
    expect(
      () => new WsIqSource({ client: asFrameClient(fake), streamId: 2, bufferFloats: 100 }),
    ).toThrow(/power of two/);
  });
});
