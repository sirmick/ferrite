import { describe, expect, it } from 'vitest';

import type { BlockIo, PortBuf } from '@ferrite/flowgraph-runtime/types';

import { AudioSink } from './audioSinkBlock.js';
import { AudioRingReader } from './ringBuffer.js';

function portBuf(name: string, data: Float32Array): PortBuf {
  return {
    name,
    portType: 'real_f32',
    meta: { sampleRateHz: 48000, centerFreqHz: 0 },
    data,
  };
}

function io(inputs: PortBuf[], outputs: PortBuf[] = []): BlockIo {
  return {
    inputs,
    outputs,
    input: (name) => inputs.find((p) => p.name === name),
    output: (name) => outputs.find((p) => p.name === name),
  };
}

describe('AudioSink', () => {
  it('exposes a ready-to-use SAB after construction', () => {
    const sink = new AudioSink({ bufferSamples: 1024 });
    expect(sink.sab.byteLength).toBeGreaterThan(1024 * 4);
    // SAB is readable as an AudioRingReader with the same capacity.
    const reader = AudioRingReader.fromSab(sink.sab);
    expect(reader.capacity).toBe(1024);
  });

  it('defaults to 8192-sample capacity', () => {
    const sink = new AudioSink();
    expect(AudioRingReader.fromSab(sink.sab).capacity).toBe(8192);
  });

  it('writes input samples into the ring', () => {
    const sink = new AudioSink({ bufferSamples: 1024 });
    sink.init();
    const src = Float32Array.from({ length: 256 }, (_, i) => i / 256);
    const work = sink.process(io([portBuf('in', src)]));
    expect(work.consumed).toEqual([256]);
    expect(work.produced).toEqual([]);
    const reader = AudioRingReader.fromSab(sink.sab);
    const out = new Float32Array(256);
    expect(reader.read(out)).toBe(256);
    expect(Array.from(out)).toEqual(Array.from(src));
  });

  it('reports full consumption and drops overflow on ring full', () => {
    const sink = new AudioSink({ bufferSamples: 16 });
    sink.init();
    const src = Float32Array.from({ length: 20 }, (_, i) => i);
    const work = sink.process(io([portBuf('in', src)]));
    // Full length reported — the sink is an absorbing terminator.
    expect(work.consumed).toEqual([20]);
    // Only 16 fit in the ring; 4 were dropped.
    expect(sink.droppedSamples).toBe(4);
  });

  it('accumulates dropped-sample counts across multiple ticks', () => {
    const sink = new AudioSink({ bufferSamples: 8 });
    sink.init();
    sink.process(io([portBuf('in', Float32Array.from([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]))]));
    sink.process(io([portBuf('in', Float32Array.from([11, 12, 13]))]));
    expect(sink.droppedSamples).toBe(2 + 3); // 2 overflow + 3 all-dropped
  });

  it('resets state on init()', () => {
    const sink = new AudioSink({ bufferSamples: 8 });
    sink.init();
    sink.process(io([portBuf('in', Float32Array.from([1, 2, 3, 4, 5, 6, 7, 8, 9]))]));
    expect(sink.droppedSamples).toBe(1);
    sink.init();
    expect(sink.droppedSamples).toBe(0);
    // Ring drained.
    const reader = AudioRingReader.fromSab(sink.sab);
    expect(reader.availableRead()).toBe(0);
  });

  it('is a no-op when no "in" port is wired', () => {
    const sink = new AudioSink({ bufferSamples: 16 });
    sink.init();
    const work = sink.process(io([]));
    expect(work).toEqual({ consumed: [0], produced: [] });
  });

  it('drains the ring on stop()', () => {
    const sink = new AudioSink({ bufferSamples: 16 });
    sink.init();
    sink.process(io([portBuf('in', Float32Array.from([1, 2, 3, 4]))]));
    const reader = AudioRingReader.fromSab(sink.sab);
    expect(reader.availableRead()).toBe(4);
    sink.stop();
    expect(reader.availableRead()).toBe(0);
  });

  it('rejects non-power-of-two buffer size', () => {
    expect(() => new AudioSink({ bufferSamples: 1000 })).toThrow(/power of two/);
  });
});
