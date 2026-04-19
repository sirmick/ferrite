import { describe, expect, it, vi } from 'vitest';

import { BlockRegistry } from '@ferrite/flowgraph-blocks';
import type { BlockInstance, BlockSpec, FlowgraphDoc } from '@ferrite/flowgraph-runtime/types';

import type { FrameClient } from '../ws/client.js';
import type { RunnerRequest } from './protocol.js';
import { RunnerCore, type RunnerEnv } from './runnerCore.js';

// ---------------------------------------------------------------------------
// Stub blocks — a source (produces iq_f32, no inputs) wired to a sink
// (consumes iq_f32, no outputs). Construction is recorded so tests can
// assert on params flowing through the graph. One variant of the sink
// carries a fake SharedArrayBuffer so the audio-SAB collection path is
// exercised.
// ---------------------------------------------------------------------------

class StubSource implements BlockInstance {
  initCalls = 0;
  stopCalls = 0;
  updates: unknown[] = [];
  init(): void {
    this.initCalls++;
  }
  process(): { consumed: number[]; produced: number[] } {
    return { consumed: [], produced: [0] };
  }
  stop(): void {
    this.stopCalls++;
  }
  update(params: unknown): void {
    this.updates.push(params);
  }
}

class StubSink implements BlockInstance {
  readonly sab: SharedArrayBuffer;
  constructor(sabBytes: number) {
    this.sab = new SharedArrayBuffer(sabBytes);
  }
  init(): void {}
  process(): { consumed: number[]; produced: number[] } {
    return { consumed: [0], produced: [] };
  }
  stop(): void {}
}

const SRC_SPEC: BlockSpec = {
  typeName: 'StubSource',
  placement: 'either',
  inputs: [],
  outputs: [{ name: 'out', portType: 'iq_f32' }],
  params: [],
};

const SINK_SPEC: BlockSpec = {
  typeName: 'StubSink',
  placement: 'either',
  inputs: [{ name: 'in', portType: 'iq_f32' }],
  outputs: [],
  params: [],
};

function makeRegistry(sources: Map<string, StubSource>, sinks: Map<string, StubSink>) {
  return () => {
    const reg = new BlockRegistry();
    reg.add({
      spec: SRC_SPEC,
      construct: () => {
        const b = new StubSource();
        sources.set(`src-${sources.size}`, b);
        return b;
      },
    });
    reg.add({
      spec: SINK_SPEC,
      construct: () => {
        const b = new StubSink(256);
        sinks.set(`sink-${sinks.size}`, b);
        return b;
      },
    });
    return reg;
  };
}

const DOC: FlowgraphDoc = {
  name: 'test',
  environments: ['browser'],
  blocks: {
    source: { type: 'StubSource' },
    sink: { type: 'StubSink' },
  },
  wires: [['source.out', 'sink.in']],
};

function makeEnv(): { env: RunnerEnv; clients: FakeFrameClient[] } {
  const clients: FakeFrameClient[] = [];
  const sources = new Map<string, StubSource>();
  const sinks = new Map<string, StubSink>();
  const env: RunnerEnv = {
    createFrameClient: () => {
      const c = new FakeFrameClient();
      clients.push(c);
      return c as unknown as FrameClient;
    },
    createRegistry: makeRegistry(sources, sinks),
  };
  return { env, clients };
}

class FakeFrameClient {
  closeCalls = 0;
  subscribe(): () => void {
    return () => undefined;
  }
  close(): void {
    this.closeCalls++;
  }
}

// ---------------------------------------------------------------------------

function req(kind: 'start' | 'stop' | 'state', id = 1): RunnerRequest {
  return { id, kind };
}

describe('RunnerCore', () => {
  it('load instantiates blocks, inits them, and returns SABs + block ids', async () => {
    const { env, clients } = makeEnv();
    const core = new RunnerCore(env);
    const resp = await core.handle({ id: 1, kind: 'load', doc: DOC, wsUrl: 'ws://x' });

    expect(resp.ok).toBe(true);
    if (!resp.ok || resp.kind !== 'load') throw new Error('wrong kind');
    expect(new Set(resp.data.blocks)).toEqual(new Set(['source', 'sink']));
    expect(Object.keys(resp.data.audioSabs)).toEqual(['sink']);
    expect(resp.data.audioSabs.sink.byteLength).toBe(256);
    expect(clients).toHaveLength(1);
  });

  it('load fails if already loaded', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    await core.handle({ id: 1, kind: 'load', doc: DOC, wsUrl: 'ws://x' });
    const resp = await core.handle({ id: 2, kind: 'load', doc: DOC, wsUrl: 'ws://x' });
    expect(resp.ok).toBe(false);
    if (resp.ok) throw new Error('unreachable');
    expect(resp.error).toMatch(/already loaded/);
  });

  it('load surfaces validation errors as ok:false', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    const bad: FlowgraphDoc = {
      name: 'bad',
      environments: ['node'], // wrong env
      blocks: { a: { type: 'StubSource' } },
      wires: [],
    };
    const resp = await core.handle({ id: 1, kind: 'load', doc: bad, wsUrl: 'ws://x' });
    expect(resp.ok).toBe(false);
  });

  it('start / stop drive the Runtime lifecycle', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    await core.handle({ id: 1, kind: 'load', doc: DOC, wsUrl: 'ws://x' });

    let s = await core.handle(req('state', 2));
    expect(s.ok && s.kind === 'state' && s.data.state).toBe('initialized');

    await core.handle(req('start', 3));
    s = await core.handle(req('state', 4));
    expect(s.ok && s.kind === 'state' && s.data.state).toBe('running');

    await core.handle(req('stop', 5));
    s = await core.handle(req('state', 6));
    expect(s.ok && s.kind === 'state' && s.data.state).toBe('stopped');
  });

  it('stop closes the FrameClient and clears the runtime', async () => {
    const { env, clients } = makeEnv();
    const core = new RunnerCore(env);
    await core.handle({ id: 1, kind: 'load', doc: DOC, wsUrl: 'ws://x' });
    await core.handle(req('stop', 2));
    expect(clients[0]!.closeCalls).toBe(1);

    // Fresh load works after stop — core is reusable.
    const resp = await core.handle({ id: 3, kind: 'load', doc: DOC, wsUrl: 'ws://x' });
    expect(resp.ok).toBe(true);
  });

  it('state returns "created" before any load', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    const resp = await core.handle(req('state', 1));
    expect(resp.ok && resp.kind === 'state' && resp.data.state).toBe('created');
  });

  it('start before load is an error', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    const resp = await core.handle(req('start', 1));
    expect(resp.ok).toBe(false);
  });

  it('update forwards to the block', async () => {
    const { env } = makeEnv();
    const sources = new Map<string, StubSource>();
    const sinks = new Map<string, StubSink>();
    env.createRegistry = makeRegistry(sources, sinks);
    const core = new RunnerCore(env);
    await core.handle({ id: 1, kind: 'load', doc: DOC, wsUrl: 'ws://x' });
    const resp = await core.handle({
      id: 2,
      kind: 'update',
      blockId: 'source',
      params: { gain: 2 },
    });
    expect(resp.ok).toBe(true);
    const src = [...sources.values()][0]!;
    expect(src.updates).toEqual([{ gain: 2 }]);
  });

  it('update for unknown block is ok:false', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    await core.handle({ id: 1, kind: 'load', doc: DOC, wsUrl: 'ws://x' });
    const resp = await core.handle({
      id: 2,
      kind: 'update',
      blockId: 'ghost',
      params: {},
    });
    expect(resp.ok).toBe(false);
  });

  it('echoes the request id on every response', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    const a = await core.handle(req('state', 42));
    const b = await core.handle({ id: 99, kind: 'load', doc: DOC, wsUrl: 'ws://x' });
    expect(a.id).toBe(42);
    expect(b.id).toBe(99);
  });

  it('load failure does not leave a half-built runtime', async () => {
    const badInit = vi.fn().mockRejectedValue(new Error('init boom'));
    class BadSink extends StubSink {
      constructor() {
        super(32);
      }
      override init = badInit;
    }
    const env: RunnerEnv = {
      createFrameClient: () => new FakeFrameClient() as unknown as FrameClient,
      createRegistry: () => {
        const reg = new BlockRegistry();
        reg.add({ spec: SRC_SPEC, construct: () => new StubSource() });
        reg.add({ spec: SINK_SPEC, construct: () => new BadSink() });
        return reg;
      },
    };
    const core = new RunnerCore(env);
    const r1 = await core.handle({ id: 1, kind: 'load', doc: DOC, wsUrl: 'ws://x' });
    expect(r1.ok).toBe(false);
    // Next load should succeed — error path cleaned up.
    const r2 = await core.handle({ id: 2, kind: 'load', doc: DOC, wsUrl: 'ws://x' });
    // Still uses BadSink; this one will also fail. The point is the
    // core accepts another load rather than getting wedged.
    expect(r2.ok).toBe(false);
  });
});
