import { describe, expect, it } from "vitest";

import {
  instantiateFlowgraph,
  type BlockRegistryLike,
  type RegisteredBlock,
} from "./instantiate.js";
import type {
  BlockInstance,
  BlockIo,
  BlockSpec,
  FlowgraphDoc,
  Work,
} from "./types.js";
import { FlowgraphValidationError } from "./validate.js";

// ---------------------------------------------------------------------------
// Test fixtures — minimum BlockInstance that counts construct calls.
// ---------------------------------------------------------------------------

class Counter implements BlockInstance {
  static calls: Array<{ type: string; params: unknown }> = [];

  init(): void {}
  process(_io: BlockIo): Work {
    return { consumed: [0], produced: [0] };
  }
}

function makeBlock(spec: BlockSpec, type = spec.typeName): RegisteredBlock {
  return {
    spec,
    construct: (params) => {
      Counter.calls.push({ type, params });
      return new Counter();
    },
  };
}

const IQ_SOURCE: BlockSpec = {
  typeName: "IqSource",
  placement: "either",
  inputs: [],
  outputs: [{ name: "out", portType: "iq_f32" }],
  params: [],
};

const FM_DEMOD: BlockSpec = {
  typeName: "FmDemod",
  placement: "either",
  inputs: [{ name: "in", portType: "iq_f32" }],
  outputs: [{ name: "out", portType: "real_f32" }],
  params: [],
};

const AUDIO_SINK: BlockSpec = {
  typeName: "AudioSink",
  placement: "wasm_only",
  inputs: [{ name: "in", portType: "real_f32" }],
  outputs: [],
  params: [],
};

const FS_FILE_SINK: BlockSpec = {
  typeName: "FsFileSink",
  placement: "native_only",
  inputs: [{ name: "in", portType: "real_f32" }],
  outputs: [],
  params: [],
};

function registry(...specs: BlockSpec[]): BlockRegistryLike {
  const m = new Map<string, RegisteredBlock>();
  for (const s of specs) m.set(s.typeName, makeBlock(s));
  return {
    get: (n) => m.get(n),
    has: (n) => m.has(n),
  };
}

const CHAIN_DOC: FlowgraphDoc = {
  name: "chain",
  environments: ["browser"],
  blocks: {
    src: { type: "IqSource" },
    demod: { type: "FmDemod" },
    audio: { type: "AudioSink" },
  },
  wires: [
    ["src.out", "demod.in"],
    ["demod.out", "audio.in"],
  ],
};

// ---------------------------------------------------------------------------

describe("instantiateFlowgraph — happy path", () => {
  it("constructs each block and returns a populated map", () => {
    Counter.calls = [];
    const reg = registry(IQ_SOURCE, FM_DEMOD, AUDIO_SINK);
    const result = instantiateFlowgraph(CHAIN_DOC, reg, "browser");
    expect(result.instances.size).toBe(3);
    expect(result.instances.get("demod")).toBeInstanceOf(Counter);
    expect(result.specs.get("demod")?.typeName).toBe("FmDemod");
    expect(Counter.calls.map((c) => c.type).sort()).toEqual([
      "AudioSink",
      "FmDemod",
      "IqSource",
    ]);
  });

  it("passes params through to construct verbatim", () => {
    Counter.calls = [];
    const doc: FlowgraphDoc = {
      ...CHAIN_DOC,
      blocks: {
        ...CHAIN_DOC.blocks,
        demod: { type: "FmDemod", params: { deviation: 75_000 } },
      },
    };
    const reg = registry(IQ_SOURCE, FM_DEMOD, AUDIO_SINK);
    instantiateFlowgraph(doc, reg, "browser");
    const demodCall = Counter.calls.find((c) => c.type === "FmDemod");
    expect(demodCall?.params).toEqual({ deviation: 75_000 });
  });

  it("defaults missing params to an empty object", () => {
    Counter.calls = [];
    const reg = registry(IQ_SOURCE, FM_DEMOD, AUDIO_SINK);
    instantiateFlowgraph(CHAIN_DOC, reg, "browser");
    const srcCall = Counter.calls.find((c) => c.type === "IqSource");
    expect(srcCall?.params).toEqual({});
  });
});

describe("instantiateFlowgraph — env_match", () => {
  it("rejects unknown block types", () => {
    const doc: FlowgraphDoc = {
      ...CHAIN_DOC,
      blocks: {
        ...CHAIN_DOC.blocks,
        demod: { type: "NoSuchBlock" },
      },
    };
    const reg = registry(IQ_SOURCE, AUDIO_SINK);
    try {
      instantiateFlowgraph(doc, reg, "browser");
      expect.fail("should have thrown");
    } catch (e) {
      const err = e as FlowgraphValidationError;
      expect(err.errors.some((v) => v.phase === "env_match")).toBe(true);
    }
  });

  it("rejects wasm_only blocks in a node environment", () => {
    const reg = registry(IQ_SOURCE, FM_DEMOD, AUDIO_SINK);
    const doc: FlowgraphDoc = {
      ...CHAIN_DOC,
      environments: ["node"],
    };
    expect(() => instantiateFlowgraph(doc, reg, "node")).toThrow(/wasm_only/);
  });

  it("rejects native_only blocks in a browser environment", () => {
    const reg = registry(IQ_SOURCE, FM_DEMOD, FS_FILE_SINK);
    const doc: FlowgraphDoc = {
      name: "headless",
      environments: ["browser"],
      blocks: {
        src: { type: "IqSource" },
        demod: { type: "FmDemod" },
        sink: { type: "FsFileSink" },
      },
      wires: [
        ["src.out", "demod.in"],
        ["demod.out", "sink.in"],
      ],
    };
    expect(() => instantiateFlowgraph(doc, reg, "browser")).toThrow(
      /native_only/,
    );
  });

  it("rejects environments the flowgraph does not declare", () => {
    const reg = registry(IQ_SOURCE, FM_DEMOD, AUDIO_SINK);
    expect(() => instantiateFlowgraph(CHAIN_DOC, reg, "node")).toThrow(
      /does not declare environment/,
    );
  });
});

describe("instantiateFlowgraph — wire_endpoints", () => {
  it("rejects wires to blocks that don't exist", () => {
    const doc: FlowgraphDoc = {
      ...CHAIN_DOC,
      wires: [
        ["src.out", "demod.in"],
        ["demod.out", "ghost.in"],
      ],
    };
    const reg = registry(IQ_SOURCE, FM_DEMOD, AUDIO_SINK);
    expect(() => instantiateFlowgraph(doc, reg, "browser")).toThrow(
      /unknown block "ghost"/,
    );
  });

  it("rejects wires to ports that don't exist on the block", () => {
    const doc: FlowgraphDoc = {
      ...CHAIN_DOC,
      wires: [
        ["src.out", "demod.nope"],
        ["demod.out", "audio.in"],
      ],
    };
    const reg = registry(IQ_SOURCE, FM_DEMOD, AUDIO_SINK);
    expect(() => instantiateFlowgraph(doc, reg, "browser")).toThrow(
      /has no input port/,
    );
  });
});

describe("instantiateFlowgraph — wire_type_match", () => {
  it("rejects type-mismatched wires", () => {
    // Wire IqSource.out (iq_f32) straight into AudioSink.in (real_f32).
    const doc: FlowgraphDoc = {
      name: "mismatch",
      environments: ["browser"],
      blocks: {
        src: { type: "IqSource" },
        audio: { type: "AudioSink" },
      },
      wires: [["src.out", "audio.in"]],
    };
    const reg = registry(IQ_SOURCE, AUDIO_SINK);
    try {
      instantiateFlowgraph(doc, reg, "browser");
      expect.fail("should have thrown");
    } catch (e) {
      const err = e as FlowgraphValidationError;
      expect(err.errors.some((v) => v.phase === "wire_type_match")).toBe(true);
    }
  });
});

describe("instantiateFlowgraph — failure atomicity", () => {
  it("constructs no blocks when validation fails", () => {
    Counter.calls = [];
    const doc: FlowgraphDoc = {
      ...CHAIN_DOC,
      blocks: {
        ...CHAIN_DOC.blocks,
        audio: { type: "Bogus" },
      },
    };
    const reg = registry(IQ_SOURCE, FM_DEMOD);
    expect(() => instantiateFlowgraph(doc, reg, "browser")).toThrow();
    expect(Counter.calls).toEqual([]);
  });
});
