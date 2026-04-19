// Flowgraph runtime — public entry point.
// Phase D is landing the runtime in slices: parser + structural validation
// first (this commit), then block registry + wiring, then the scheduler.

export * from "./types.js";
export {
  FlowgraphValidationError,
  parseFlowgraph,
  validateFlowgraph,
} from "./validate.js";
export type { ParsedFlowgraph } from "./validate.js";
export { instantiateFlowgraph } from "./instantiate.js";
export type {
  BlockRegistryLike,
  InstantiatedGraph,
  RegisteredBlock,
} from "./instantiate.js";
export { Scheduler, buildWirePlan, topologicalOrder } from "./schedule.js";
export type { InputSource, WirePlan } from "./schedule.js";

export const RUNTIME_VERSION = "0.0.1";
