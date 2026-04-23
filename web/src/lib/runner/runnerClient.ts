// FlowgraphRunner — main-thread client around the runner Worker.
//
// Wraps postMessage in a promise-per-request API. Requests are
// correlated with responses by auto-incrementing `id`, so multiple
// in-flight calls can overlap without tripping over each other.
// The `WorkerLike` shape is the minimum subset of `Worker` we rely
// on — tests pass a fake; production passes `new Worker(...)`.

import type { FlowgraphDoc } from '../flowgraph.js';

import type { LoadResult, RunnerRequest, RunnerResponse, RuntimeState } from './protocol.js';

/**
 * The shape we require of the underlying transport. `Worker` satisfies
 * it; so does a `MessageChannel` port and hand-rolled test doubles.
 */
export interface WorkerLike {
  postMessage(message: RunnerRequest): void;
  onmessage: ((ev: MessageEvent<RunnerResponse>) => void) | null;
  terminate?(): void;
}

interface Pending {
  resolve: (value: RunnerResponse) => void;
  reject: (err: Error) => void;
}

export class FlowgraphRunner {
  private nextId = 1;
  private readonly pending = new Map<number, Pending>();

  constructor(
    private readonly worker: WorkerLike,
    private readonly onDiag?: (text: string) => void,
  ) {
    this.worker.onmessage = (ev) => this.onResponse(ev.data);
  }

  async load(doc: FlowgraphDoc, wsUrl: string): Promise<LoadResult> {
    const resp = await this.send({ id: this.nextId++, kind: 'load', doc, wsUrl });
    assertKind(resp, 'load');
    return resp.data;
  }

  async start(): Promise<void> {
    const resp = await this.send({ id: this.nextId++, kind: 'start' });
    assertKind(resp, 'start');
  }

  async stop(): Promise<void> {
    const resp = await this.send({ id: this.nextId++, kind: 'stop' });
    assertKind(resp, 'stop');
  }

  async state(): Promise<RuntimeState> {
    const resp = await this.send({ id: this.nextId++, kind: 'state' });
    assertKind(resp, 'state');
    return resp.data.state;
  }

  terminate(): void {
    this.worker.terminate?.();
    const leftover = new Error('FlowgraphRunner: terminated with pending requests');
    for (const { reject } of this.pending.values()) reject(leftover);
    this.pending.clear();
  }

  private send(req: RunnerRequest): Promise<RunnerResponse> {
    return new Promise<RunnerResponse>((resolve, reject) => {
      this.pending.set(req.id, { resolve, reject });
      this.worker.postMessage(req);
    });
  }

  private onResponse(resp: RunnerResponse | DiagMessage): void {
    // Unsolicited diagnostic log from the worker — threaded out
    // through an optional callback rather than a new protocol kind,
    // so test doubles that don't care about diags stay unaffected.
    if ((resp as DiagMessage).kind === 'diag') {
      this.onDiag?.((resp as DiagMessage).text);
      return;
    }
    const r = resp as RunnerResponse;
    const slot = this.pending.get(r.id);
    if (!slot) return;
    this.pending.delete(r.id);
    if (r.ok) {
      slot.resolve(r);
    } else {
      slot.reject(new Error(r.error));
    }
  }
}

interface DiagMessage {
  readonly kind: 'diag';
  readonly text: string;
}

type SuccessResp = Extract<RunnerResponse, { ok: true }>;

function assertKind<K extends SuccessResp['kind']>(
  resp: RunnerResponse,
  expected: K,
): asserts resp is Extract<SuccessResp, { kind: K }> {
  if (!resp.ok) {
    throw new Error(resp.error);
  }
  if (resp.kind !== expected) {
    throw new Error(
      `FlowgraphRunner: unexpected response kind ${JSON.stringify(resp.kind)}, expected ${JSON.stringify(expected)}`,
    );
  }
}
