// WASM-block wrappers.
//
// Each block in the ferrite-blocks Rust crate gets a thin TS wrapper
// implementing the runtime's Block interface. The concrete wrappers
// land alongside their Rust counterparts starting Phase B.

import type { PortType } from '@ferrite/flowgraph-runtime/types';

export const BLOCKS_PACKAGE_VERSION = '0.0.1';

export interface BlockWrapperSpec {
  readonly typeName: string;
  readonly inputs: ReadonlyArray<{ name: string; type: PortType }>;
  readonly outputs: ReadonlyArray<{ name: string; type: PortType }>;
}
