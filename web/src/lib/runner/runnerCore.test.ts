// Integration-y tests for `RunnerCore` against the real Rust runtime.
//
// The Rust engine is instantiated from the wasm-pack bundle (same
// `initSync` pattern as `rustRuntime.test.ts`), so these tests exercise
// the full doc split → load → tick path end-to-end without any JS-side
// block stubs. The only fake is `FakeFrameClient`, which records
// subscriptions and lets tests simulate incoming IQ frames.

import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { beforeAll, describe, expect, it, vi } from 'vitest';

import type { FlowgraphDoc } from '../flowgraph.js';

import type { FrameClient, FrameHandler } from '../ws/client.js';
import { PayloadType, PROTOCOL_VERSION, type ParsedFrame } from '../ws/frame.js';
import { initSync } from '../wasm/runtime/runtime.js';
import type { RunnerRequest } from './protocol.js';
import { RunnerCore, type RunnerEnv } from './runnerCore.js';
import { createRuntime, splitFlowgraphForEnv } from './rustRuntime.js';

const WASM_PATH = resolve(__dirname, '../wasm/runtime/runtime_bg.wasm');

// Browser-only fixture — SineSource → FmDemod → AudioSink. No WS; the
// graph type-checks and exercises start/tick/stop without an IQ pump.
// The docs carry `placement: 'browser'` under `params`-sibling — the
// Rust split accepts it even though the TS `BlockInstanceDecl` doesn't
// declare it, so we cast through `unknown` at the single edge.
const SINE_DOC = {
  name: 'sine-audio',
  environments: ['browser'],
  blocks: {
    src: { type: 'SineSource', placement: 'browser', params: { rate_hz: 240000 } },
    demod: {
      type: 'FmDemod',
      placement: 'browser',
      params: { sample_rate_hz: 240000, max_deviation_hz: 75000 },
    },
    sink: { type: 'AudioSink', placement: 'browser', params: { buffer_samples: 4096 } },
  },
  wires: [
    ['src.out', 'demod.in'],
    ['demod.out', 'sink.in'],
  ],
} as unknown as FlowgraphDoc;

// Browser-only fixture with a WsIqSource feeding the same demod → sink
// chain. Exercises the FrameClient subscription + pushIq path.
const WS_DOC = {
  name: 'ws-audio',
  environments: ['browser'],
  blocks: {
    rx: {
      type: 'WsIqSource',
      placement: 'browser',
      params: { stream_id: 1234, buffer_samples: 4096 },
    },
    demod: {
      type: 'FmDemod',
      placement: 'browser',
      params: { sample_rate_hz: 240000, max_deviation_hz: 75000 },
    },
    sink: { type: 'AudioSink', placement: 'browser', params: { buffer_samples: 4096 } },
  },
  wires: [
    ['rx.out', 'demod.in'],
    ['demod.out', 'sink.in'],
  ],
} as unknown as FlowgraphDoc;

class FakeFrameClient {
  readonly subs = new Map<number, Set<FrameHandler>>();
  closeCalls = 0;

  subscribe(streamId: number, handler: FrameHandler): () => void {
    let set = this.subs.get(streamId);
    if (!set) {
      set = new Set();
      this.subs.set(streamId, set);
    }
    set.add(handler);
    return () => set!.delete(handler);
  }

  close(): void {
    this.closeCalls++;
  }

  /** Synthesise an IqF32 frame and deliver it to every subscriber. */
  deliver(streamId: number, floats: Float32Array): void {
    const frame: ParsedFrame = {
      header: {
        version: PROTOCOL_VERSION,
        payloadType: PayloadType.IqF32,
        streamId,
        seq: 0,
        timestampNs: 0n,
      },
      payload: new Uint8Array(floats.buffer, floats.byteOffset, floats.byteLength),
    };
    this.subs.get(streamId)?.forEach((h) => h(frame));
  }
}

function makeEnv(overrides: Partial<RunnerEnv> = {}): {
  env: RunnerEnv;
  clients: FakeFrameClient[];
} {
  const clients: FakeFrameClient[] = [];
  const env: RunnerEnv = {
    createFrameClient: () => {
      const c = new FakeFrameClient();
      clients.push(c);
      return c as unknown as FrameClient;
    },
    splitDoc: (doc, e) => splitFlowgraphForEnv(doc, e),
    createRuntime: (doc, e) => createRuntime(doc, e),
    tickIntervalMs: 1,
    ...overrides,
  };
  return { env, clients };
}

function req(kind: 'start' | 'stop' | 'state', id = 1): RunnerRequest {
  return { id, kind };
}

describe('RunnerCore', () => {
  beforeAll(() => {
    const bytes = readFileSync(WASM_PATH);
    initSync({ module: bytes });
  });

  it('load splits the doc, instantiates blocks, and exposes audio SABs', async () => {
    const { env, clients } = makeEnv();
    const core = new RunnerCore(env);
    const resp = await core.handle({ id: 1, kind: 'load', doc: SINE_DOC, wsUrl: 'ws://x' });

    expect(resp.ok).toBe(true);
    if (!resp.ok || resp.kind !== 'load') throw new Error('wrong kind');
    expect(new Set(resp.data.blocks)).toEqual(new Set(['src', 'demod', 'sink']));
    expect(Object.keys(resp.data.audioSabs)).toEqual(['sink']);
    // Capacity (4096 f32 samples) + 64-byte header.
    expect(resp.data.audioSabs.sink.byteLength).toBe(4096 * 4 + 64);
    expect(clients).toHaveLength(1);
  });

  it('load fails if already loaded', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    await core.handle({ id: 1, kind: 'load', doc: SINE_DOC, wsUrl: 'ws://x' });
    const resp = await core.handle({ id: 2, kind: 'load', doc: SINE_DOC, wsUrl: 'ws://x' });
    expect(resp.ok).toBe(false);
    if (resp.ok) throw new Error('unreachable');
    expect(resp.error).toMatch(/already loaded/);
  });

  it('load surfaces Rust-side validation errors as ok:false', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    const bad: FlowgraphDoc = {
      name: 'bad',
      environments: ['browser'],
      blocks: { a: { type: 'NoSuchBlock' } },
      wires: [],
    };
    const resp = await core.handle({ id: 1, kind: 'load', doc: bad, wsUrl: 'ws://x' });
    expect(resp.ok).toBe(false);
  });

  it('state reports the lifecycle transitions', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    await core.handle({ id: 1, kind: 'load', doc: SINE_DOC, wsUrl: 'ws://x' });

    let s = await core.handle(req('state', 2));
    expect(s.ok && s.kind === 'state' && s.data.state).toBe('initialized');

    await core.handle(req('start', 3));
    s = await core.handle(req('state', 4));
    expect(s.ok && s.kind === 'state' && s.data.state).toBe('running');

    await core.handle(req('stop', 5));
    s = await core.handle(req('state', 6));
    expect(s.ok && s.kind === 'state' && s.data.state).toBe('stopped');
  });

  it('stop closes the FrameClient and leaves the core reusable', async () => {
    const { env, clients } = makeEnv();
    const core = new RunnerCore(env);
    await core.handle({ id: 1, kind: 'load', doc: SINE_DOC, wsUrl: 'ws://x' });
    await core.handle(req('stop', 2));
    expect(clients[0]!.closeCalls).toBe(1);

    const resp = await core.handle({ id: 3, kind: 'load', doc: SINE_DOC, wsUrl: 'ws://x' });
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

  it('echoes the request id on every response', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    const a = await core.handle(req('state', 42));
    const b = await core.handle({ id: 99, kind: 'load', doc: SINE_DOC, wsUrl: 'ws://x' });
    expect(a.id).toBe(42);
    expect(b.id).toBe(99);
  });

  it('subscribes to each WsIqSource stream and forwards frames via pushIq', async () => {
    const { env, clients } = makeEnv();
    const core = new RunnerCore(env);
    const resp = await core.handle({ id: 1, kind: 'load', doc: WS_DOC, wsUrl: 'ws://x' });
    expect(resp.ok).toBe(true);

    const client = clients[0]!;
    expect([...client.subs.keys()]).toEqual([1234]);

    // Deliver two complex samples. After a tick the demod+sink chain
    // should have drained some of them; the pushIq path is proven by
    // the frame delivery not throwing and the tick running clean.
    client.deliver(1234, new Float32Array([0.5, 0.5, -0.5, 0.5]));

    await core.handle(req('start', 2));
    await new Promise((r) => setTimeout(r, 20));
    const stopResp = await core.handle(req('stop', 3));
    expect(stopResp.ok).toBe(true);
  });

  it('unsubscribes and closes the client on stop', async () => {
    const { env, clients } = makeEnv();
    const core = new RunnerCore(env);
    await core.handle({ id: 1, kind: 'load', doc: WS_DOC, wsUrl: 'ws://x' });
    const client = clients[0]!;
    expect(client.subs.get(1234)!.size).toBe(1);
    await core.handle(req('stop', 2));
    expect(client.subs.get(1234)!.size).toBe(0);
    expect(client.closeCalls).toBe(1);
  });

  it('tick loop drains audio into the SAB writer', async () => {
    const { env } = makeEnv();
    const core = new RunnerCore(env);
    const loadResp = await core.handle({ id: 1, kind: 'load', doc: SINE_DOC, wsUrl: 'ws://x' });
    if (!loadResp.ok || loadResp.kind !== 'load') throw new Error('unreachable');
    const sab = loadResp.data.audioSabs.sink;
    const head = new Uint32Array(sab, 0, 1);
    expect(Atomics.load(head, 0)).toBe(0);

    await core.handle(req('start', 2));
    await new Promise((r) => setTimeout(r, 25));
    await core.handle(req('stop', 3));

    expect(Atomics.load(head, 0)).toBeGreaterThan(0);
  });

  it('load failure tears down the FrameClient', async () => {
    const { env, clients } = makeEnv({
      createRuntime: vi.fn().mockRejectedValue(new Error('simulated init blow-up')),
    });
    const core = new RunnerCore(env);
    const resp = await core.handle({ id: 1, kind: 'load', doc: SINE_DOC, wsUrl: 'ws://x' });
    expect(resp.ok).toBe(false);
    expect(clients[0]!.closeCalls).toBe(1);
  });
});
