// Decimator — parity with the Rust reference.

import { describe, expect, it } from "vitest";

import type { BlockIo } from "@ferrite/flowgraph-runtime/types";

import { Decimator, designLpf } from "./block.js";

const TAU = Math.PI * 2;

function makeIo(input: Float32Array, outSamples: number): BlockIo {
  const out = new Float32Array(outSamples * 2);
  const inputs = [
    {
      name: "in" as const,
      portType: "iq_f32" as const,
      meta: { sampleRateHz: 240_000, centerFreqHz: 0 },
      data: input,
    },
  ];
  const outputs = [
    {
      name: "out" as const,
      portType: "iq_f32" as const,
      meta: { sampleRateHz: 60_000, centerFreqHz: 0 },
      data: out,
    },
  ];
  return {
    inputs,
    outputs,
    input(name) {
      return inputs.find((p) => p.name === name);
    },
    output(name) {
      return outputs.find((p) => p.name === name);
    },
  };
}

function iqOnes(n: number): Float32Array {
  const out = new Float32Array(n * 2);
  for (let i = 0; i < n; i++) {
    out[i * 2] = 1;
    out[i * 2 + 1] = 0;
  }
  return out;
}

function iqTone(n: number, fNorm: number): Float32Array {
  const out = new Float32Array(n * 2);
  for (let i = 0; i < n; i++) {
    const t = TAU * fNorm * i;
    out[i * 2] = Math.cos(t);
    out[i * 2 + 1] = Math.sin(t);
  }
  return out;
}

describe("designLpf", () => {
  it("has unit DC gain", () => {
    const taps = designLpf(33, 0.1);
    let sum = 0;
    for (const t of taps) sum += t;
    expect(Math.abs(sum - 1)).toBeLessThan(1e-6);
  });
});

describe("Decimator", () => {
  it("rejects bad params", () => {
    expect(() => new Decimator({ factor: 0 })).toThrow();
    expect(() => new Decimator({ factor: 2, num_taps: 2 })).toThrow();
    expect(() => new Decimator({ factor: 2, cutoff_normalized: 0 })).toThrow();
    expect(
      () => new Decimator({ factor: 2, cutoff_normalized: 0.5 }),
    ).toThrow();
  });

  it("passes DC with unit gain after warm-up", () => {
    const dec = new Decimator({ factor: 4 });
    dec.init({
      frameHint: 1024,
      inputRate: () => 240_000,
      outputRate: () => 60_000,
    });
    const nTaps = dec.taps.length;
    const input = iqOnes(4 * nTaps);
    const io = makeIo(input, input.length / 4 / 2 + 4);
    dec.process(io);
    const out = io.outputs[0]!.data as Float32Array;
    // Check the last four produced samples (past warm-up).
    const lastIdx = (input.length / 2 / 4 - 1) * 2;
    for (let k = lastIdx - 6; k <= lastIdx; k += 2) {
      expect(Math.abs(out[k]! - 1)).toBeLessThan(1e-4);
      expect(Math.abs(out[k + 1]!)).toBeLessThan(1e-4);
    }
  });

  it("holds the rate invariant across chunked calls", () => {
    const dec = new Decimator({ factor: 3 });
    dec.init({
      frameHint: 1024,
      inputRate: () => 240_000,
      outputRate: () => 80_000,
    });
    const chunks = [1, 2, 3, 4, 7, 11];
    const totalIn = chunks.reduce((a, b) => a + b, 0);
    let totalOut = 0;
    for (const n of chunks) {
      const input = new Float32Array(n * 2);
      for (let i = 0; i < n * 2; i += 2) {
        input[i] = 0.5;
        input[i + 1] = -0.5;
      }
      const io = makeIo(input, n + 1);
      const w = dec.process(io);
      expect(w.consumed[0]).toBe(n * 2);
      totalOut += w.produced[0]! / 2;
    }
    expect(totalOut).toBe(Math.floor(totalIn / 3));
  });

  it("rejects out-of-band tones, preserves in-band", () => {
    const measureGain = (toneHzNorm: number): number => {
      const dec = new Decimator({
        factor: 4,
        num_taps: 65,
        cutoff_normalized: 0.11,
      });
      dec.init({
        frameHint: 4096,
        inputRate: () => 240_000,
        outputRate: () => 60_000,
      });
      const n = 4096;
      const input = iqTone(n, toneHzNorm);
      const io = makeIo(input, n / 4 + 4);
      const w = dec.process(io);
      const out = io.outputs[0]!.data as Float32Array;
      const produced = w.produced[0]! / 2;
      const warm = Math.floor(65 / 4) + 4;
      let rms = 0;
      const count = produced - warm;
      for (let i = warm; i < produced; i++) {
        const re = out[i * 2]!;
        const im = out[i * 2 + 1]!;
        rms += re * re + im * im;
      }
      return Math.sqrt(rms / count);
    };
    expect(measureGain(0.05)).toBeGreaterThan(0.9);
    expect(measureGain(0.3)).toBeLessThan(0.05);
  });

  it("pauses cleanly on output backpressure", () => {
    const dec = new Decimator({ factor: 4 });
    dec.init({
      frameHint: 1024,
      inputRate: () => 240_000,
      outputRate: () => 60_000,
    });
    const input = iqOnes(40);
    const ioA = makeIo(input, 2);
    const wA = dec.process(ioA);
    expect(wA.produced[0]! / 2).toBe(2);
    const consumedA = wA.consumed[0]! / 2;
    expect(consumedA).toBeLessThanOrEqual(40);
    const remainder = input.subarray(consumedA * 2);
    const ioB = makeIo(remainder, 20);
    const wB = dec.process(ioB);
    expect(wA.produced[0]! / 2 + wB.produced[0]! / 2).toBe(10);
    expect(consumedA + wB.consumed[0]! / 2).toBe(40);
  });
});
