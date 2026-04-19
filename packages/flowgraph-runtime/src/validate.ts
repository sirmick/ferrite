// Flowgraph JSON parser + structural validators.
//
// Implements the first four checks from `docs/04-flowgraphs.md` §"Validation":
//
//   1. shape         — JSON conforms to the top-level flowgraph schema
//   6. fan           — each port has at most one wire
//   7. dag           — the block graph is acyclic
//   8. connectivity  — warn on blocks with no wires (non-fatal)
//
// The remaining phases (env_match, params, wire_endpoints, wire_type_match,
// rate_negotiation) need the block registry and are the subject of later
// commits in Phase D. They are represented as distinct `phase` values in
// `ValidationError` so the caller can slot them in without a type change.
//
// The shape checks below are hand-written rather than delegated to Ajv;
// the schema is small enough that dragging a JSON Schema validator in as
// a dependency is the wrong trade-off. If the schema grows teeth (nested
// allOf/oneOf, $ref chains), swap to Ajv behind the same `parseFlowgraph`
// signature.

import type {
  BlockInstanceDecl,
  Environment,
  FlowgraphDoc,
  ValidationError,
  Wire,
} from "./types.js";

/**
 * Raised when a flowgraph JSON input fails parsing or structural
 * validation. Carries the list of `ValidationError` — usually one, but
 * independent fan/connectivity checks can yield several.
 */
export class FlowgraphValidationError extends Error {
  readonly errors: ReadonlyArray<ValidationError>;

  constructor(errors: ReadonlyArray<ValidationError>) {
    super(
      `flowgraph validation failed: ${errors
        .map((e) => `[${e.phase}] ${e.error}`)
        .join("; ")}`,
    );
    this.name = "FlowgraphValidationError";
    this.errors = errors;
  }
}

/**
 * Parse a JSON string into a validated `FlowgraphDoc`. Throws on malformed
 * JSON or any shape/fan/dag check failure. Connectivity warnings are
 * returned, not thrown — callers that want "no warnings at all" can
 * inspect the `warnings` list on the result.
 */
export function parseFlowgraph(json: string): ParsedFlowgraph {
  let raw: unknown;
  try {
    raw = JSON.parse(json);
  } catch (err) {
    throw new FlowgraphValidationError([
      {
        phase: "shape",
        error: `invalid JSON: ${(err as Error).message}`,
      },
    ]);
  }
  return validateFlowgraph(raw);
}

export interface ParsedFlowgraph {
  readonly doc: FlowgraphDoc;
  readonly warnings: ReadonlyArray<ValidationError>;
}

/**
 * Validate an already-parsed JSON value. Split out so callers who already
 * have an object (e.g. from Vite's JSON import) can skip the parse step.
 */
export function validateFlowgraph(raw: unknown): ParsedFlowgraph {
  const shapeErrors = validateShape(raw);
  if (shapeErrors.length > 0) {
    throw new FlowgraphValidationError(shapeErrors);
  }
  // Shape validation narrowed `raw` — assert that here rather than
  // threading a type guard through the helpers.
  const doc = raw as FlowgraphDoc;

  const errors: ValidationError[] = [];
  errors.push(...checkFan(doc));
  errors.push(...checkDag(doc));
  const warnings = checkConnectivity(doc);

  if (errors.length > 0) {
    throw new FlowgraphValidationError(errors);
  }
  return { doc, warnings };
}

// ---------------------------------------------------------------------------
// Phase 1 — shape
// ---------------------------------------------------------------------------

const VALID_ENVIRONMENTS: ReadonlySet<Environment> = new Set([
  "browser",
  "node",
]);

function validateShape(raw: unknown): ValidationError[] {
  if (!isPlainObject(raw)) {
    return [{ phase: "shape", error: "flowgraph must be a JSON object" }];
  }
  const errors: ValidationError[] = [];
  const obj = raw;

  if (typeof obj.name !== "string" || obj.name.length === 0) {
    errors.push({ phase: "shape", error: "`name` must be a non-empty string" });
  }
  if (obj.label !== undefined && typeof obj.label !== "string") {
    errors.push({ phase: "shape", error: "`label` must be a string when set" });
  }
  if (obj.description !== undefined && typeof obj.description !== "string") {
    errors.push({
      phase: "shape",
      error: "`description` must be a string when set",
    });
  }
  if (!Array.isArray(obj.environments) || obj.environments.length === 0) {
    errors.push({
      phase: "shape",
      error: "`environments` must be a non-empty array",
    });
  } else {
    for (const env of obj.environments) {
      if (
        typeof env !== "string" ||
        !VALID_ENVIRONMENTS.has(env as Environment)
      ) {
        errors.push({
          phase: "shape",
          error: `unknown environment ${JSON.stringify(env)} (expected 'browser' or 'node')`,
        });
      }
    }
  }

  if (!isPlainObject(obj.blocks)) {
    errors.push({ phase: "shape", error: "`blocks` must be an object" });
  } else {
    for (const [instanceId, decl] of Object.entries(obj.blocks)) {
      errors.push(...validateBlockDecl(instanceId, decl));
    }
  }

  if (!Array.isArray(obj.wires)) {
    errors.push({ phase: "shape", error: "`wires` must be an array" });
  } else {
    for (let i = 0; i < obj.wires.length; i++) {
      errors.push(...validateWire(obj.wires[i], i));
    }
  }
  return errors;
}

function validateBlockDecl(
  instanceId: string,
  decl: unknown,
): ValidationError[] {
  if (!isPlainObject(decl)) {
    return [
      {
        phase: "shape",
        block: instanceId,
        error: `block ${JSON.stringify(instanceId)} must be an object`,
      },
    ];
  }
  const errors: ValidationError[] = [];
  if (typeof decl.type !== "string" || decl.type.length === 0) {
    errors.push({
      phase: "shape",
      block: instanceId,
      error: `block ${JSON.stringify(instanceId)} missing \`type\` string`,
    });
  }
  if (decl.params !== undefined && !isPlainObject(decl.params)) {
    errors.push({
      phase: "shape",
      block: instanceId,
      error: `block ${JSON.stringify(instanceId)} \`params\` must be an object when set`,
    });
  }
  return errors;
}

function validateWire(raw: unknown, index: number): ValidationError[] {
  if (!Array.isArray(raw) || raw.length !== 2) {
    return [
      {
        phase: "shape",
        error: `wire[${index}] must be a [source, sink] pair`,
      },
    ];
  }
  const errors: ValidationError[] = [];
  for (const endpoint of raw) {
    if (typeof endpoint !== "string" || !endpoint.includes(".")) {
      errors.push({
        phase: "shape",
        error: `wire[${index}] endpoints must be \"instance.port\" strings (got ${JSON.stringify(endpoint)})`,
      });
    }
  }
  return errors;
}

// ---------------------------------------------------------------------------
// Phase 6 — fan (one wire per port)
// ---------------------------------------------------------------------------

function checkFan(doc: FlowgraphDoc): ValidationError[] {
  const seenOut = new Set<string>();
  const seenIn = new Set<string>();
  const errors: ValidationError[] = [];
  for (const wire of doc.wires) {
    const [a, b] = wire;
    if (seenOut.has(a)) {
      errors.push({
        phase: "fan",
        wire,
        error: `output port ${JSON.stringify(a)} is wired more than once — use a Tee block to broadcast`,
      });
    }
    seenOut.add(a);
    if (seenIn.has(b)) {
      errors.push({
        phase: "fan",
        wire,
        error: `input port ${JSON.stringify(b)} is wired more than once`,
      });
    }
    seenIn.add(b);
  }
  return errors;
}

// ---------------------------------------------------------------------------
// Phase 7 — DAG (no cycles)
// ---------------------------------------------------------------------------

function checkDag(doc: FlowgraphDoc): ValidationError[] {
  // Build adjacency keyed by instance id (wires already addressed by
  // "instance.port"; strip the port for reachability purposes).
  const adj = new Map<string, Set<string>>();
  const blockIds = Object.keys(doc.blocks);
  for (const id of blockIds) {
    adj.set(id, new Set());
  }
  for (const [a, b] of doc.wires) {
    const [fromId] = splitEndpoint(a);
    const [toId] = splitEndpoint(b);
    if (!adj.has(fromId) || !adj.has(toId)) {
      // Dangling endpoint — not our phase, let wire_endpoints catch it.
      continue;
    }
    adj.get(fromId)!.add(toId);
  }

  // White/grey/black DFS. A back-edge to a grey node is a cycle.
  const WHITE = 0;
  const GREY = 1;
  const BLACK = 2;
  const color = new Map<string, number>();
  for (const id of blockIds) color.set(id, WHITE);

  const errors: ValidationError[] = [];
  const stack: { id: string; iter: Iterator<string> }[] = [];
  for (const root of blockIds) {
    if (color.get(root) !== WHITE) continue;
    color.set(root, GREY);
    stack.push({ id: root, iter: adj.get(root)!.values() });
    while (stack.length > 0) {
      const frame = stack[stack.length - 1]!;
      const next = frame.iter.next();
      if (next.done) {
        color.set(frame.id, BLACK);
        stack.pop();
        continue;
      }
      const nb = next.value;
      const c = color.get(nb);
      if (c === GREY) {
        errors.push({
          phase: "dag",
          error: `cycle detected: ${frame.id} → ${nb}`,
        });
        // One cycle is enough — keep a short error list.
        return errors;
      }
      if (c === WHITE) {
        color.set(nb, GREY);
        stack.push({ id: nb, iter: adj.get(nb)!.values() });
      }
    }
  }
  return errors;
}

// ---------------------------------------------------------------------------
// Phase 8 — connectivity (warning, not error)
// ---------------------------------------------------------------------------

function checkConnectivity(doc: FlowgraphDoc): ValidationError[] {
  const touched = new Set<string>();
  for (const [a, b] of doc.wires) {
    touched.add(splitEndpoint(a)[0]);
    touched.add(splitEndpoint(b)[0]);
  }
  const warnings: ValidationError[] = [];
  for (const id of Object.keys(doc.blocks)) {
    if (!touched.has(id)) {
      warnings.push({
        phase: "connectivity",
        block: id,
        error: `block ${JSON.stringify(id)} has no wires (isolated)`,
      });
    }
  }
  return warnings;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** Split "instance.port" into [instance, port]. Falls back to the whole
 * string as the instance if there is no dot — shape validation already
 * rejects that case, so the return here is defensive only. */
function splitEndpoint(endpoint: string): [string, string] {
  const dot = endpoint.indexOf(".");
  if (dot < 0) return [endpoint, ""];
  return [endpoint.slice(0, dot), endpoint.slice(dot + 1)];
}

// Re-export wire tuple form so test files have a single import surface.
export type { Wire, BlockInstanceDecl, ValidationError, FlowgraphDoc };
