// Runtime lifecycle — the final wire-up layer on top of the scheduler.
//
// A `Runtime` drives an `InstantiatedGraph` through a small state
// machine:
//
//   created ── init() ─▶ initialized ── start() ─▶ running ── stop() ─▶ stopped
//                                                  ╰─── stop() ────────▶ stopped
//
// The states exist so callers can wire up instrumentation, UI
// indicators, and error recovery without reading into ad-hoc booleans.
// Transitions are explicit and one-way; a `Runtime` is not reusable
// after `stop()`.
//
// Tick pump: `init()` allocates one Float32Array per output port sized
// from `frameHint` and the port type (2 floats per iq_f32 sample, 1
// per real_f32 / fft_f32 sample). `start()` begins driving `tick()`
// on a `setInterval`. Each tick walks blocks in topological order,
// feeds each consumer a **view of its upstream's output buffer trimmed
// to the producer's `Work.produced[i]`**, collects the block's own
// work, and records new produced counts for downstream consumers.
//
// Backpressure is coarse today — every block sees its full-capacity
// output buffer. Blocks that can't consume everything (Decimator under
// output backpressure) leave the rest in their own state; the scheduler
// does not re-present unconsumed inputs. This is fine for the sample
// flow currently in the repo (WsIqSource → FmDemod → AudioSink) where
// every block consumes what it's given. A proper credit-based scheme
// lands when a block needs it.

import type { InstantiatedGraph } from "./instantiate.js";
import { Scheduler } from "./schedule.js";
import type {
  BlockIo,
  BlockSpec,
  InitCtx,
  PortBuf,
  PortType,
} from "./types.js";

export type RuntimeState = "created" | "initialized" | "running" | "stopped";

export interface RuntimeOptions {
  /**
   * Default per-call frame budget handed to `BlockInstance.init` via
   * `InitCtx.frameHint`. Output buffers are sized from this too: an
   * iq_f32 port gets `frameHint · 2` floats, a real_f32 port gets
   * `frameHint` floats. 1024 matches the browser `AudioWorklet` batch
   * (`128 · 8`) and is a reasonable default for native too.
   */
  readonly frameHint?: number;
  /**
   * How often `tick()` runs once the runtime is `running`. Default
   * 10 ms, i.e. ~100 ticks/sec — fast enough to keep a 100 kS/s IQ
   * stream draining ahead of the audio clock, slow enough that the
   * event loop stays responsive. Tests and specialised callers can
   * pass 0 and drive `tick()` manually.
   */
  readonly tickIntervalMs?: number;
}

const DEFAULT_FRAME_HINT = 1024;
const DEFAULT_TICK_INTERVAL_MS = 10;

/** Minimal `setInterval`/`clearInterval` surface — typed once so the
 *  runtime works in Node and browser without DOM lib dependencies. */
type IntervalHandle = ReturnType<typeof setInterval>;

/**
 * Wraps an `InstantiatedGraph` with a typed lifecycle. Construct once,
 * `init()` once, `start()`/`stop()` once each. Not reusable after
 * `stop()` — build a fresh `Runtime` to restart.
 */
export class Runtime {
  readonly scheduler: Scheduler;
  readonly graph: InstantiatedGraph;
  private readonly frameHint: number;
  private readonly tickIntervalMs: number;
  private _state: RuntimeState = "created";
  /** Per block, per output port → backing Float32Array. Allocated in init(). */
  private outputBuffers = new Map<string, Map<string, Float32Array>>();
  /** Per block, per output port → count of elements produced last tick. */
  private lastProduced = new Map<string, Map<string, number>>();
  private intervalHandle: IntervalHandle | undefined;

  constructor(graph: InstantiatedGraph, options: RuntimeOptions = {}) {
    this.graph = graph;
    this.scheduler = new Scheduler(graph);
    this.frameHint = options.frameHint ?? DEFAULT_FRAME_HINT;
    this.tickIntervalMs = options.tickIntervalMs ?? DEFAULT_TICK_INTERVAL_MS;
  }

  get state(): RuntimeState {
    return this._state;
  }

  /**
   * Call each block's `init` in topological order. Awaits every
   * returned promise; failures propagate and leave the runtime in
   * `created` so the caller can retry or dispose. Allocates the
   * per-port output buffers the tick pump hands to `process()`.
   */
  async init(): Promise<void> {
    if (this._state !== "created") {
      throw new Error(
        `Runtime.init: expected state "created", got "${this._state}"`,
      );
    }
    for (const id of this.scheduler.order) {
      const block = this.graph.instances.get(id);
      if (!block) {
        // Should be unreachable — scheduler.order comes from the same
        // graph. Surface loud so a future refactor that breaks this
        // invariant fails immediately.
        throw new Error(
          `Runtime.init: scheduler references unknown block ${JSON.stringify(id)}`,
        );
      }
      const ctx = this.buildInitCtx(id);
      await block.init(ctx);
    }
    this.allocateBuffers();
    this._state = "initialized";
  }

  /**
   * Enter `running` and begin driving `tick()` on the configured
   * interval. When `tickIntervalMs` is 0 the interval is not scheduled
   * — the caller is expected to drive `tick()` manually (tests,
   * headless harnesses that want lock-step control).
   */
  async start(): Promise<void> {
    if (this._state !== "initialized") {
      throw new Error(
        `Runtime.start: expected state "initialized", got "${this._state}"`,
      );
    }
    this._state = "running";
    if (this.tickIntervalMs > 0) {
      this.intervalHandle = setInterval(() => {
        try {
          this.tick();
        } catch {
          // Tick errors are surfaced to the caller via manual `tick()`;
          // on the interval we swallow so a transient failure doesn't
          // unregister the pump. A proper supervisor lands with the
          // logging slice.
        }
      }, this.tickIntervalMs);
    }
  }

  /**
   * Run one pass of the scheduler: walk blocks in topological order,
   * call `process()` on each with inputs wired from upstream outputs.
   * Safe to call from `running` or from `initialized` (useful in tests
   * that want deterministic ticks without an interval).
   *
   * Throws if any block's `process()` throws. Callers driving ticks by
   * hand see the error directly; the interval-driven pump swallows.
   */
  tick(): void {
    if (this._state !== "running" && this._state !== "initialized") {
      throw new Error(`Runtime.tick: not allowed from state "${this._state}"`);
    }
    for (const id of this.scheduler.order) {
      const block = this.graph.instances.get(id);
      const spec = this.graph.specs.get(id);
      if (!block || !spec) continue;
      const io = this.buildBlockIo(id, spec);
      const work = block.process(io);
      const producedByPort = new Map<string, number>();
      for (let i = 0; i < spec.outputs.length; i++) {
        const portName = spec.outputs[i]!.name;
        producedByPort.set(portName, work.produced[i] ?? 0);
      }
      this.lastProduced.set(id, producedByPort);
    }
  }

  /**
   * Release every block. Runs in reverse topological order so
   * consumers stop before the producers they depend on. Idempotent
   * from the `stopped` state; throws if called before `init()` so
   * callers don't mask programming errors.
   *
   * Individual `block.stop()` failures are collected and re-thrown as
   * an `AggregateError` after *every* block has been given a chance
   * to release — one broken block should not leak the others.
   */
  async stop(): Promise<void> {
    if (this._state === "stopped") return;
    if (this._state === "created") {
      throw new Error(
        `Runtime.stop: cannot stop before init(); state is "created"`,
      );
    }
    if (this.intervalHandle !== undefined) {
      clearInterval(this.intervalHandle);
      this.intervalHandle = undefined;
    }
    const failures: unknown[] = [];
    for (let i = this.scheduler.order.length - 1; i >= 0; i--) {
      const id = this.scheduler.order[i]!;
      const block = this.graph.instances.get(id);
      if (!block?.stop) continue;
      try {
        await block.stop();
      } catch (err) {
        failures.push(err);
      }
    }
    this._state = "stopped";
    if (failures.length > 0) {
      throw new AggregateError(
        failures,
        `Runtime.stop: ${failures.length} block(s) failed to release cleanly`,
      );
    }
  }

  /**
   * Forward a param update to one block's optional `update()` hook.
   * The block decides which fields it accepts — only params flagged
   * `mutableWhileStreaming` in its spec are expected to arrive here;
   * anything else requires a flowgraph rebuild.
   *
   * Silently ignored for blocks that do not implement `update`;
   * throws if the block id is unknown or if the runtime has been
   * stopped. Valid from `initialized` onward so callers can pre-load
   * params before the first tick.
   */
  update(blockId: string, params: unknown): void {
    if (this._state === "created" || this._state === "stopped") {
      throw new Error(
        `Runtime.update: not allowed from state "${this._state}"`,
      );
    }
    const block = this.graph.instances.get(blockId);
    if (!block) {
      throw new Error(
        `Runtime.update: unknown block ${JSON.stringify(blockId)}`,
      );
    }
    block.update?.(params);
  }

  /**
   * Synthesise the per-block `InitCtx`. Rate negotiation has not
   * landed yet, so `inputRate`/`outputRate` return undefined; the
   * frame hint comes from `RuntimeOptions` (default 1024). Blocks
   * that need rates must either accept `undefined` and fall back to
   * params, or wait for the rate-negotiation slice.
   */
  private buildInitCtx(_blockId: string): InitCtx {
    return {
      frameHint: this.frameHint,
      inputRate: () => undefined,
      outputRate: () => undefined,
    };
  }

  /**
   * Allocate one Float32Array per output port. Size is `frameHint ·
   * floatsPerSample(portType)` so a block never sees a buffer smaller
   * than what it asked the runtime to hint at.
   */
  private allocateBuffers(): void {
    this.outputBuffers.clear();
    this.lastProduced.clear();
    for (const [id, spec] of this.graph.specs) {
      const perPort = new Map<string, Float32Array>();
      const producedInit = new Map<string, number>();
      for (const out of spec.outputs) {
        const floats = this.frameHint * floatsPerSample(out.portType);
        perPort.set(out.name, new Float32Array(floats));
        producedInit.set(out.name, 0);
      }
      this.outputBuffers.set(id, perPort);
      this.lastProduced.set(id, producedInit);
    }
  }

  /** Build the per-tick `BlockIo` for one block: inputs view upstream
   *  producer buffers, outputs point at our own pool. */
  private buildBlockIo(id: string, spec: BlockSpec): BlockIo {
    const wirePlanForBlock = this.scheduler.wirePlan.get(id);
    const inputs: PortBuf[] = spec.inputs.map((ip) => {
      const src = wirePlanForBlock?.get(ip.name);
      if (!src) {
        return {
          name: ip.name,
          portType: ip.portType,
          meta: { sampleRateHz: 0, centerFreqHz: 0 },
          data: new Float32Array(0),
        };
      }
      const upstream = this.outputBuffers
        .get(src.sourceBlock)
        ?.get(src.sourcePort);
      const produced =
        this.lastProduced.get(src.sourceBlock)?.get(src.sourcePort) ?? 0;
      const view =
        upstream && produced > 0
          ? upstream.subarray(0, produced)
          : new Float32Array(0);
      return {
        name: ip.name,
        portType: ip.portType,
        meta: { sampleRateHz: 0, centerFreqHz: 0 },
        data: view,
      };
    });
    const outputs: PortBuf[] = spec.outputs.map((op) => {
      const buf =
        this.outputBuffers.get(id)?.get(op.name) ?? new Float32Array(0);
      return {
        name: op.name,
        portType: op.portType,
        meta: { sampleRateHz: 0, centerFreqHz: 0 },
        data: buf,
      };
    });
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
}

/**
 * Float-count per logical sample for each port type. The scheduler
 * allocates output buffers as `frameHint · this`, and Work counts are
 * interpreted as float elements throughout the block API.
 *
 * Non-f32 types (bits/events/frames) get a conservative 1 element per
 * "sample" — blocks consuming those types must not be combined with
 * the Float32Array-backed pool until a typed buffer variant lands.
 */
function floatsPerSample(portType: PortType): number {
  switch (portType) {
    case "iq_f32":
      return 2;
    case "real_f32":
    case "fft_f32":
    case "fft_u8":
      return 1;
    case "iq_s16":
      return 2;
    case "real_i16":
    case "bits":
    case "frames":
    case "events":
      return 1;
  }
}
