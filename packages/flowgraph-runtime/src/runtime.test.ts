import { describe, expect, it, vi } from "vitest";

import {
  instantiateFlowgraph,
  type BlockRegistryLike,
  type RegisteredBlock,
} from "./instantiate.js";
import { Runtime } from "./runtime.js";
import type {
  BlockInstance,
  BlockIo,
  BlockSpec,
  FlowgraphDoc,
  InitCtx,
  Work,
} from "./types.js";

// ---------------------------------------------------------------------------
// Recording fixture — captures lifecycle calls in declaration order.
// ---------------------------------------------------------------------------

interface CallLog {
  readonly init: string[];
  readonly process: string[];
  readonly stop: string[];
  readonly update: Array<{ id: string; params: unknown }>;
  readonly initCtx: Map<string, InitCtx>;
}

function makeLog(): CallLog {
  return {
    init: [],
    process: [],
    stop: [],
    update: [],
    initCtx: new Map(),
  };
}

interface RecorderOptions {
  readonly failInit?: boolean;
  readonly failStop?: boolean;
  readonly noStop?: boolean;
  readonly noUpdate?: boolean;
}

function recorder(
  id: string,
  log: CallLog,
  opts: RecorderOptions = {},
): BlockInstance {
  const block: BlockInstance = {
    init(ctx: InitCtx) {
      log.init.push(id);
      log.initCtx.set(id, ctx);
      if (opts.failInit) throw new Error(`init failed: ${id}`);
    },
    process(_io: BlockIo): Work {
      log.process.push(id);
      return { consumed: [0], produced: [0] };
    },
  };
  if (!opts.noStop) {
    block.stop = () => {
      log.stop.push(id);
      if (opts.failStop) throw new Error(`stop failed: ${id}`);
    };
  }
  if (!opts.noUpdate) {
    block.update = (params: unknown) => {
      log.update.push({ id, params });
    };
  }
  return block;
}

function makeBlock(
  spec: BlockSpec,
  log: CallLog,
  perId: Record<string, RecorderOptions> = {},
): RegisteredBlock {
  return {
    spec,
    construct: () => recorder(spec.typeName, log, perId[spec.typeName] ?? {}),
  };
}

function registry(log: CallLog, ...specs: BlockSpec[]): BlockRegistryLike {
  const m = new Map<string, RegisteredBlock>();
  for (const s of specs) m.set(s.typeName, makeBlock(s, log));
  return {
    get: (n) => m.get(n),
    has: (n) => m.has(n),
  };
}

const SRC: BlockSpec = {
  typeName: "Src",
  placement: "either",
  inputs: [],
  outputs: [{ name: "out", portType: "iq_f32" }],
  params: [],
};
const MID: BlockSpec = {
  typeName: "Mid",
  placement: "either",
  inputs: [{ name: "in", portType: "iq_f32" }],
  outputs: [{ name: "out", portType: "real_f32" }],
  params: [],
};
const SINK: BlockSpec = {
  typeName: "Sink",
  placement: "either",
  inputs: [{ name: "in", portType: "real_f32" }],
  outputs: [],
  params: [],
};

function buildChain(
  log: CallLog,
  reg: BlockRegistryLike = registry(log, SRC, MID, SINK),
) {
  const doc: FlowgraphDoc = {
    name: "chain",
    environments: ["browser"],
    blocks: {
      src: { type: "Src" },
      mid: { type: "Mid" },
      sink: { type: "Sink" },
    },
    wires: [
      ["src.out", "mid.in"],
      ["mid.out", "sink.in"],
    ],
  };
  return instantiateFlowgraph(doc, reg, "browser");
}

// ---------------------------------------------------------------------------

describe("Runtime state machine", () => {
  it("starts in 'created' and does not tick anything", () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    expect(rt.state).toBe("created");
    expect(log.init).toEqual([]);
  });

  it("init → initialized → start → running → stop → stopped", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));

    await rt.init();
    expect(rt.state).toBe("initialized");

    await rt.start();
    expect(rt.state).toBe("running");

    await rt.stop();
    expect(rt.state).toBe("stopped");
  });

  it("rejects start() before init()", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    await expect(rt.start()).rejects.toThrow(/expected state "initialized"/);
  });

  it("rejects init() twice", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    await rt.init();
    await expect(rt.init()).rejects.toThrow(/expected state "created"/);
  });

  it("rejects stop() from 'created'", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    await expect(rt.stop()).rejects.toThrow(/cannot stop before init/);
  });

  it("is idempotent on repeated stop() after first stop", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    await rt.init();
    await rt.start();
    await rt.stop();
    await rt.stop(); // second call: no-op
    expect(log.stop).toEqual(["Sink", "Mid", "Src"]);
  });
});

describe("Runtime.init", () => {
  it("calls block.init in topological order", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    await rt.init();
    expect(log.init).toEqual(["Src", "Mid", "Sink"]);
  });

  it("hands each block an InitCtx with the configured frame hint", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log), { frameHint: 256 });
    await rt.init();
    for (const [_id, ctx] of log.initCtx) {
      expect(ctx.frameHint).toBe(256);
    }
  });

  it("defaults frameHint to 1024", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    await rt.init();
    expect(log.initCtx.get("Src")?.frameHint).toBe(1024);
  });

  it("InitCtx.inputRate/outputRate return undefined (no rate negotiation yet)", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    await rt.init();
    const ctx = log.initCtx.get("Mid")!;
    expect(ctx.inputRate("in")).toBeUndefined();
    expect(ctx.outputRate("out")).toBeUndefined();
  });

  it("awaits async init implementations", async () => {
    const log = makeLog();
    const asyncSpec: BlockSpec = { ...SRC, typeName: "AsyncSrc" };
    const slowInit = vi.fn(async () => {
      await new Promise((r) => setTimeout(r, 1));
      log.init.push("AsyncSrc");
    });
    const reg: BlockRegistryLike = {
      get: (n) =>
        n === "AsyncSrc"
          ? {
              spec: asyncSpec,
              construct: () =>
                ({
                  init: slowInit,
                  process: () => ({ consumed: [], produced: [0] }),
                }) as BlockInstance,
            }
          : undefined,
      has: (n) => n === "AsyncSrc",
    };
    const doc: FlowgraphDoc = {
      name: "one",
      environments: ["browser"],
      blocks: { src: { type: "AsyncSrc" } },
      wires: [],
    };
    const g = instantiateFlowgraph(doc, reg, "browser");
    await new Runtime(g).init();
    expect(slowInit).toHaveBeenCalled();
    expect(log.init).toEqual(["AsyncSrc"]);
  });

  it("leaves state in 'created' when a block's init throws", async () => {
    const log = makeLog();
    const doc: FlowgraphDoc = {
      name: "broken",
      environments: ["browser"],
      blocks: { src: { type: "Src" }, mid: { type: "Mid" } },
      wires: [["src.out", "mid.in"]],
    };
    const reg: BlockRegistryLike = {
      get: (n) =>
        ({
          Src: makeBlock(SRC, log, { Src: { failInit: true } }),
          Mid: makeBlock(MID, log),
        })[n],
      has: (n) => n === "Src" || n === "Mid",
    };
    const g = instantiateFlowgraph(doc, reg, "browser");
    const rt = new Runtime(g);
    await expect(rt.init()).rejects.toThrow(/init failed: Src/);
    expect(rt.state).toBe("created");
  });
});

describe("Runtime.stop", () => {
  it("calls block.stop in reverse topological order", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    await rt.init();
    await rt.start();
    await rt.stop();
    expect(log.stop).toEqual(["Sink", "Mid", "Src"]);
  });

  it("skips blocks that did not implement stop()", async () => {
    const log = makeLog();
    const reg: BlockRegistryLike = {
      get: (n) =>
        ({
          Src: {
            spec: SRC,
            construct: () => recorder("Src", log, { noStop: true }),
          },
          Mid: makeBlock(MID, log),
          Sink: makeBlock(SINK, log),
        })[n],
      has: (n) => ["Src", "Mid", "Sink"].includes(n),
    };
    const g = buildChain(log, reg);
    const rt = new Runtime(g);
    await rt.init();
    await rt.start();
    await rt.stop();
    expect(log.stop).toEqual(["Sink", "Mid"]);
  });

  it("keeps going when one block's stop throws, then aggregates failures", async () => {
    const log = makeLog();
    const reg: BlockRegistryLike = {
      get: (n) =>
        ({
          Src: makeBlock(SRC, log),
          Mid: {
            spec: MID,
            construct: () => recorder("Mid", log, { failStop: true }),
          },
          Sink: makeBlock(SINK, log),
        })[n],
      has: (n) => ["Src", "Mid", "Sink"].includes(n),
    };
    const g = buildChain(log, reg);
    const rt = new Runtime(g);
    await rt.init();
    await rt.start();
    await expect(rt.stop()).rejects.toBeInstanceOf(AggregateError);
    // All three got a chance; Mid threw but Src/Sink still released.
    expect(log.stop).toEqual(["Sink", "Mid", "Src"]);
    expect(rt.state).toBe("stopped");
  });
});

describe("Runtime.update", () => {
  it("forwards params to the named block's update()", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    await rt.init();
    rt.update("mid", { deviation: 75_000 });
    expect(log.update).toEqual([{ id: "Mid", params: { deviation: 75_000 } }]);
  });

  it("is a no-op when the block doesn't implement update()", async () => {
    const log = makeLog();
    const reg: BlockRegistryLike = {
      get: (n) =>
        ({
          Src: makeBlock(SRC, log),
          Mid: {
            spec: MID,
            construct: () => recorder("Mid", log, { noUpdate: true }),
          },
          Sink: makeBlock(SINK, log),
        })[n],
      has: (n) => ["Src", "Mid", "Sink"].includes(n),
    };
    const g = buildChain(log, reg);
    const rt = new Runtime(g);
    await rt.init();
    expect(() => rt.update("mid", { x: 1 })).not.toThrow();
    expect(log.update).toEqual([]);
  });

  it("throws on unknown block id", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    await rt.init();
    expect(() => rt.update("ghost", {})).toThrow(/unknown block "ghost"/);
  });

  it("rejects update() before init()", () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    expect(() => rt.update("mid", {})).toThrow(
      /not allowed from state "created"/,
    );
  });

  it("rejects update() after stop()", async () => {
    const log = makeLog();
    const rt = new Runtime(buildChain(log));
    await rt.init();
    await rt.start();
    await rt.stop();
    expect(() => rt.update("mid", {})).toThrow(
      /not allowed from state "stopped"/,
    );
  });
});
