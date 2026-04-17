// Flowgraph runtime — public entry point.
// The runtime implementation lands in Phase D. Export the type surface now
// so downstream packages can compile against it.

export * from './types';

export const RUNTIME_VERSION = '0.0.1';
