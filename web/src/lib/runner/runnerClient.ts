// FlowgraphRunner — main-thread client around the runner Worker.
//
// Wraps postMessage in a promise-per-request API. Requests are
// correlated with responses by auto-incrementing `id`, so multiple
// in-flight calls can overlap without tripping over each other.
// The `WorkerLike` shape is the minimum subset of `Worker` we rely
// on — tests pass a fake; production passes `new Worker(...)`.

import type { FlowgraphDoc } from '../flowgraph.js';

import type {
  LoadResult,
  ReconfigureResult,
  RunnerRequest,
  RunnerResponse,
  RuntimeState,
} from './protocol.js';

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
    /** Unified-transport loopback: browser-side decoder events the
     *  worker drained, to be injected into the main-thread
     *  `FrameClient` on `streamId` (same channel pattern as `onDiag`,
     *  so test doubles that ignore it stay unaffected). */
    private readonly onEvents?: (streamId: number, lines: string[]) => void,
    /** One-shot: the negotiated input sample rate of a
     *  `VoiceTranscribe` block, forwarded to the matching
     *  transcription Worker so it can resample to 16 kHz. Same
     *  optional-callback pattern as `onEvents`. */
    private readonly onTranscribeRate?: (blockId: string, rateHz: number) => void,
    /** Browser-side flow-diagnostics snapshot (raw `DiagSnapshot`
     *  JSON), fed straight to the Flow store — a dedicated channel,
     *  never the log stream / server round-trip. Same optional-callback
     *  pattern as `onEvents`. */
    private readonly onFlowdiag?: (json: string) => void,
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

  /** Apply a params delta to a browser-side block. Wraps the worker's
   *  Rust `liveReconfigureBlock` via postMessage — hot-path apply for
   *  blocks that implement `apply_live_params`, block-rebuild fallback
   *  otherwise. No network hop (the browser runtime is already
   *  in-process in the worker). */
  async reconfigureBlock(
    blockId: string,
    delta: Record<string, unknown>,
  ): Promise<ReconfigureResult> {
    const resp = await this.send({
      id: this.nextId++,
      kind: 'reconfigureBlock',
      blockId,
      delta,
    });
    assertKind(resp, 'reconfigureBlock');
    return resp.data;
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

  private onResponse(
    resp: RunnerResponse | DiagMessage | EventsMessage | TranscribeRateMessage | FlowdiagMessage,
  ): void {
    if ((resp as FlowdiagMessage).kind === 'flowdiag') {
      this.onFlowdiag?.((resp as FlowdiagMessage).json);
      return;
    }
    // Unsolicited diagnostic log from the worker — threaded out
    // through an optional callback rather than a new protocol kind,
    // so test doubles that don't care about diags stay unaffected.
    if ((resp as DiagMessage).kind === 'diag') {
      this.onDiag?.((resp as DiagMessage).text);
      return;
    }
    if ((resp as EventsMessage).kind === 'events') {
      const m = resp as EventsMessage;
      this.onEvents?.(m.streamId, m.lines);
      return;
    }
    if ((resp as TranscribeRateMessage).kind === 'transcribeRate') {
      const m = resp as TranscribeRateMessage;
      this.onTranscribeRate?.(m.blockId, m.rateHz);
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

interface EventsMessage {
  readonly kind: 'events';
  readonly streamId: number;
  readonly lines: string[];
}

interface TranscribeRateMessage {
  readonly kind: 'transcribeRate';
  readonly blockId: string;
  readonly rateHz: number;
}

interface FlowdiagMessage {
  readonly kind: 'flowdiag';
  /** Raw `DiagSnapshot` JSON from the worker's `rt.diagSnapshot()`. */
  readonly json: string;
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
