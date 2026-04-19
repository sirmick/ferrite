// Decimator — integer-factor IQ decimation with windowed-sinc LPF.

import type { BlockSpec } from "@ferrite/flowgraph-runtime/types";

import { defineBlock } from "../../registry.js";
import specJson from "./spec.json";
import { Decimator, type DecimatorParams } from "./block.js";

const spec = specJson as BlockSpec;

export default defineBlock({
  spec,
  construct: (params) => new Decimator((params ?? {}) as DecimatorParams),
});
