// AudioWorkletProcessor that pulls samples from the SAB ring buffer
// into the audio graph. Loaded via `audioContext.audioWorklet.addModule`
// on a URL resolved through Vite's `new URL(..., import.meta.url)`
// pattern, so this file must be self-contained per audio-graph realm —
// no runtime imports beyond the co-located ring buffer module (which
// Vite bundles into the worklet output).
//
// Contract:
//   • Construct the node with
//       new AudioWorkletNode(ctx, AUDIO_RING_PROCESSOR_NAME, {
//         processorOptions: { sab: <SharedArrayBuffer from AudioRingWriter> },
//         outputChannelCount: [1],
//       })
//     — or attach the SAB later by posting `{ sab }` to the port.
//   • Each process() call the worklet emits exactly 128 frames — the
//     Web Audio fixed batch — drawn from the ring. Underruns zero-fill
//     the remainder (AudioContext keeps running; user hears silence
//     rather than a stall).
//   • Every output channel receives the same samples (mono-fanned).
//     Producer writes mono; any stereo spread is a mixer concern.
//
// The processor lives until the node is disconnected; `process` returns
// true forever. External teardown is the owner's responsibility.

import { AudioRingReader } from './ringBuffer.js';

export const AUDIO_RING_PROCESSOR_NAME = 'audio-ring-consumer';

/**
 * Fill one 128-frame output batch from the ring. Extracted so it can
 * be exercised in unit tests without a live AudioContext — the
 * AudioWorkletProcessor class is only available on the audio thread.
 *
 *   • `reader === null` → silence on every channel (ring not wired up
 *     yet; the node posts messages asking for it).
 *   • Partial read → leading frames are real samples, trailing frames
 *     are zeros. Underrun in Web Audio equals silence.
 *   • Fan-out → channel 0 is read from the ring, channels 1..n are
 *     copied from channel 0.
 */
export function fillAudioBatch(reader: AudioRingReader | null, output: Float32Array[]): void {
  if (output.length === 0) return;
  const first = output[0];
  if (!first || first.length === 0) return;
  if (reader) {
    const n = reader.read(first);
    if (n < first.length) first.fill(0, n);
  } else {
    first.fill(0);
  }
  for (let ch = 1; ch < output.length; ch++) {
    output[ch]!.set(first);
  }
}

// Ambient declarations — Web Audio worklet globals aren't in the
// default TS libs. Keep these minimal: just what this file uses.
declare const AudioWorkletProcessor: {
  new (options?: AudioWorkletNodeOptions): AudioWorkletProcessor;
  prototype: AudioWorkletProcessor;
};
interface AudioWorkletProcessor {
  readonly port: MessagePort;
}
declare function registerProcessor(
  name: string,
  processor: new (options?: AudioWorkletNodeOptions) => AudioWorkletProcessor,
): void;

interface ProcessorOptions {
  readonly sab?: SharedArrayBuffer;
}

// The registerProcessor call below only runs when this module is
// loaded inside an AudioWorkletGlobalScope. In any other context
// (Vite dev-transform, unit-test import, SSR) `registerProcessor` is
// undefined, so we guard the call — the `fillAudioBatch` export
// remains usable from the main thread for testing.
if (typeof (globalThis as { registerProcessor?: unknown }).registerProcessor === 'function') {
  class AudioRingConsumerProcessor extends AudioWorkletProcessor {
    private reader: AudioRingReader | null = null;

    constructor(options?: AudioWorkletNodeOptions) {
      super(options);
      const pOpts = options?.processorOptions as ProcessorOptions | undefined;
      if (pOpts?.sab) {
        this.reader = AudioRingReader.fromSab(pOpts.sab);
      }
      this.port.onmessage = (ev: MessageEvent) => {
        // Late-bound SAB support — main thread can attach the ring
        // after the node is constructed (e.g. flowgraph starts later
        // than the AudioContext).
        const msg = ev.data as { sab?: SharedArrayBuffer } | undefined;
        if (msg?.sab instanceof SharedArrayBuffer) {
          this.reader = AudioRingReader.fromSab(msg.sab);
        }
      };
    }

    process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
      const output = outputs[0];
      if (output) fillAudioBatch(this.reader, output);
      return true;
    }
  }
  registerProcessor(AUDIO_RING_PROCESSOR_NAME, AudioRingConsumerProcessor);
}
