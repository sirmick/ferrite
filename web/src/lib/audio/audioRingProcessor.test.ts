import { describe, expect, it } from 'vitest';

import { fillAudioBatch } from './audioRingProcessor.js';
import { AudioRingReader, AudioRingWriter } from './ringBuffer.js';

const FRAME = 128; // Web Audio's fixed process batch.

function makeOutput(channels: number, frames = FRAME): Float32Array[] {
  return Array.from({ length: channels }, () => new Float32Array(frames));
}

describe('fillAudioBatch', () => {
  it('copies 128 frames from the ring into a single-channel output', () => {
    const w = AudioRingWriter.create(512);
    const r = AudioRingReader.fromSab(w.sab);
    const samples = new Float32Array(FRAME);
    for (let i = 0; i < FRAME; i++) samples[i] = i * 0.01;
    w.write(samples);

    const output = makeOutput(1);
    fillAudioBatch(r, output);
    expect(Array.from(output[0]!)).toEqual(Array.from(samples));
    expect(r.availableRead()).toBe(0);
  });

  it('zero-fills the tail of the batch on underrun', () => {
    const w = AudioRingWriter.create(512);
    const r = AudioRingReader.fromSab(w.sab);
    w.write(Float32Array.from({ length: 10 }, () => 0.5));

    const output = makeOutput(1);
    fillAudioBatch(r, output);
    const first10 = Array.from(output[0]!.subarray(0, 10));
    const rest = Array.from(output[0]!.subarray(10));
    expect(first10).toEqual(new Array(10).fill(0.5));
    expect(rest.every((v) => v === 0)).toBe(true);
  });

  it('emits pure silence when the ring is empty', () => {
    const w = AudioRingWriter.create(512);
    const r = AudioRingReader.fromSab(w.sab);
    const output = makeOutput(1);
    output[0]!.fill(0.9); // simulate leftover from a previous batch
    fillAudioBatch(r, output);
    expect(output[0]!.every((v) => v === 0)).toBe(true);
  });

  it('emits silence when no reader is attached yet', () => {
    const output = makeOutput(1);
    output[0]!.fill(0.7);
    fillAudioBatch(null, output);
    expect(output[0]!.every((v) => v === 0)).toBe(true);
  });

  it('fans mono samples out to every output channel', () => {
    const w = AudioRingWriter.create(512);
    const r = AudioRingReader.fromSab(w.sab);
    const samples = Float32Array.from({ length: FRAME }, (_, i) => i);
    w.write(samples);

    const output = makeOutput(2);
    fillAudioBatch(r, output);
    expect(Array.from(output[0]!)).toEqual(Array.from(samples));
    expect(Array.from(output[1]!)).toEqual(Array.from(samples));
  });

  it('streams across many 128-frame batches without drift', () => {
    const w = AudioRingWriter.create(1024);
    const r = AudioRingReader.fromSab(w.sab);
    const total = 10 * FRAME;
    // Producer pushes a full second+ of samples in one go (< capacity).
    const src = Float32Array.from({ length: total }, (_, i) => i * 0.001);
    // Chunk into bursts so the ring stays within capacity.
    for (let off = 0; off < total; off += 256) {
      w.write(src.subarray(off, off + 256));
      // Consumer processes two 128-batches per burst.
      for (let b = 0; b < 2; b++) {
        const output = makeOutput(1);
        fillAudioBatch(r, output);
        const expected = src.subarray(off + b * FRAME, off + (b + 1) * FRAME);
        expect(Array.from(output[0]!)).toEqual(Array.from(expected));
      }
    }
    expect(r.availableRead()).toBe(0);
  });

  it('is a no-op when output has no channels', () => {
    expect(() => fillAudioBatch(null, [])).not.toThrow();
  });

  it('is a no-op when the first channel has zero frames', () => {
    const output = [new Float32Array(0)];
    expect(() => fillAudioBatch(null, output)).not.toThrow();
  });
});
