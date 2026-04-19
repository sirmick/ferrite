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
// What this commit *does* do:
//   • call `init()` on every block in topological order, handing each
//     one an `InitCtx` synthesised from the wire plan
//   • start → running transition (a flag flip today; the tick pump
//     lands in a later commit with buffer allocation and `process()`
//     dispatch)
//   • stop() in reverse topological order so consumers drain before
//     producers disappear
//   • runtime.update(blockId, params) — forwards to a block's optional
//     `update()` hook for mutable-while-streaming params
//
// What this commit does *not* do:
//   • drive `process()` — no tick pump yet
//   • allocate ring buffers between blocks — same
//   • rate negotiation — `InitCtx` returns undefined for unknown rates
//     and a fixed default frame hint; the negotiation phase listed in
//     the validator will plug in here without changing the external
//     shape

import type { InstantiatedGraph } from "./instantiate.js";
import { Scheduler } from "./schedule.js";
import type { InitCtx } from "./types.js";

export type RuntimeState = "created" | "initialized" | "running" | "stopped";

export interface RuntimeOptions {
  /**
   * Default per-call frame budget handed to `BlockInstance.init` via
   * `InitCtx.frameHint`. Blocks free to honour or ignore. 1024 matches
   * the browser `AudioWorklet` batch (`128 * 8`) and is a reasonable
   * default for native too.
   */
  readonly frameHint?: number;
}

const DEFAULT_FRAME_HINT = 1024;

/**
 * Wraps an `InstantiatedGraph` with a typed lifecycle. Construct once,
 * `init()` once, `start()`/`stop()` once each. Not reusable after
 * `stop()` — build a fresh `Runtime` to restart.
 */
export class Runtime {
  readonly scheduler: Scheduler;
  readonly graph: InstantiatedGraph;
  private readonly frameHint: number;
  private _state: RuntimeState = "created";

  constructor(graph: InstantiatedGraph, options: RuntimeOptions = {}) {
    this.graph = graph;
    this.scheduler = new Scheduler(graph);
    this.frameHint = options.frameHint ?? DEFAULT_FRAME_HINT;
  }

  get state(): RuntimeState {
    return this._state;
  }

  /**
   * Call each block's `init` in topological order. Awaits every
   * returned promise; failures propagate and leave the runtime in
   * `created` so the caller can retry or dispose.
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
    this._state = "initialized";
  }

  /**
   * Enter `running`. Today this is just a flag flip — the tick pump
   * lives in a later commit. Callers should `await init()` first.
   */
  async start(): Promise<void> {
    if (this._state !== "initialized") {
      throw new Error(
        `Runtime.start: expected state "initialized", got "${this._state}"`,
      );
    }
    this._state = "running";
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
}
