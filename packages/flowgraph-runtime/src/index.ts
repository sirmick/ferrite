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

export const RUNTIME_VERSION = "0.0.1";
