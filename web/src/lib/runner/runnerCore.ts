// RunnerCore — the worker-agnostic half of the flowgraph runner.
//
// The Worker glue (`runnerWorker.ts`) forwards every inbound message
// to `RunnerCore.handle`, which dispatches it against the lifecycle
// and produces a single response envelope. Kept free of Worker globals
// so the dispatch logic is unit-testable in jsdom.
//
// One RunnerCore hosts at most one Runtime. A second `load` after a
// successful load is rejected — the caller must `stop` first, then
// spawn a new Worker. That constraint mirrors the Runtime itself (not
// reusable after stop) and keeps the state machine tiny.

import { instantiateFlowgraph, Runtime, type BlockRegistryLike } from '@ferrite/flowgraph-runtime';
import type { FlowgraphDoc } from '@ferrite/flowgraph-runtime/types';

import type { FrameClient } from '../ws/client.js';
import type { LoadResult, RunnerRequest, RunnerResponse } from './protocol.js';

/**
 * Environment hooks the core needs but should not own directly.
 * Injecting lets tests swap in fakes that don't open WebSockets and
 * register stub blocks instead of the production web registry.
 */
export interface RunnerEnv {
  createFrameClient(wsUrl: string): FrameClient;
  createRegistry(client: FrameClient): BlockRegistryLike;
}

export class RunnerCore {
  private runtime: Runtime | null = null;
  private client: FrameClient | null = null;

  constructor(private readonly env: RunnerEnv) {}

  async handle(req: RunnerRequest): Promise<RunnerResponse> {
    try {
      switch (req.kind) {
        case 'load':
          return { id: req.id, ok: true, kind: 'load', data: await this.load(req.doc, req.wsUrl) };
        case 'start':
          await this.requireRuntime().start();
          return { id: req.id, ok: true, kind: 'start' };
        case 'stop':
          await this.stop();
          return { id: req.id, ok: true, kind: 'stop' };
        case 'update':
          this.requireRuntime().update(req.blockId, req.params);
          return { id: req.id, ok: true, kind: 'update' };
        case 'state':
          return {
            id: req.id,
            ok: true,
            kind: 'state',
            data: { state: this.runtime?.state ?? 'created' },
          };
      }
    } catch (err) {
      return { id: req.id, ok: false, error: errorMessage(err) };
    }
  }

  private async load(doc: FlowgraphDoc, wsUrl: string): Promise<LoadResult> {
    if (this.runtime && this.runtime.state !== 'stopped') {
      throw new Error('RunnerCore.load: already loaded — stop() first');
    }
    // Drop any prior stopped runtime before building the new one.
    this.runtime = null;
    this.client?.close();
    this.client = null;

    const client = this.env.createFrameClient(wsUrl);
    try {
      const registry = this.env.createRegistry(client);
      const graph = instantiateFlowgraph(doc, registry, 'browser');
      const runtime = new Runtime(graph);
      await runtime.init();
      this.client = client;
      this.runtime = runtime;
      return {
        blocks: [...graph.instances.keys()],
        audioSabs: collectAudioSabs(graph.instances),
      };
    } catch (err) {
      client.close();
      throw err;
    }
  }

  private async stop(): Promise<void> {
    const rt = this.runtime;
    if (!rt) return;
    try {
      await rt.stop();
    } finally {
      this.client?.close();
      this.client = null;
      // Runtime stays set so callers can still query `state` ("stopped")
      // until the next `load` discards it.
    }
  }

  private requireRuntime(): Runtime {
    if (!this.runtime) {
      throw new Error('RunnerCore: no flowgraph loaded');
    }
    return this.runtime;
  }
}

function collectAudioSabs(
  instances: ReadonlyMap<string, unknown>,
): Record<string, SharedArrayBuffer> {
  const out: Record<string, SharedArrayBuffer> = {};
  for (const [id, block] of instances) {
    const maybe = (block as { sab?: unknown }).sab;
    if (maybe instanceof SharedArrayBuffer) {
      out[id] = maybe;
    }
  }
  return out;
}

function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}
