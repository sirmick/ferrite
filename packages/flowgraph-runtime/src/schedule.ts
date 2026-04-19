// Scheduler wire-up — takes an `InstantiatedGraph` and builds the data
// structures the lifecycle (next commit) needs to drive it:
//
//   • a **topological order** of block IDs — blocks appear after every
//     producer they consume from, so a tick can walk the list once
//   • a **wire plan** — for each block's input ports, which upstream
//     (block, port) feeds it
//
// The caller is expected to have already passed the graph through
// `instantiateFlowgraph`, which rejects cycles, unknown endpoints, and
// type mismatches. This module assumes those invariants and is allowed
// to throw `Error` if they are violated — the exceptions are bugs in
// the caller, not graph-author mistakes.
//
// Why Kahn's algorithm over the DFS that `validate.ts` already runs:
// Kahn is simpler to implement deterministically (sort the ready set by
// instance id) so test fixtures can assert on exact orderings. Cost is
// negligible at flowgraph scale.

import type { InstantiatedGraph } from "./instantiate.js";
import type { PortType } from "./types.js";

/**
 * For one consumer input port, where its samples come from. Both halves
 * of the tuple are guaranteed to reference real blocks/ports in the
 * graph — `instantiateFlowgraph` validated that.
 */
export interface InputSource {
  readonly sourceBlock: string;
  readonly sourcePort: string;
  readonly portType: PortType;
}

/** Per block id → (input port name → its upstream producer). */
export type WirePlan = ReadonlyMap<string, ReadonlyMap<string, InputSource>>;

/**
 * Scheduler wire-up. A `Scheduler` instance is cheap to build and
 * immutable after construction; rebuild if the graph changes.
 */
export class Scheduler {
  readonly order: ReadonlyArray<string>;
  readonly wirePlan: WirePlan;

  constructor(readonly graph: InstantiatedGraph) {
    this.wirePlan = buildWirePlan(graph);
    this.order = topologicalOrder(graph);
  }
}

// ---------------------------------------------------------------------------
// Topological order — Kahn with deterministic tie-breaking.
// ---------------------------------------------------------------------------

/**
 * Returns block IDs in a run order: every producer appears before every
 * consumer of its output. When multiple blocks are ready simultaneously
 * they are emitted in lexicographic id order so callers can pin tests
 * against exact sequences.
 *
 * Assumes the graph has already been validated acyclic. Throws if a
 * cycle sneaks in (defensive; should be unreachable via the public API).
 */
export function topologicalOrder(
  graph: InstantiatedGraph,
): ReadonlyArray<string> {
  const blockIds = Object.keys(graph.doc.blocks);
  const indegree = new Map<string, number>();
  const out = new Map<string, Set<string>>();
  for (const id of blockIds) {
    indegree.set(id, 0);
    out.set(id, new Set());
  }
  for (const [from, to] of graph.doc.wires) {
    const src = splitEndpoint(from)[0];
    const dst = splitEndpoint(to)[0];
    // Self-loops are cycles; validated away, but guard anyway.
    if (src === dst) continue;
    if (!out.get(src)!.has(dst)) {
      out.get(src)!.add(dst);
      indegree.set(dst, (indegree.get(dst) ?? 0) + 1);
    }
  }
  const ready: string[] = [];
  for (const id of blockIds) {
    if ((indegree.get(id) ?? 0) === 0) ready.push(id);
  }
  ready.sort();

  const order: string[] = [];
  while (ready.length > 0) {
    const next = ready.shift()!;
    order.push(next);
    for (const dst of out.get(next) ?? []) {
      const d = (indegree.get(dst) ?? 0) - 1;
      indegree.set(dst, d);
      if (d === 0) {
        insertSorted(ready, dst);
      }
    }
  }
  if (order.length !== blockIds.length) {
    throw new Error(
      `scheduler bug: topological order missing blocks — graph contains a cycle but was not rejected by instantiateFlowgraph`,
    );
  }
  return order;
}

function insertSorted(arr: string[], value: string): void {
  let lo = 0;
  let hi = arr.length;
  while (lo < hi) {
    const mid = (lo + hi) >>> 1;
    if (arr[mid]! < value) lo = mid + 1;
    else hi = mid;
  }
  arr.splice(lo, 0, value);
}

// ---------------------------------------------------------------------------
// Wire plan — reverse-index wires by consumer input.
// ---------------------------------------------------------------------------

/**
 * Builds a `WirePlan`: for every block, the map from its input port
 * names to the (block, port) feeding them. Blocks with no inputs get an
 * empty inner map — they're still present so callers can iterate every
 * instance uniformly.
 */
export function buildWirePlan(graph: InstantiatedGraph): WirePlan {
  const plan = new Map<string, Map<string, InputSource>>();
  for (const id of Object.keys(graph.doc.blocks)) {
    plan.set(id, new Map());
  }
  for (const [from, to] of graph.doc.wires) {
    const [srcBlock, srcPort] = splitEndpoint(from);
    const [dstBlock, dstPort] = splitEndpoint(to);
    const srcSpec = graph.specs.get(srcBlock);
    // Both sides exist (instantiateFlowgraph enforced it) but defend
    // anyway — a misuse should surface loud.
    if (!srcSpec) {
      throw new Error(
        `scheduler bug: wire ${from} → ${to} references unknown source block ${srcBlock}`,
      );
    }
    const outPort = srcSpec.outputs.find((p) => p.name === srcPort);
    if (!outPort) {
      throw new Error(
        `scheduler bug: wire ${from} → ${to} references unknown source port ${srcPort} on ${srcBlock}`,
      );
    }
    plan.get(dstBlock)!.set(dstPort, {
      sourceBlock: srcBlock,
      sourcePort: srcPort,
      portType: outPort.portType,
    });
  }
  return plan;
}

function splitEndpoint(endpoint: string): [string, string] {
  const dot = endpoint.indexOf(".");
  if (dot < 0) return [endpoint, ""];
  return [endpoint.slice(0, dot), endpoint.slice(dot + 1)];
}
