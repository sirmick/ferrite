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

import initWasm, {
  parseAndValidateDoc,
  RuntimeHandle,
  splitDocForEnvironment,
  version,
} from '../wasm/runtime/runtime.js';

import type { FlowgraphDoc } from '@ferrite/flowgraph-runtime/types';

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

/**
 * Split a cross-env flowgraph for the given environment. Returns the
 * resulting subgraph doc with auto-inserted `WsBridgeTx`/`WsBridgeRx`
 * pairs terminating every cross-env wire. Stream IDs start at
 * `CROSS_ENV_STREAM_BASE` (1000) and match the other half's allocation
 * deterministically.
 */
export async function splitFlowgraphForEnv(
  doc: FlowgraphDoc,
  env: 'node' | 'browser',
): Promise<FlowgraphDoc> {
  await ensureInit();
  const out = splitDocForEnvironment(JSON.stringify(doc), env);
  return JSON.parse(out) as FlowgraphDoc;
}

/**
 * Construct a Rust-side runtime over `doc` targeting `env`. The returned
 * handle is a live wasm object — callers own it and must invoke
 * `handle.free()` (or `stop()` then drop) to release the underlying Rust
 * allocation. `init()` must be called before the first `tick()`.
 */
export async function createRuntime(
  doc: FlowgraphDoc,
  env: 'node' | 'browser',
  framesHint?: number,
): Promise<RuntimeHandle> {
  await ensureInit();
  return new RuntimeHandle(JSON.stringify(doc), env, framesHint);
}

/**
 * Copy audio samples accumulated in the named `AudioSink` block into
 * `out`. Returns the number of samples actually written; short return
 * means the sink's ring drained dry. Typically invoked immediately
 * after `tick()` by the flowgraph runner Worker, which then pushes
 * the samples into a `SharedArrayBuffer` read by the `AudioWorklet`.
 */
export function drainAudioFromSink(rt: RuntimeHandle, blockId: string, out: Float32Array): number {
  return rt.drainAudio(blockId, out);
}

/**
 * Push a batch of interleaved IQ floats into the named `WsIqSource`
 * block. Typically called from the Worker that hosts the multiplexed
 * WebSocket: each incoming IQ frame for this stream id lands here, the
 * block's internal ring absorbs it, and the next `tick` emits the
 * samples onto the flowgraph.
 */
export function pushIqIntoSource(rt: RuntimeHandle, blockId: string, samples: Float32Array): void {
  rt.pushIq(blockId, samples);
}

export type { RuntimeHandle };
