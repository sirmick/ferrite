// Runner Worker entry point — glue that dispatches incoming messages
// to `RunnerCore` and posts the reply back.
//
// Worker-only: `self` is the worker's global scope. Unit-tested via
// `RunnerCore` directly (this file is a thin adapter with no branching
// worth covering).

import { FrameClient } from '../ws/client.js';
import type { RunnerRequest } from './protocol.js';
import { RunnerCore } from './runnerCore.js';
import { createRuntime, splitFlowgraphForEnv } from './rustRuntime.js';

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
