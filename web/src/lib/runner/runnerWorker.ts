// Runner Worker entry point — glue that dispatches incoming messages
// to `RunnerCore` and posts the reply back.
//
// Worker-only: `self` is the worker's global scope. Unit-tested via
// `RunnerCore` directly (this file is a 15-line adapter with no
// branching worth covering).

import { FrameClient } from '../ws/client.js';
import { createWebBlockRegistry } from './blockRegistry.js';
import type { RunnerRequest } from './protocol.js';
import { RunnerCore } from './runnerCore.js';

const core = new RunnerCore({
  createFrameClient: (wsUrl) => new FrameClient({ url: wsUrl }),
  createRegistry: (client) => createWebBlockRegistry(client),
});

self.onmessage = async (ev: MessageEvent<RunnerRequest>) => {
  const resp = await core.handle(ev.data);
  (self as unknown as Worker).postMessage(resp);
};
