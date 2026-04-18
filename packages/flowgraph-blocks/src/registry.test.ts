// Tests for the block-registry contract.
//
// These are tiny by design — the registry is a Map with one invariant
// (no duplicate typeName). The real test of the folder contract is that
// every shipped block module type-checks against `BlockModule`; tsc in
// `pnpm check` is what enforces that.

import { describe, expect, it } from "vitest";

import { BlockRegistry, blocks, registerAll } from "./index.js";

describe("BlockRegistry", () => {
  it("rejects duplicate type names", () => {
    const reg = new BlockRegistry();
    registerAll(reg);
    expect(() => registerAll(reg)).toThrow(/duplicate/);
  });

  it("registers every shipped block", () => {
    const reg = new BlockRegistry();
    registerAll(reg);
    for (const name of Object.keys(blocks)) {
      expect(reg.has(name)).toBe(true);
      expect(reg.get(name)?.spec.typeName).toBe(name);
    }
    expect(reg.size()).toBe(Object.keys(blocks).length);
  });

  it("lists block names sorted", () => {
    const reg = new BlockRegistry();
    registerAll(reg);
    const list = reg.list();
    const sorted = [...list].sort();
    expect(list).toEqual(sorted);
  });
});

describe("Passthru", () => {
  it("spec declares in/out real_f32 ports and a gain param", () => {
    const p = blocks.Passthru!;
    expect(p.spec.typeName).toBe("Passthru");
    expect(p.spec.inputs).toHaveLength(1);
    expect(p.spec.outputs).toHaveLength(1);
    expect(p.spec.inputs[0]!.portType).toBe("real_f32");
    expect(p.spec.outputs[0]!.portType).toBe("real_f32");
    const gain = p.spec.params[0]!;
    expect(gain.key).toBe("gain");
    expect(gain.mutableWhileStreaming).toBe(true);
  });

  it("construct returns a process-able BlockInstance", () => {
    const p = blocks.Passthru!;
    const instance = p.construct({ gain: 2.0 });
    const input = new Float32Array([1, 2, 3, 4]);
    const output = new Float32Array(4);
    const io = {
      inputs: [
        {
          name: "in",
          portType: "real_f32",
          meta: { sampleRateHz: 48_000, centerFreqHz: 0 },
          data: input,
        },
      ],
      outputs: [
        {
          name: "out",
          portType: "real_f32",
          meta: { sampleRateHz: 48_000, centerFreqHz: 0 },
          data: output,
        },
      ],
      input(name: string) {
        return this.inputs.find((pp) => pp.name === name);
      },
      output(name: string) {
        return this.outputs.find((pp) => pp.name === name);
      },
    } as const;
    const work = instance.process(io);
    expect(Array.from(output)).toEqual([2, 4, 6, 8]);
    expect(work.consumed[0]).toBe(4);
    expect(work.produced[0]).toBe(4);
  });

  it("update mutates gain at runtime", () => {
    const p = blocks.Passthru!;
    const instance = p.construct({ gain: 1.0 });
    instance.update?.({ gain: 0.5 });
    const input = new Float32Array([2, 4]);
    const output = new Float32Array(2);
    const io = {
      inputs: [
        {
          name: "in",
          portType: "real_f32",
          meta: { sampleRateHz: 48_000, centerFreqHz: 0 },
          data: input,
        },
      ],
      outputs: [
        {
          name: "out",
          portType: "real_f32",
          meta: { sampleRateHz: 48_000, centerFreqHz: 0 },
          data: output,
        },
      ],
      input(name: string) {
        return this.inputs.find((pp) => pp.name === name);
      },
      output(name: string) {
        return this.outputs.find((pp) => pp.name === name);
      },
    } as const;
    instance.process(io);
    expect(Array.from(output)).toEqual([1, 2]);
  });
});
