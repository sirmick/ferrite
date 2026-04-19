// FmDemod — complex IQ → real audio, phase-discriminator style.
//
// Mirrors the Rust `FmDemod` in `blocks/src/fm_demod.rs`: for every IQ
// sample x[n] the block computes `atan2(x[n] · conj(x[n-1]))`, i.e. the
// phase advance per sample, then rescales by `fs / (2π · deviation)`
// so a full ±deviation swing maps to ±1.0.
//
// Unit convention matches the other TS blocks: `Work.consumed` and
// `Work.produced` count **Float32Array elements**, not logical samples.
// One iq_f32 sample occupies two floats (interleaved re, im); one
// real_f32 sample occupies one float. This keeps `process` composable
// with buffers produced by `WsIqSource` upstream.

import type {
  BlockInstance,
  BlockIo,
  InitCtx,
  Work,
} from "@ferrite/flowgraph-runtime/types";

const TAU = Math.PI * 2;

export interface FmDemodParams {
  /** Input IQ sample rate (Hz). */
  sample_rate_hz?: number;
  /** Peak FM deviation (Hz). Audio full-scale maps to this. */
  max_deviation_hz?: number;
}

export class FmDemod implements BlockInstance {
  #gain: number;
  #prevRe = 0;
  #prevIm = 0;

  constructor(params: FmDemodParams = {}) {
    const fs = params.sample_rate_hz ?? 240_000;
    const dev = params.max_deviation_hz ?? 75_000;
    if (!(Number.isFinite(fs) && fs > 0)) {
      throw new RangeError(`FmDemod: sample_rate_hz must be > 0 (got ${fs})`);
    }
    if (!(Number.isFinite(dev) && dev > 0)) {
      throw new RangeError(
        `FmDemod: max_deviation_hz must be > 0 (got ${dev})`,
      );
    }
    this.#gain = fs / (TAU * dev);
  }

  /** Discriminator scale factor — audio = atan2(...) · gain. */
  get gain(): number {
    return this.#gain;
  }

  init(_ctx: InitCtx): void {
    this.#prevRe = 0;
    this.#prevIm = 0;
  }

  process(io: BlockIo): Work {
    const src = io.input("in");
    const dst = io.output("out");
    if (!src || !dst) return { consumed: [0], produced: [0] };
    const from = src.data as Float32Array;
    const to = dst.data as Float32Array;
    // `from.length` is float-count; two floats per IQ sample. Number of
    // output samples we can produce is bounded by both sides.
    const iqSamples = Math.min(from.length >>> 1, to.length);
    let prevRe = this.#prevRe;
    let prevIm = this.#prevIm;
    const g = this.#gain;
    for (let i = 0; i < iqSamples; i++) {
      const re = from[i * 2]!;
      const im = from[i * 2 + 1]!;
      // x · conj(prev) = (re·prevRe + im·prevIm) + j(im·prevRe − re·prevIm)
      const pRe = re * prevRe + im * prevIm;
      const pIm = im * prevRe - re * prevIm;
      to[i] = Math.atan2(pIm, pRe) * g;
      prevRe = re;
      prevIm = im;
    }
    this.#prevRe = prevRe;
    this.#prevIm = prevIm;
    return { consumed: [iqSamples * 2], produced: [iqSamples] };
  }

  stop(): void {
    this.#prevRe = 0;
    this.#prevIm = 0;
  }
}
