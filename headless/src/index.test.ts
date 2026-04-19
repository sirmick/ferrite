import { BlockRegistry, registerAll } from "@ferrite/flowgraph-blocks";
import { describe, expect, it } from "vitest";

import { HEADLESS_VERSION, runFlowgraphHeadless } from "./index.js";

function freshRegistry() {
  const reg = new BlockRegistry();
  registerAll(reg);
  return reg;
}

describe("headless package", () => {
  it("exposes a version string", () => {
    expect(HEADLESS_VERSION).toMatch(/^\d+\.\d+\.\d+$/);
  });

  it("runs a minimal node-environment flowgraph through its lifecycle", async () => {
    const doc = {
      name: "passthru-smoke",
      environments: ["node"],
      blocks: {
        a: { type: "Passthru", params: { gain: 1.0 } },
        b: { type: "Passthru", params: { gain: 0.5 } },
      },
      wires: [["a.out", "b.in"]],
    };
    const graph = await runFlowgraphHeadless(JSON.stringify(doc), {
      registry: freshRegistry(),
    });
    expect(graph.environment).toBe("node");
    expect([...graph.instances.keys()].sort()).toEqual(["a", "b"]);
  });

  it("invokes the afterInit hook between init() and start()", async () => {
    const doc = {
      name: "after-init",
      environments: ["node"],
      blocks: { a: { type: "Passthru", params: { gain: 1.0 } } },
      wires: [],
    };
    let seenIds: string[] = [];
    await runFlowgraphHeadless(JSON.stringify(doc), {
      registry: freshRegistry(),
      afterInit: (graph) => {
        seenIds = [...graph.instances.keys()];
      },
    });
    expect(seenIds).toEqual(["a"]);
  });

  it("rejects flowgraphs that omit the node environment", async () => {
    const browserOnly = {
      name: "browser-only",
      environments: ["browser"],
      blocks: { a: { type: "Passthru", params: {} } },
      wires: [],
    };
    await expect(
      runFlowgraphHeadless(JSON.stringify(browserOnly), {
        registry: freshRegistry(),
      }),
    ).rejects.toThrow(/node/);
  });
});
