// AudioSink — terminal block that pushes real-valued audio samples into
// a SharedArrayBuffer ring, to be consumed by the AudioWorklet on the
// audio thread (see `audioRingProcessor.ts`).
//
// The scheduler drives `process(io)` on whatever cadence the flowgraph
// runs at; the AudioWorklet drains on its own 48 kHz / 128-frame clock.
// When the ring fills (consumer slow, producer fast) we drop the
// overflow rather than backpressure — audio pacing belongs to the
// audio clock, and a glitch is preferable to a stall.
//
// The block owns the SAB. The main thread retrieves it via `sab` and
// hands it to the `AudioWorkletNode` (either via `processorOptions.sab`
// at construction or by posting `{ sab }` to `node.port` afterwards).

import type { BlockInstance, BlockIo, Work } from '@ferrite/flowgraph-runtime/types';

import { AudioRingWriter } from './ringBuffer.js';

export interface AudioSinkParams {
  /**
   * Ring capacity in samples. Must be a power of two. Defaults to
   * 8192 — about 170 ms at 48 kHz, enough headroom for GC pauses and
   * flowgraph scheduling jitter without adding noticeable latency to
   * the live listening experience.
   */
  bufferSamples?: number;
}

const DEFAULT_BUFFER_SAMPLES = 8192;

export class AudioSink implements BlockInstance {
  private readonly writer: AudioRingWriter;
  private _droppedSamples = 0;

  constructor(params: AudioSinkParams = {}) {
    const cap = params.bufferSamples ?? DEFAULT_BUFFER_SAMPLES;
    this.writer = AudioRingWriter.create(cap);
  }

  /**
   * The SAB backing the ring — hand this to the AudioWorkletNode.
   * Stable for the lifetime of the block; safe to read once after
   * construction.
   */
  get sab(): SharedArrayBuffer {
    return this.writer.sab;
  }

  /** Cumulative count of samples dropped due to ring overflow. */
  get droppedSamples(): number {
    return this._droppedSamples;
  }

  init(): void {
    this.writer.reset();
    this._droppedSamples = 0;
  }

  process(io: BlockIo): Work {
    const src = io.input('in');
    if (!src) return { consumed: [0], produced: [] };
    const samples = src.data as Float32Array;
    const written = this.writer.write(samples);
    if (written < samples.length) {
      this._droppedSamples += samples.length - written;
    }
    // Report the full input as consumed even on overflow. The
    // alternative (reporting `written`) would make the scheduler
    // re-present the same samples, effectively backpressuring the
    // upstream — which stalls the radio pipeline to keep audio pacing
    // in sync with the flowgraph clock rather than the audio clock.
    // For live listening the audio clock wins; drop and move on.
    return { consumed: [samples.length], produced: [] };
  }

  stop(): void {
    this.writer.reset();
  }
}
