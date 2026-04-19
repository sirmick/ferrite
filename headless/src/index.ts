// @ferrite/headless — Node-side flowgraph runner.
//
// Drives an `InstantiatedGraph` through its lifecycle without the
// browser-only plumbing (AudioSink, WsIqSource, SharedArrayBuffer).
// The tick pump + real source blocks land in later commits; this
// skeleton just wraps `Runtime.init → start → stop` so callers can
// load a flowgraph and run it in a test harness or CLI.
//
// Sinks deliberately absent — "no sinks yet" is a property of the
// skeleton, not a missing feature. A `FileSink` / `CallbackSink`
// will live in a sibling module once the scheduler tick pump is
// wired through.

import {
  FlowgraphValidationError,
  instantiateFlowgraph,
  parseFlowgraph,
  Runtime,
  type BlockRegistryLike,
  type FlowgraphDoc,
  type InstantiatedGraph,
} from "@ferrite/flowgraph-runtime";

export const HEADLESS_VERSION = "0.0.1";

export interface HeadlessRunOptions {
  readonly registry: BlockRegistryLike;
  /**
   * If provided, called once after `init()` and before `start()` —
   * useful for seeding buffers or attaching diagnostics to freshly
   * constructed blocks. Errors propagate and abort the lifecycle.
   */
  readonly afterInit?: (graph: InstantiatedGraph) => void | Promise<void>;
}

/**
 * Load a flowgraph from a JSON string, instantiate it for the `"node"`
 * environment, and drive it through `init → start → stop`. Returns the
 * instantiated graph so callers can inspect the constructed blocks.
 *
 * This is the minimum useful headless entry point. Once the scheduler
 * exposes a tick pump, `runFlowgraphHeadless` will accept a duration or
 * tick count and actually advance the graph.
 */
export async function runFlowgraphHeadless(
  json: string,
  options: HeadlessRunOptions,
): Promise<InstantiatedGraph> {
  const { doc } = parseFlowgraph(json);
  return runInstantiatedHeadless(doc, options);
}

/** Variant that skips the parse step — useful when the caller already
 *  has a validated doc (e.g. imported JSON). */
export async function runInstantiatedHeadless(
  doc: FlowgraphDoc,
  options: HeadlessRunOptions,
): Promise<InstantiatedGraph> {
  if (!doc.environments.includes("node")) {
    throw new FlowgraphValidationError([
      {
        phase: "env_match",
        error: `flowgraph ${JSON.stringify(doc.name)} does not declare environment "node"`,
      },
    ]);
  }
  const graph = instantiateFlowgraph(doc, options.registry, "node");
  const runtime = new Runtime(graph);
  await runtime.init();
  if (options.afterInit) await options.afterInit(graph);
  await runtime.start();
  await runtime.stop();
  return graph;
}

export type {
  BlockRegistryLike,
  FlowgraphDoc,
  InstantiatedGraph,
} from "@ferrite/flowgraph-runtime";
