// Decimator — integer-factor decimation with windowed-sinc anti-alias LPF.
//
// One IQ-in, one IQ-out. Mirrors the Rust `Decimator` in
// `blocks/src/decimator.rs`: Hann-windowed sinc filter normalised to
// unit DC gain, phase counter emits every `factor`-th sample. State
// (delay ring + phase) persists across `process` calls, so a chunked
// drive is indistinguishable from one long call.
//
// Unit convention: Work.consumed / Work.produced count **floats**, not
// complex samples. One IQ sample = 2 floats (interleaved re, im). This
// matches `WsIqSource` upstream and `FmDemod` downstream.

import type {
  BlockInstance,
  BlockIo,
  InitCtx,
  Work,
} from "@ferrite/flowgraph-runtime/types";

export interface DecimatorParams {
  /** Integer decimation factor M ≥ 1. Output rate = input rate / M. */
  factor?: number;
  /** FIR length. Defaults to 8·M + 1. */
  num_taps?: number;
  /** LPF cutoff as a fraction of input rate, in (0, 0.5). */
  cutoff_normalized?: number;
}

export class Decimator implements BlockInstance {
  readonly #factor: number;
  readonly #taps: Float32Array;
  readonly #delayRe: Float32Array;
  readonly #delayIm: Float32Array;
  #delayIdx = 0;
  #phase = 0;

  constructor(params: DecimatorParams = {}) {
    const factor = Math.trunc(params.factor ?? 4);
    if (!(factor >= 1)) {
      throw new RangeError(`Decimator: factor must be ≥ 1 (got ${factor})`);
    }
    const defaultTaps = 8 * factor + 1;
    const numTaps = Math.trunc(params.num_taps ?? defaultTaps);
    if (!(numTaps >= 3)) {
      throw new RangeError(`Decimator: num_taps must be ≥ 3 (got ${numTaps})`);
    }
    const defaultCutoff = 0.4 / factor;
    const cutoff = params.cutoff_normalized ?? defaultCutoff;
    if (!(cutoff > 0 && cutoff < 0.5)) {
      throw new RangeError(
        `Decimator: cutoff_normalized must be in (0, 0.5) (got ${cutoff})`,
      );
    }
    this.#factor = factor;
    this.#taps = designLpf(numTaps, cutoff);
    this.#delayRe = new Float32Array(numTaps);
    this.#delayIm = new Float32Array(numTaps);
  }

  /** Filter taps — exposed for tests. */
  get taps(): Float32Array {
    return this.#taps;
  }

  get factor(): number {
    return this.#factor;
  }

  init(_ctx: InitCtx): void {
    this.#delayIdx = 0;
    this.#phase = 0;
    this.#delayRe.fill(0);
    this.#delayIm.fill(0);
  }

  process(io: BlockIo): Work {
    const src = io.input("in");
    const dst = io.output("out");
    if (!src || !dst) return { consumed: [0], produced: [0] };
    const from = src.data as Float32Array;
    const to = dst.data as Float32Array;
    const inSamples = from.length >>> 1;
    const outCap = to.length >>> 1;
    const n = this.#taps.length;
    let consumed = 0;
    let produced = 0;
    let delayIdx = this.#delayIdx;
    let phase = this.#phase;
    const factor = this.#factor;
    const delayRe = this.#delayRe;
    const delayIm = this.#delayIm;
    for (let i = 0; i < inSamples; i++) {
      // Backpressure: if the next input would produce an output but we
      // have no room, stop. Don't consume the input — the scheduler
      // will call us again when space opens up.
      if (phase + 1 === factor && produced === outCap) break;
      delayRe[delayIdx] = from[i * 2]!;
      delayIm[delayIdx] = from[i * 2 + 1]!;
      delayIdx++;
      if (delayIdx === n) delayIdx = 0;
      consumed++;
      phase++;
      if (phase === factor) {
        phase = 0;
        // Convolve the delay ring starting from the oldest (just-overwritten) slot.
        let accRe = 0;
        let accIm = 0;
        let idx = delayIdx;
        for (let k = 0; k < n; k++) {
          const t = this.#taps[k]!;
          accRe += delayRe[idx]! * t;
          accIm += delayIm[idx]! * t;
          idx++;
          if (idx === n) idx = 0;
        }
        to[produced * 2] = accRe;
        to[produced * 2 + 1] = accIm;
        produced++;
      }
    }
    this.#delayIdx = delayIdx;
    this.#phase = phase;
    return { consumed: [consumed * 2], produced: [produced * 2] };
  }

  stop(): void {
    this.#delayIdx = 0;
    this.#phase = 0;
    this.#delayRe.fill(0);
    this.#delayIm.fill(0);
  }
}

/** Hann-windowed sinc LPF, normalised so taps sum to 1 (unit DC gain). */
export function designLpf(numTaps: number, cutoff: number): Float32Array {
  const m = (numTaps - 1) / 2;
  const h = new Float32Array(numTaps);
  const TAU = Math.PI * 2;
  for (let n = 0; n < numTaps; n++) {
    const k = n - m;
    const ideal =
      Math.abs(k) < 1e-7
        ? 2 * cutoff
        : Math.sin(2 * Math.PI * cutoff * k) / (Math.PI * k);
    const windowPhase = (TAU * n) / (numTaps - 1);
    const window = 0.5 * (1 - Math.cos(windowPhase));
    h[n] = ideal * window;
  }
  let sum = 0;
  for (let i = 0; i < numTaps; i++) sum += h[i]!;
  for (let i = 0; i < numTaps; i++) h[i]! /= sum;
  return h;
}
