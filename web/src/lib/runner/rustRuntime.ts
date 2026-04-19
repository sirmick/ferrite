// Thin TS wrapper over the wasm-pack output for `ferrite-runtime`.
//
// The underlying module is generated under `../wasm/runtime/` by
// `pnpm wasm:build:runtime`. This file is the import shim — it does
// the one-time WASM instantiation and exposes the Rust-side exports
// behind a small typed surface. As M4 progresses, additional exports
// (doc parse, tick pump, update) gain matching helpers here; today
// the only export is `version()` and this is purely a tracer bullet
// proving the wasm-pack + vite-plugin-wasm path is wired.
//
// Instantiation is idempotent — the first caller awaits `init()`; later
// callers get the already-resolved module instantly.

import initWasm, { parseAndValidateDoc, version } from '../wasm/runtime/runtime.js';

let initPromise: Promise<void> | undefined;

async function ensureInit(): Promise<void> {
  if (!initPromise) {
    initPromise = (async () => {
      await initWasm();
    })();
  }
  await initPromise;
}

export async function rustRuntimeVersion(): Promise<string> {
  await ensureInit();
  return version();
}

/**
 * Parse a flowgraph JSON string and run the registry-independent
 * validation passes. Returns the doc's `name` on success; throws the
 * underlying Rust error if validation fails.
 */
export async function validateFlowgraphJson(json: string): Promise<string> {
  await ensureInit();
  return parseAndValidateDoc(json);
}
