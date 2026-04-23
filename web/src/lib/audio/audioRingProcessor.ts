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

/** Peak+RMS over one 128-frame batch, before gain is applied. */
export interface AudioBatchStats {
  peak: number;
  rms: number;
}

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
 *   • `gain` scales the samples in place (post-underrun-fill so the
 *     silence portion stays at zero regardless). Applied pre-fanout so
 *     every channel sees the same scaled sample.
 *
 * Returns peak/RMS computed on the **pre-gain** samples — the meter
 * reflects source level, not the user's volume knob position.
 */
export function fillAudioBatch(
  reader: AudioRingReader | null,
  output: Float32Array[],
  gain = 1,
): AudioBatchStats {
  if (output.length === 0) return { peak: 0, rms: 0 };
  const first = output[0];
  if (!first || first.length === 0) return { peak: 0, rms: 0 };
  if (reader) {
    const n = reader.read(first);
    if (n < first.length) first.fill(0, n);
  } else {
    first.fill(0);
  }
  let peak = 0;
  let sumSq = 0;
  for (let i = 0; i < first.length; i++) {
    const s = first[i]!;
    const abs = s < 0 ? -s : s;
    if (abs > peak) peak = abs;
    sumSq += s * s;
  }
  const rms = Math.sqrt(sumSq / first.length);
  if (gain !== 1) {
    for (let i = 0; i < first.length; i++) first[i] = first[i]! * gain;
  }
  for (let ch = 1; ch < output.length; ch++) {
    output[ch]!.set(first);
  }
  return { peak, rms };
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

/** Message shape the main thread sends over `node.port` to adjust
 *  playback. All fields optional — each message is a partial. */
export interface AudioControlMessage {
  sab?: SharedArrayBuffer;
  /** Linear gain applied after the ring read. 0 = silent, 1 = unity. */
  volume?: number;
  /** Hard mute independent of `volume`. Handy for toggling without
   *  losing the user's slider position. */
  muted?: boolean;
}

/** Message shape the worklet posts back via `node.port` at ~30 Hz.
 *  Peak/RMS are pre-gain linear values in [0, 1] nominally. */
export interface AudioLevelMessage {
  type: 'level';
  peak: number;
  rms: number;
}

interface ProcessorOptions {
  readonly sab?: SharedArrayBuffer;
}

// How many 128-frame process() calls to aggregate before posting a
// level message back to the main thread. At 48 kHz that's ~375 Hz
// process calls; batching 12 → ~31 Hz updates, smooth for a meter
// without drowning the message port.
const LEVEL_REPORT_EVERY = 12;

// The registerProcessor call below only runs when this module is
// loaded inside an AudioWorkletGlobalScope. In any other context
// (Vite dev-transform, unit-test import, SSR) `registerProcessor` is
// undefined, so we guard the call — the `fillAudioBatch` export
// remains usable from the main thread for testing.
if (typeof (globalThis as { registerProcessor?: unknown }).registerProcessor === 'function') {
  class AudioRingConsumerProcessor extends AudioWorkletProcessor {
    private reader: AudioRingReader | null = null;
    private volume = 1;
    private muted = false;
    private batchesSinceReport = 0;
    private peakAcc = 0;
    private sumSqAcc = 0;
    private samplesAcc = 0;

    constructor(options?: AudioWorkletNodeOptions) {
      super(options);
      const pOpts = options?.processorOptions as ProcessorOptions | undefined;
      if (pOpts?.sab) {
        this.reader = AudioRingReader.fromSab(pOpts.sab);
      }
      this.port.onmessage = (ev: MessageEvent) => {
        // Partial updates — main thread sends one field at a time.
        const msg = ev.data as AudioControlMessage | undefined;
        if (!msg) return;
        if (msg.sab instanceof SharedArrayBuffer) {
          this.reader = AudioRingReader.fromSab(msg.sab);
        }
        if (typeof msg.volume === 'number' && Number.isFinite(msg.volume)) {
          this.volume = Math.max(0, msg.volume);
        }
        if (typeof msg.muted === 'boolean') {
          this.muted = msg.muted;
        }
      };
    }

    process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
      const output = outputs[0];
      if (!output) return true;
      const gain = this.muted ? 0 : this.volume;
      const stats = fillAudioBatch(this.reader, output, gain);

      // Aggregate batch stats, post every N batches. We aggregate peak
      // as the max and RMS as a true RMS over the combined window
      // (sum of squares is additive, sample count too).
      if (stats.peak > this.peakAcc) this.peakAcc = stats.peak;
      const samples = output[0]?.length ?? 0;
      this.sumSqAcc += stats.rms * stats.rms * samples;
      this.samplesAcc += samples;
      this.batchesSinceReport += 1;
      if (this.batchesSinceReport >= LEVEL_REPORT_EVERY) {
        const rms = this.samplesAcc > 0 ? Math.sqrt(this.sumSqAcc / this.samplesAcc) : 0;
        this.port.postMessage({
          type: 'level',
          peak: this.peakAcc,
          rms,
        } satisfies AudioLevelMessage);
        this.peakAcc = 0;
        this.sumSqAcc = 0;
        this.samplesAcc = 0;
        this.batchesSinceReport = 0;
      }
      return true;
    }
  }
  registerProcessor(AUDIO_RING_PROCESSOR_NAME, AudioRingConsumerProcessor);
}
