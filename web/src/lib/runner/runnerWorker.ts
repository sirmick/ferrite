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
  createFrameClient: (wsUrl) => new FrameClient({ url: wsUrl }),
  splitDoc: (doc, env) => splitFlowgraphForEnv(doc, env),
  createRuntime: (doc, env) => createRuntime(doc, env),
});

self.onmessage = async (ev: MessageEvent<RunnerRequest>) => {
  const resp = await core.handle(ev.data);
  (self as unknown as Worker).postMessage(resp);
};
