// Smoke-test every JSON under `flowgraphs/` at the repo root against
// the structural validator. Catches typos, dangling wires, or shape
// drift in checked-in presets without needing the full block registry.
//
// This test does *not* exercise instantiation — block types referenced
// by a preset may not yet be registered (Rust blocks ship their TS
// wrappers in later commits). Structural validity is the strongest
// guarantee we can make at this layer.

import { readdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { describe, expect, it } from "vitest";

import { parseFlowgraph } from "./validate.js";

const repoRoot = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "..",
);
const flowgraphsDir = join(repoRoot, "flowgraphs");

const jsonFiles = readdirSync(flowgraphsDir).filter((f) => f.endsWith(".json"));

describe("shipped flowgraph presets", () => {
  it("flowgraphs/ directory is not empty", () => {
    expect(jsonFiles.length).toBeGreaterThan(0);
  });

  for (const file of jsonFiles) {
    it(`${file} parses and passes structural validation`, () => {
      const text = readFileSync(join(flowgraphsDir, file), "utf-8");
      const { doc, warnings } = parseFlowgraph(text);
      // `name` must match the filename minus `.json` — keeps presets
      // addressable by a stable slug and catches renames.
      expect(doc.name).toBe(file.replace(/\.json$/, ""));
      expect(warnings).toEqual([]);
    });
  }
});
