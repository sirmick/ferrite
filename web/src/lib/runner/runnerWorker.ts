// Runner Worker entry point — glue that dispatches incoming messages
// to `RunnerCore` and posts the reply back.
//
// Worker-only: `self` is the worker's global scope. Unit-tested via
// `RunnerCore` directly (this file is a thin adapter with no branching
// worth covering).

import { FrameClient } from '../ws/client.js';
import { initFrameDecoder } from '../ws/frame.js';
import type { RunnerRequest } from './protocol.js';
import { RunnerCore } from './runnerCore.js';
import { createRuntime, splitFlowgraphForEnv } from './rustRuntime.js';

// The frame decoder wasm is realm-local — the main thread initialises
// its own on page load; the worker needs its own before the first
// frame arrives on the subscribed WebSocket. Fire this once at worker
// startup; `FrameClient.onMessage` will block decode attempts until
// it resolves. (The core's unit tests construct `RunnerCore` directly
// without going through this glue, so they stay WASM-free.)
void initFrameDecoder();

// NB: the fldigi Emscripten bridge is NOT loaded unconditionally here.
// Eagerly importing the Emscripten module in every worker is wasteful
// (most flowgraphs have no fldigi block). Instead it's instantiated
// lazily and conditionally in `rustRuntime.createRuntime()` — only
// when the browser-side split doc actually contains a fldigi block,
// and before the Rust runtime constructs it (its wasm-bindgen snippet
// reads the module off `globalThis.__FERRITE_FLDIGI__`). Inert if the
// artifact is absent. Bridge code stays in web/src/lib/wasm/fldigi/.

const core = new RunnerCore({
  createFrameClient: (wsUrl) =>
    new FrameClient({
      url: wsUrl,
      // Status and decode errors inside the worker are invisible to the
      // main thread otherwise — forward them through the diag channel
      // so `[runner-ws]` lines land in the logs store alongside the
      // per-second tick summary.
      onStatus: (s) => {
        (self as unknown as Worker).postMessage({
          kind: 'diag',
          text: `runner-ws: ${s}`,
        });
      },
      onDecodeError: (err) => {
        (self as unknown as Worker).postMessage({
          kind: 'diag',
          text: `runner-ws decode: ${err.message}`,
        });
      },
    }),
  splitDoc: (doc, env) => splitFlowgraphForEnv(doc, env),
  // Pass a larger `frames_hint` so the scheduler's per-tick batch
  // accommodates wideband cross-env flow. Default 1024 caps each tick
  // at 1024 IQ samples → 204 audio samples/tick → ~12 kHz effective
  // rate at 60 ticks/sec (visible as 4x-slow playback against 48 kHz
  // Web Audio). 8192 gives 98 kHz headroom, enough for anything up to
  // the channelizer output at 250 kS/s IQ with comfortable margin.
  createRuntime: (doc, env) => createRuntime(doc, env, 8192),
});

self.onmessage = async (ev: MessageEvent<RunnerRequest>) => {
  const resp = await core.handle(ev.data);
  (self as unknown as Worker).postMessage(resp);
};
