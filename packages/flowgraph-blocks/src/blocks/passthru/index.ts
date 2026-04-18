// Passthru block — demonstrates the folder contract.

import type { BlockSpec } from "@ferrite/flowgraph-runtime/types";

import { defineBlock } from "../../registry.js";
import specJson from "./spec.json";
import { Passthru, type PassthruParams } from "./block.js";

const spec = specJson as BlockSpec;

export default defineBlock({
  spec,
  construct: (params) => new Passthru((params ?? {}) as PassthruParams),
});
