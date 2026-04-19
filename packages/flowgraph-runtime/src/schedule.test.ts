import { describe, expect, it } from "vitest";

import {
  instantiateFlowgraph,
  type BlockRegistryLike,
  type RegisteredBlock,
} from "./instantiate.js";
import { Scheduler, buildWirePlan, topologicalOrder } from "./schedule.js";
import type {
  BlockInstance,
  BlockIo,
  BlockSpec,
  FlowgraphDoc,
  Work,
} from "./types.js";

// ---------------------------------------------------------------------------
// Fixtures: no-op blocks that satisfy BlockInstance without doing work.
// ---------------------------------------------------------------------------

class NoOp implements BlockInstance {
  init(): void {}
  process(_io: BlockIo): Work {
    return { consumed: [0], produced: [0] };
  }
}

function makeBlock(spec: BlockSpec): RegisteredBlock {
  return { spec, construct: () => new NoOp() };
}

function registry(...specs: BlockSpec[]): BlockRegistryLike {
  const m = new Map<string, RegisteredBlock>();
  for (const s of specs) m.set(s.typeName, makeBlock(s));
  return {
    get: (n) => m.get(n),
    has: (n) => m.has(n),
  };
}

const IQ_SRC: BlockSpec = {
  typeName: "IqSrc",
  placement: "either",
  inputs: [],
  outputs: [{ name: "out", portType: "iq_f32" }],
  params: [],
};
const IQ_TEE: BlockSpec = {
  typeName: "IqTee",
  placement: "either",
  inputs: [{ name: "in", portType: "iq_f32" }],
  outputs: [
    { name: "a", portType: "iq_f32" },
    { name: "b", portType: "iq_f32" },
  ],
  params: [],
};
const IQ_TO_REAL: BlockSpec = {
  typeName: "IqToReal",
  placement: "either",
  inputs: [{ name: "in", portType: "iq_f32" }],
  outputs: [{ name: "out", portType: "real_f32" }],
  params: [],
};
const REAL_MIX: BlockSpec = {
  typeName: "RealMix",
  placement: "either",
  inputs: [
    { name: "a", portType: "real_f32" },
    { name: "b", portType: "real_f32" },
  ],
  outputs: [{ name: "out", portType: "real_f32" }],
  params: [],
};
const REAL_SINK: BlockSpec = {
  typeName: "RealSink",
  placement: "either",
  inputs: [{ name: "in", portType: "real_f32" }],
  outputs: [],
  params: [],
};

// ---------------------------------------------------------------------------

describe("topologicalOrder — linear chain", () => {
  it("src → demod → sink", () => {
    const doc: FlowgraphDoc = {
      name: "linear",
      environments: ["browser"],
      blocks: {
        demod: { type: "IqToReal" },
        sink: { type: "RealSink" },
        src: { type: "IqSrc" },
      },
      wires: [
        ["src.out", "demod.in"],
        ["demod.out", "sink.in"],
      ],
    };
    const g = instantiateFlowgraph(
      doc,
      registry(IQ_SRC, IQ_TO_REAL, REAL_SINK),
      "browser",
    );
    expect(topologicalOrder(g)).toEqual(["src", "demod", "sink"]);
  });
});

describe("topologicalOrder — diamond", () => {
  it("both branches appear before the sink", () => {
    const doc: FlowgraphDoc = {
      name: "diamond",
      environments: ["browser"],
      blocks: {
        src: { type: "IqSrc" },
        tee: { type: "IqTee" },
        left: { type: "IqToReal" },
        right: { type: "IqToReal" },
        mix: { type: "RealMix" },
      },
      wires: [
        ["src.out", "tee.in"],
        ["tee.a", "left.in"],
        ["tee.b", "right.in"],
        ["left.out", "mix.a"],
        ["right.out", "mix.b"],
      ],
    };
    const g = instantiateFlowgraph(
      doc,
      registry(IQ_SRC, IQ_TEE, IQ_TO_REAL, REAL_MIX),
      "browser",
    );
    const order = topologicalOrder(g);
    // src first, tee second; left/right in lex order (deterministic
    // tie-break); mix last.
    expect(order).toEqual(["src", "tee", "left", "right", "mix"]);
  });
});

describe("topologicalOrder — multiple independent sources", () => {
  it("breaks ties by id — producer emitted before a newly-ready peer", () => {
    // Priority-queue Kahn: once "a_src" emits and "a_demod" becomes
    // ready, the ready set is sorted ["a_demod", "b_src"] and a_demod
    // pops first. The invariant that matters is "every producer before
    // its consumer"; the interleaving is deterministic so tests can
    // pin it.
    const doc: FlowgraphDoc = {
      name: "parallel",
      environments: ["browser"],
      blocks: {
        b_src: { type: "IqSrc" },
        a_src: { type: "IqSrc" },
        a_demod: { type: "IqToReal" },
        b_demod: { type: "IqToReal" },
      },
      wires: [
        ["a_src.out", "a_demod.in"],
        ["b_src.out", "b_demod.in"],
      ],
    };
    const g = instantiateFlowgraph(
      doc,
      registry(IQ_SRC, IQ_TO_REAL),
      "browser",
    );
    expect(topologicalOrder(g)).toEqual([
      "a_src",
      "a_demod",
      "b_src",
      "b_demod",
    ]);
  });
});

describe("buildWirePlan", () => {
  it("reverse-indexes every input to its upstream producer", () => {
    const doc: FlowgraphDoc = {
      name: "diamond",
      environments: ["browser"],
      blocks: {
        src: { type: "IqSrc" },
        tee: { type: "IqTee" },
        left: { type: "IqToReal" },
        right: { type: "IqToReal" },
        mix: { type: "RealMix" },
      },
      wires: [
        ["src.out", "tee.in"],
        ["tee.a", "left.in"],
        ["tee.b", "right.in"],
        ["left.out", "mix.a"],
        ["right.out", "mix.b"],
      ],
    };
    const g = instantiateFlowgraph(
      doc,
      registry(IQ_SRC, IQ_TEE, IQ_TO_REAL, REAL_MIX),
      "browser",
    );
    const plan = buildWirePlan(g);

    expect(plan.get("src")?.size).toBe(0);
    expect(plan.get("tee")?.get("in")).toEqual({
      sourceBlock: "src",
      sourcePort: "out",
      portType: "iq_f32",
    });
    expect(plan.get("left")?.get("in")).toEqual({
      sourceBlock: "tee",
      sourcePort: "a",
      portType: "iq_f32",
    });
    expect(plan.get("right")?.get("in")).toEqual({
      sourceBlock: "tee",
      sourcePort: "b",
      portType: "iq_f32",
    });
    expect(plan.get("mix")?.get("a")).toEqual({
      sourceBlock: "left",
      sourcePort: "out",
      portType: "real_f32",
    });
    expect(plan.get("mix")?.get("b")).toEqual({
      sourceBlock: "right",
      sourcePort: "out",
      portType: "real_f32",
    });
  });
});

describe("Scheduler", () => {
  it("exposes order and wirePlan in a single object", () => {
    const doc: FlowgraphDoc = {
      name: "tiny",
      environments: ["browser"],
      blocks: {
        src: { type: "IqSrc" },
        demod: { type: "IqToReal" },
        sink: { type: "RealSink" },
      },
      wires: [
        ["src.out", "demod.in"],
        ["demod.out", "sink.in"],
      ],
    };
    const g = instantiateFlowgraph(
      doc,
      registry(IQ_SRC, IQ_TO_REAL, REAL_SINK),
      "browser",
    );
    const s = new Scheduler(g);
    expect(s.order).toEqual(["src", "demod", "sink"]);
    expect(s.wirePlan.get("demod")?.get("in")?.sourceBlock).toBe("src");
  });

  it("references the graph it was built from", () => {
    const doc: FlowgraphDoc = {
      name: "tiny",
      environments: ["browser"],
      blocks: { src: { type: "IqSrc" } },
      wires: [],
    };
    const g = instantiateFlowgraph(doc, registry(IQ_SRC), "browser");
    const s = new Scheduler(g);
    expect(s.graph).toBe(g);
  });
});
