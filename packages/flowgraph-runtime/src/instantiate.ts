// Flowgraph instantiation — the second slice of the runtime, taking a
// structurally valid `FlowgraphDoc` + a block registry and producing a
// map of constructed `BlockInstance`s ready for the scheduler to wire up.
//
// This file adds the registry-dependent validation phases that were
// reserved as strings in `ValidationError` but unchecked in `validate.ts`:
//
//   2. env_match       — every declared type is registered and runnable
//                        in the target environment
//   4. wire_endpoints  — wire endpoints resolve to real ports on real
//                        blocks (validate.ts caught shape-level errors
//                        only; this enforces the graph references the
//                        registry knows about)
//   5. wire_type_match — the PortType on each end of a wire agrees
//
// Param validation (`params` phase) is defensible either here or in a
// per-block module — we leave it for a later commit when the first block
// that *needs* generic range/enum checks lands. The shape validator
// already ensures params is an object; runtime-side `construct` is free
// to reject bad values from there.
//
// Wiring (#79) and the scheduler/lifecycle (#80) run on the `instances`
// + `specs` maps this module returns.

import type {
  BlockInstance,
  BlockSpec,
  Environment,
  FlowgraphDoc,
  PortSpec,
  PortType,
  ValidationError,
  Wire,
} from "./types.js";
import { FlowgraphValidationError } from "./validate.js";

/**
 * The subset of a block registry this module actually uses. Defined here
 * so the runtime package never imports from `@ferrite/flowgraph-blocks` —
 * the concrete `BlockRegistry` in that package satisfies this shape
 * structurally, but other registries (e.g. a test fake or a dynamically
 * loaded set) work too.
 */
export interface BlockRegistryLike {
  get(typeName: string): RegisteredBlock | undefined;
  has(typeName: string): boolean;
}

/** The runtime's view of a registered block: spec + factory. */
export interface RegisteredBlock {
  readonly spec: BlockSpec;
  construct(params: unknown): BlockInstance;
}

/** Result of a successful `instantiateFlowgraph` call. */
export interface InstantiatedGraph {
  readonly doc: FlowgraphDoc;
  readonly environment: Environment;
  readonly instances: ReadonlyMap<string, BlockInstance>;
  readonly specs: ReadonlyMap<string, BlockSpec>;
}

/**
 * Validate a flowgraph against a registry and construct every block.
 * The caller is expected to have already passed `doc` through
 * `validateFlowgraph` for the shape/fan/dag/connectivity checks. This
 * function layers on the checks that need the registry and then calls
 * each block module's `construct` with the declared params.
 *
 * On validation failure, throws `FlowgraphValidationError` *before*
 * calling any `construct` — construction is all-or-nothing so half-built
 * graphs never escape.
 */
export function instantiateFlowgraph(
  doc: FlowgraphDoc,
  registry: BlockRegistryLike,
  environment: Environment,
): InstantiatedGraph {
  if (!doc.environments.includes(environment)) {
    throw new FlowgraphValidationError([
      {
        phase: "env_match",
        error: `flowgraph ${JSON.stringify(doc.name)} does not declare environment ${JSON.stringify(environment)} (declared: ${JSON.stringify(doc.environments)})`,
      },
    ]);
  }

  const errors: ValidationError[] = [];
  errors.push(...checkEnvMatch(doc, registry, environment));
  errors.push(...checkWireEndpoints(doc, registry));
  errors.push(...checkWireTypeMatch(doc, registry));

  if (errors.length > 0) {
    throw new FlowgraphValidationError(errors);
  }

  const instances = new Map<string, BlockInstance>();
  const specs = new Map<string, BlockSpec>();
  for (const [id, decl] of Object.entries(doc.blocks)) {
    const mod = registry.get(decl.type);
    // Guaranteed present — checkEnvMatch would have errored otherwise.
    if (!mod) continue;
    specs.set(id, mod.spec);
    instances.set(id, mod.construct(decl.params ?? {}));
  }
  return { doc, environment, instances, specs };
}

// ---------------------------------------------------------------------------
// env_match — type exists, placement is compatible with the chosen env
// ---------------------------------------------------------------------------

function checkEnvMatch(
  doc: FlowgraphDoc,
  registry: BlockRegistryLike,
  environment: Environment,
): ValidationError[] {
  const errors: ValidationError[] = [];
  for (const [id, decl] of Object.entries(doc.blocks)) {
    const mod = registry.get(decl.type);
    if (!mod) {
      errors.push({
        phase: "env_match",
        block: id,
        error: `block type ${JSON.stringify(decl.type)} is not registered`,
      });
      continue;
    }
    if (!placementFits(mod.spec.placement, environment)) {
      errors.push({
        phase: "env_match",
        block: id,
        error: `block type ${JSON.stringify(decl.type)} is ${mod.spec.placement}, cannot run in ${environment}`,
      });
    }
  }
  return errors;
}

function placementFits(
  placement: BlockSpec["placement"],
  environment: Environment,
): boolean {
  if (placement === "either") return true;
  if (placement === "native_only") return environment === "node";
  return environment === "browser";
}

// ---------------------------------------------------------------------------
// wire_endpoints — both sides resolve to declared ports
// ---------------------------------------------------------------------------

function checkWireEndpoints(
  doc: FlowgraphDoc,
  registry: BlockRegistryLike,
): ValidationError[] {
  const errors: ValidationError[] = [];
  for (const wire of doc.wires) {
    const [src, dst] = wire;
    const srcErr = resolvePort(src, "output", doc, registry);
    if (srcErr) errors.push({ ...srcErr, phase: "wire_endpoints", wire });
    const dstErr = resolvePort(dst, "input", doc, registry);
    if (dstErr) errors.push({ ...dstErr, phase: "wire_endpoints", wire });
  }
  return errors;
}

interface ResolveFailure {
  error: string;
  block?: string;
  port?: string;
}

function resolvePort(
  endpoint: string,
  side: "input" | "output",
  doc: FlowgraphDoc,
  registry: BlockRegistryLike,
): ResolveFailure | undefined {
  const [instanceId, portName] = splitEndpoint(endpoint);
  const decl = doc.blocks[instanceId];
  if (!decl) {
    return {
      error: `endpoint ${JSON.stringify(endpoint)} references unknown block ${JSON.stringify(instanceId)}`,
      block: instanceId,
    };
  }
  const mod = registry.get(decl.type);
  if (!mod) {
    // env_match already emitted an error for this; skip here so the
    // caller does not see the same block flagged twice.
    return undefined;
  }
  const ports = side === "input" ? mod.spec.inputs : mod.spec.outputs;
  if (!ports.some((p) => p.name === portName)) {
    return {
      error: `block ${JSON.stringify(instanceId)} (${decl.type}) has no ${side} port ${JSON.stringify(portName)}`,
      block: instanceId,
      port: portName,
    };
  }
  return undefined;
}

// ---------------------------------------------------------------------------
// wire_type_match — PortType agrees on both sides
// ---------------------------------------------------------------------------

function checkWireTypeMatch(
  doc: FlowgraphDoc,
  registry: BlockRegistryLike,
): ValidationError[] {
  const errors: ValidationError[] = [];
  for (const wire of doc.wires) {
    const srcType = portType(wire[0], "output", doc, registry);
    const dstType = portType(wire[1], "input", doc, registry);
    if (!srcType || !dstType) continue; // wire_endpoints already errored
    if (srcType !== dstType) {
      errors.push({
        phase: "wire_type_match",
        wire,
        error: `${wire[0]} is ${srcType}; ${wire[1]} expects ${dstType}`,
      });
    }
  }
  return errors;
}

function portType(
  endpoint: string,
  side: "input" | "output",
  doc: FlowgraphDoc,
  registry: BlockRegistryLike,
): PortType | undefined {
  const [instanceId, portName] = splitEndpoint(endpoint);
  const decl = doc.blocks[instanceId];
  if (!decl) return undefined;
  const mod = registry.get(decl.type);
  if (!mod) return undefined;
  const ports = side === "input" ? mod.spec.inputs : mod.spec.outputs;
  return ports.find((p: PortSpec) => p.name === portName)?.portType;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function splitEndpoint(endpoint: string): [string, string] {
  const dot = endpoint.indexOf(".");
  if (dot < 0) return [endpoint, ""];
  return [endpoint.slice(0, dot), endpoint.slice(dot + 1)];
}

export type { Wire };
