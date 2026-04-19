// @ferrite/flowgraph-blocks — catalog of block modules for flowgraph runtimes.
//
// Contract: every block lives in `src/blocks/<kebab-name>/` and default-exports
// a `BlockModule` (see `registry.ts`). This file re-exports the registry
// primitives plus a `registerAll(registry)` helper that wires up every known
// block in one call. The list is hand-maintained until enough blocks land to
// justify a codegen barrel.

import { BlockRegistry, type BlockModule } from "./registry.js";
import decimator from "./blocks/decimator/index.js";
import fmDemod from "./blocks/fm-demod/index.js";

export const BLOCKS_PACKAGE_VERSION = "0.0.1";

export { BlockRegistry, defineBlock } from "./registry.js";
export type { BlockModule, BlockWidget, InstallCtx } from "./registry.js";

/** All blocks shipped by this package, keyed by `spec.typeName`. */
export const blocks: Readonly<Record<string, BlockModule>> = {
  Decimator: decimator,
  FmDemod: fmDemod,
};

/** Register every block in this package into `registry`. */
export function registerAll(registry: BlockRegistry): void {
  for (const mod of Object.values(blocks)) {
    registry.add(mod);
  }
}
