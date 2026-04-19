import { describe, expect, it } from "vitest";

import {
  FlowgraphValidationError,
  parseFlowgraph,
  validateFlowgraph,
} from "./validate.js";

const WBFM_DOC = {
  name: "wbfm",
  label: "WBFM broadcast",
  environments: ["browser"],
  blocks: {
    src: { type: "WsIqSource", params: { stream: "vfo.primary" } },
    demod: { type: "FmDemod", params: {} },
    decim: { type: "Decimator", params: { out_rate: 48000 } },
    audio: { type: "AudioSink", params: { rate: 48000 } },
  },
  wires: [
    ["src.out", "demod.in"],
    ["demod.out", "decim.in"],
    ["decim.out", "audio.in"],
  ],
};

describe("parseFlowgraph", () => {
  it("accepts a well-formed three-block chain", () => {
    const { doc, warnings } = parseFlowgraph(JSON.stringify(WBFM_DOC));
    expect(doc.name).toBe("wbfm");
    expect(doc.blocks).toHaveProperty("demod");
    expect(warnings).toEqual([]);
  });

  it("rejects malformed JSON with a shape error", () => {
    try {
      parseFlowgraph("{not valid json");
      expect.fail("should have thrown");
    } catch (e) {
      expect(e).toBeInstanceOf(FlowgraphValidationError);
      const err = e as FlowgraphValidationError;
      expect(err.errors[0]?.phase).toBe("shape");
    }
  });
});

describe("validateFlowgraph — shape", () => {
  it("rejects missing name", () => {
    const bad = { ...WBFM_DOC, name: "" };
    expect(() => validateFlowgraph(bad)).toThrow(/name/);
  });

  it("rejects empty environments", () => {
    const bad = { ...WBFM_DOC, environments: [] };
    expect(() => validateFlowgraph(bad)).toThrow(/environments/);
  });

  it("rejects unknown environment values", () => {
    const bad = { ...WBFM_DOC, environments: ["mars"] };
    expect(() => validateFlowgraph(bad)).toThrow(/unknown environment/);
  });

  it("rejects wires that aren't [source, sink] pairs", () => {
    const bad = { ...WBFM_DOC, wires: [["src.out"]] };
    expect(() => validateFlowgraph(bad)).toThrow(/source, sink/);
  });

  it("rejects wire endpoints missing the dot", () => {
    const bad = {
      ...WBFM_DOC,
      wires: [["src_out", "demod.in"]],
    };
    expect(() => validateFlowgraph(bad)).toThrow(/instance\.port/);
  });

  it("rejects blocks that aren't objects", () => {
    const bad = { ...WBFM_DOC, blocks: { src: "not-an-object" } };
    expect(() => validateFlowgraph(bad)).toThrow(/must be an object/);
  });

  it("rejects block declarations with no type", () => {
    const bad = { ...WBFM_DOC, blocks: { src: { params: {} } } };
    expect(() => validateFlowgraph(bad)).toThrow(/`type`/);
  });
});

describe("validateFlowgraph — fan", () => {
  it("rejects two wires leaving the same output port", () => {
    const bad = {
      ...WBFM_DOC,
      blocks: {
        ...WBFM_DOC.blocks,
        extra: { type: "AudioSink", params: {} },
      },
      wires: [
        ["src.out", "demod.in"],
        ["src.out", "extra.in"],
        ["demod.out", "decim.in"],
        ["decim.out", "audio.in"],
      ],
    };
    try {
      validateFlowgraph(bad);
      expect.fail("should have thrown");
    } catch (e) {
      const err = e as FlowgraphValidationError;
      expect(err.errors.some((v) => v.phase === "fan")).toBe(true);
    }
  });

  it("rejects two wires arriving at the same input port", () => {
    const bad = {
      ...WBFM_DOC,
      blocks: {
        ...WBFM_DOC.blocks,
        extra: { type: "WsIqSource", params: {} },
      },
      wires: [
        ["src.out", "demod.in"],
        ["extra.out", "demod.in"],
        ["demod.out", "decim.in"],
        ["decim.out", "audio.in"],
      ],
    };
    expect(() => validateFlowgraph(bad)).toThrow(/wired more than once/);
  });
});

describe("validateFlowgraph — dag", () => {
  it("rejects a two-block cycle", () => {
    const bad = {
      name: "loop",
      environments: ["browser"],
      blocks: {
        a: { type: "FmDemod" },
        b: { type: "FmDemod" },
      },
      wires: [
        ["a.out", "b.in"],
        ["b.out", "a.in"],
      ],
    };
    try {
      validateFlowgraph(bad);
      expect.fail("should have thrown");
    } catch (e) {
      const err = e as FlowgraphValidationError;
      expect(err.errors.some((v) => v.phase === "dag")).toBe(true);
    }
  });

  it("rejects a three-block cycle", () => {
    const bad = {
      name: "loop3",
      environments: ["browser"],
      blocks: {
        a: { type: "FmDemod" },
        b: { type: "FmDemod" },
        c: { type: "FmDemod" },
      },
      wires: [
        ["a.out", "b.in"],
        ["b.out", "c.in"],
        ["c.out", "a.in"],
      ],
    };
    expect(() => validateFlowgraph(bad)).toThrow(/cycle/);
  });

  it("accepts a diamond (no cycle)", () => {
    const ok = {
      name: "diamond",
      environments: ["browser"],
      blocks: {
        src: { type: "WsIqSource" },
        tee: { type: "Tee" },
        left: { type: "FmDemod" },
        right: { type: "FmDemod" },
        mix: { type: "Mixer" },
      },
      wires: [
        ["src.out", "tee.in"],
        ["tee.a", "left.in"],
        ["tee.b", "right.in"],
        ["left.out", "mix.a"],
        ["right.out", "mix.b"],
      ],
    };
    const { warnings } = validateFlowgraph(ok);
    expect(warnings).toEqual([]);
  });
});

describe("validateFlowgraph — connectivity", () => {
  it("warns on isolated blocks but does not throw", () => {
    const doc = {
      ...WBFM_DOC,
      blocks: {
        ...WBFM_DOC.blocks,
        orphan: { type: "AudioSink" },
      },
    };
    const { warnings } = validateFlowgraph(doc);
    expect(warnings).toHaveLength(1);
    expect(warnings[0]?.phase).toBe("connectivity");
    expect(warnings[0]?.block).toBe("orphan");
  });

  it("returns no warnings when every block has a wire", () => {
    const { warnings } = validateFlowgraph(WBFM_DOC);
    expect(warnings).toEqual([]);
  });
});
