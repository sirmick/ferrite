// FmDemod — phase-discriminator FM demodulator, pure TS.

import type { BlockSpec } from "@ferrite/flowgraph-runtime/types";

import { defineBlock } from "../../registry.js";
import specJson from "./spec.json";
import { FmDemod, type FmDemodParams } from "./block.js";

const spec = specJson as BlockSpec;

export default defineBlock({
  spec,
  construct: (params) => new FmDemod((params ?? {}) as FmDemodParams),
});
