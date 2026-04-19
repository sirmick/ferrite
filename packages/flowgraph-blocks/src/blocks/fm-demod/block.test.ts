// FmDemod — parity with the Rust reference implementation.

import { describe, expect, it } from "vitest";

import type { BlockIo, Work } from "@ferrite/flowgraph-runtime/types";

import { FmDemod } from "./block.js";

const TAU = Math.PI * 2;

function makeIo(input: Float32Array, outSamples: number): BlockIo {
  const out = new Float32Array(outSamples);
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
      portType: "real_f32" as const,
      meta: { sampleRateHz: 240_000, centerFreqHz: 0 },
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

function run(
  demod: FmDemod,
  iq: Float32Array,
): { out: Float32Array; work: Work } {
  const io = makeIo(iq, iq.length >>> 1);
  const work = demod.process(io);
  return { out: io.outputs[0]!.data as Float32Array, work };
}

function toneIq(n: number, fTest: number, fs: number): Float32Array {
  const out = new Float32Array(n * 2);
  const step = (TAU * fTest) / fs;
  for (let i = 0; i < n; i++) {
    const t = step * i;
    out[i * 2] = Math.cos(t);
    out[i * 2 + 1] = Math.sin(t);
  }
  return out;
}

describe("FmDemod", () => {
  it("rejects non-positive rates", () => {
    expect(
      () => new FmDemod({ sample_rate_hz: 0, max_deviation_hz: 75_000 }),
    ).toThrow();
    expect(
      () => new FmDemod({ sample_rate_hz: 240_000, max_deviation_hz: -1 }),
    ).toThrow();
    expect(
      () => new FmDemod({ sample_rate_hz: NaN, max_deviation_hz: 75_000 }),
    ).toThrow();
  });

  it("DC input yields zero output", () => {
    const demod = new FmDemod();
    demod.init({
      frameHint: 1024,
      inputRate: () => 240_000,
      outputRate: () => 240_000,
    });
    // Same complex value repeated — phase derivative is 0 once prev is set.
    const n = 128;
    const input = new Float32Array(n * 2);
    for (let i = 0; i < n; i++) {
      input[i * 2] = 0.7;
      input[i * 2 + 1] = -0.3;
    }
    const { out } = run(demod, input);
    for (let i = 0; i < out.length; i++) {
      expect(Math.abs(out[i]!)).toBeLessThan(1e-6);
    }
  });

  it("tone at half deviation gives half-scale audio", () => {
    const fs = 240_000;
    const dev = 75_000;
    const fTest = dev / 2;
    const demod = new FmDemod({ sample_rate_hz: fs, max_deviation_hz: dev });
    demod.init({ frameHint: 1024, inputRate: () => fs, outputRate: () => fs });
    const input = toneIq(256, fTest, fs);
    const { out } = run(demod, input);
    // Skip first sample (prev = 0 on first call).
    for (let i = 1; i < out.length; i++) {
      expect(Math.abs(out[i]! - 0.5)).toBeLessThan(1e-4);
    }
  });

  it("tone at -deviation gives -1.0 audio", () => {
    const fs = 240_000;
    const dev = 75_000;
    const demod = new FmDemod({ sample_rate_hz: fs, max_deviation_hz: dev });
    demod.init({ frameHint: 1024, inputRate: () => fs, outputRate: () => fs });
    const input = toneIq(128, -dev, fs);
    const { out } = run(demod, input);
    for (let i = 1; i < out.length; i++) {
      expect(Math.abs(out[i]! + 1.0)).toBeLessThan(1e-4);
    }
  });

  it("prev persists across process calls", () => {
    const fs = 240_000;
    const dev = 75_000;
    const fTest = dev / 4;
    const whole = new FmDemod({ sample_rate_hz: fs, max_deviation_hz: dev });
    const split = new FmDemod({ sample_rate_hz: fs, max_deviation_hz: dev });
    const ctx = { frameHint: 1024, inputRate: () => fs, outputRate: () => fs };
    whole.init(ctx);
    split.init(ctx);
    const input = toneIq(64, fTest, fs);
    const { out: full } = run(whole, input);
    const { out: first } = run(split, input.subarray(0, 64));
    const { out: second } = run(split, input.subarray(64));
    const joined = new Float32Array(first.length + second.length);
    joined.set(first);
    joined.set(second, first.length);
    for (let i = 0; i < full.length; i++) {
      expect(Math.abs(full[i]! - joined[i]!)).toBeLessThan(1e-6);
    }
  });

  it("reports consumed in floats, produced in samples", () => {
    const demod = new FmDemod();
    demod.init({
      frameHint: 1024,
      inputRate: () => 240_000,
      outputRate: () => 240_000,
    });
    const input = toneIq(10, 1_000, 240_000);
    const { work } = run(demod, input);
    expect(work.consumed[0]).toBe(20); // 10 samples × 2 floats
    expect(work.produced[0]).toBe(10);
  });
});
