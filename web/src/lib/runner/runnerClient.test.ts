import { describe, expect, it } from 'vitest';

import type { FlowgraphDoc } from '@ferrite/flowgraph-runtime/types';

import type { RunnerRequest, RunnerResponse } from './protocol.js';
import { FlowgraphRunner, type WorkerLike } from './runnerClient.js';

// ---------------------------------------------------------------------------
// FakeWorker — records every postMessage and exposes `reply()` so tests
// can synthesise responses. Emits them via `onmessage` asynchronously
// (microtask) to match how the platform schedules them.
// ---------------------------------------------------------------------------

class FakeWorker implements WorkerLike {
  readonly posted: RunnerRequest[] = [];
  onmessage: ((ev: MessageEvent<RunnerResponse>) => void) | null = null;
  terminated = 0;

  postMessage(message: RunnerRequest): void {
    this.posted.push(message);
  }

  terminate(): void {
    this.terminated++;
  }

  reply(resp: RunnerResponse): void {
    this.onmessage?.({ data: resp } as MessageEvent<RunnerResponse>);
  }
}

const DOC: FlowgraphDoc = {
  name: 'test',
  environments: ['browser'],
  blocks: {},
  wires: [],
};

describe('FlowgraphRunner', () => {
  it('load sends a load request and resolves with the result', async () => {
    const w = new FakeWorker();
    const runner = new FlowgraphRunner(w);
    const pending = runner.load(DOC, 'ws://x');

    expect(w.posted).toHaveLength(1);
    const req = w.posted[0]!;
    expect(req.kind).toBe('load');
    if (req.kind !== 'load') throw new Error('unreachable');
    expect(req.doc).toBe(DOC);
    expect(req.wsUrl).toBe('ws://x');

    const sab = new SharedArrayBuffer(32);
    w.reply({
      id: req.id,
      ok: true,
      kind: 'load',
      data: { blocks: ['a'], audioSabs: { a: sab } },
    });
    const result = await pending;
    expect(result.blocks).toEqual(['a']);
    expect(result.audioSabs.a).toBe(sab);
  });

  it('auto-increments request ids across calls', async () => {
    const w = new FakeWorker();
    const runner = new FlowgraphRunner(w);
    void runner.start();
    void runner.stop();
    void runner.state();
    expect(w.posted.map((r) => r.id)).toEqual([1, 2, 3]);
  });

  it('matches responses to their request ids (out-of-order ok)', async () => {
    const w = new FakeWorker();
    const runner = new FlowgraphRunner(w);

    const s1 = runner.state();
    const s2 = runner.state();
    const [r1, r2] = w.posted;

    // Reply in reverse order.
    w.reply({ id: r2!.id, ok: true, kind: 'state', data: { state: 'running' } });
    w.reply({ id: r1!.id, ok: true, kind: 'state', data: { state: 'initialized' } });

    expect(await s1).toBe('initialized');
    expect(await s2).toBe('running');
  });

  it('rejects when the Worker returns ok:false', async () => {
    const w = new FakeWorker();
    const runner = new FlowgraphRunner(w);
    const pending = runner.start();
    w.reply({ id: w.posted[0]!.id, ok: false, error: 'no graph' });
    await expect(pending).rejects.toThrow(/no graph/);
  });

  it('rejects when the kind of the response disagrees with the request', async () => {
    const w = new FakeWorker();
    const runner = new FlowgraphRunner(w);
    const pending = runner.start();
    // Server misbehaves — sends a state response to a start request.
    w.reply({
      id: w.posted[0]!.id,
      ok: true,
      kind: 'state',
      data: { state: 'running' },
    });
    await expect(pending).rejects.toThrow(/unexpected response kind/);
  });

  it('terminate() calls through and rejects pending requests', async () => {
    const w = new FakeWorker();
    const runner = new FlowgraphRunner(w);
    const pending = runner.start();
    runner.terminate();
    expect(w.terminated).toBe(1);
    await expect(pending).rejects.toThrow(/terminated/);
  });

  it('ignores replies with unknown ids', async () => {
    const w = new FakeWorker();
    new FlowgraphRunner(w);
    // Should not throw even though nothing is pending for id 999.
    w.reply({ id: 999, ok: true, kind: 'start' });
  });
});
