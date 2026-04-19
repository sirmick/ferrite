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
