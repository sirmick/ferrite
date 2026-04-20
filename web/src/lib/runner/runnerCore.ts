// RunnerCore — the worker-agnostic half of the flowgraph runner.
//
// Hosts one Rust-side `RuntimeHandle` at a time. The Worker glue
// (`runnerWorker.ts`) forwards each inbound message to `handle`, which
// dispatches against the lifecycle and produces one response envelope.
// Kept free of Worker globals so the dispatch logic is unit-testable
// under jsdom.
//
// Transport shape: the WebSocket is JS-owned (one multiplexed
// `FrameClient` per session). Each `WsIqSource` block in the graph is
// wired to its configured `stream_id` subscription, and every incoming
// IQ frame lands via `RuntimeHandle.pushIq`. Audio flows the other way
// — each `tick` is followed by `drainAudio` into a SAB-backed
// `AudioRingWriter` that an `AudioWorkletNode` on the main thread
// drains on the audio clock.

import type { FlowgraphDoc } from '../flowgraph.js';

import { AudioRingWriter } from '../audio/ringBuffer.js';
import type { FrameClient } from '../ws/client.js';
import { PayloadType } from '../ws/frame.js';
import { viewIqF32 } from '../ws/source.js';
import type { LoadResult, RunnerRequest, RunnerResponse, RuntimeState } from './protocol.js';
import type { RuntimeHandle } from './rustRuntime.js';

/**
 * Environment hooks injected by the Worker glue (or tests). Splits the
 * Rust-side construction concerns out of the core so a test run can
 * stitch in fakes without actually opening a WebSocket or parsing the
 * WASM bundle.
 */
export interface RunnerEnv {
  createFrameClient(wsUrl: string): FrameClient;
  splitDoc(doc: FlowgraphDoc, env: 'browser'): Promise<FlowgraphDoc>;
  createRuntime(doc: FlowgraphDoc, env: 'browser'): Promise<RuntimeHandle>;
  /** Override the between-tick delay; production uses the default. */
  tickIntervalMs?: number;
}

const DEFAULT_TICK_MS = 10;
const DEFAULT_AUDIO_SINK_CAPACITY = 8192;

interface AudioBinding {
  readonly blockId: string;
  readonly writer: AudioRingWriter;
  readonly scratch: Float32Array;
}

interface LoadedState {
  rt: RuntimeHandle;
  client: FrameClient;
  subscribers: Array<() => void>;
  audio: AudioBinding[];
  tickTimer: ReturnType<typeof setTimeout> | null;
  running: boolean;
}

export class RunnerCore {
  private loaded: LoadedState | null = null;
  private lastState: RuntimeState = 'created';

  constructor(private readonly env: RunnerEnv) {}

  async handle(req: RunnerRequest): Promise<RunnerResponse> {
    try {
      switch (req.kind) {
        case 'load':
          return {
            id: req.id,
            ok: true,
            kind: 'load',
            data: await this.load(req.doc, req.wsUrl),
          };
        case 'start':
          this.doStart();
          return { id: req.id, ok: true, kind: 'start' };
        case 'stop':
          this.doStop();
          return { id: req.id, ok: true, kind: 'stop' };
        case 'state':
          return {
            id: req.id,
            ok: true,
            kind: 'state',
            data: { state: this.currentState() },
          };
      }
    } catch (err) {
      return { id: req.id, ok: false, error: errorMessage(err) };
    }
  }

  private currentState(): RuntimeState {
    if (!this.loaded) return this.lastState;
    return toProtocolState(this.loaded.rt.state);
  }

  private async load(doc: FlowgraphDoc, wsUrl: string): Promise<LoadResult> {
    if (this.loaded) {
      throw new Error('RunnerCore.load: already loaded — stop() first');
    }
    const splitDoc = await this.env.splitDoc(doc, 'browser');
    const client = this.env.createFrameClient(wsUrl);
    let rt: RuntimeHandle | null = null;
    try {
      rt = await this.env.createRuntime(splitDoc, 'browser');
      rt.init();
      const { subscribers, audio, audioSabs } = wireBlocks(splitDoc, client, rt);
      const blocks = Object.keys(splitDoc.blocks ?? {});
      this.loaded = {
        rt,
        client,
        subscribers,
        audio,
        tickTimer: null,
        running: false,
      };
      this.lastState = 'initialized';
      return { blocks, audioSabs };
    } catch (err) {
      rt?.free();
      client.close();
      throw err;
    }
  }

  private doStart(): void {
    const state = this.loaded ?? throwNoRuntime();
    state.rt.start();
    state.running = true;
    this.scheduleTick(state);
  }

  private scheduleTick(state: LoadedState): void {
    if (!state.running || state.tickTimer !== null) return;
    const interval = this.env.tickIntervalMs ?? DEFAULT_TICK_MS;
    state.tickTimer = setTimeout(() => {
      state.tickTimer = null;
      if (this.loaded !== state || !state.running) return;
      try {
        state.rt.tick();
        for (const a of state.audio) {
          const n = state.rt.drainAudio(a.blockId, a.scratch);
          if (n > 0) a.writer.write(a.scratch.subarray(0, n));
        }
      } catch {
        // Tick errors are isolated to this iteration — an explicit stop
        // will clear the loop. A noisier policy (surface via a Worker
        // message) lands with the diagnostics pane in M5.
      }
      this.scheduleTick(state);
    }, interval);
  }

  private doStop(): void {
    const state = this.loaded;
    if (!state) return;
    state.running = false;
    if (state.tickTimer !== null) {
      clearTimeout(state.tickTimer);
      state.tickTimer = null;
    }
    for (const unsub of state.subscribers) unsub();
    try {
      state.rt.stop();
    } finally {
      state.rt.free();
      state.client.close();
    }
    this.loaded = null;
    this.lastState = 'stopped';
  }
}

function wireBlocks(
  doc: FlowgraphDoc,
  client: FrameClient,
  rt: RuntimeHandle,
): {
  subscribers: Array<() => void>;
  audio: AudioBinding[];
  audioSabs: Record<string, SharedArrayBuffer>;
} {
  const subscribers: Array<() => void> = [];
  const audio: AudioBinding[] = [];
  const audioSabs: Record<string, SharedArrayBuffer> = {};
  for (const [blockId, raw] of Object.entries(doc.blocks ?? {})) {
    const block = raw as { type?: string; params?: Record<string, unknown> };
    if (block.type === 'WsIqSource') {
      const streamId = Number(block.params?.stream_id ?? 0);
      const unsub = client.subscribe(streamId, (frame) => {
        if (frame.header.payloadType !== PayloadType.IqF32) return;
        rt.pushIq(blockId, viewIqF32(frame.payload));
      });
      subscribers.push(unsub);
    } else if (block.type === 'AudioSink') {
      const capacity = nextPow2(
        Number(block.params?.buffer_samples ?? DEFAULT_AUDIO_SINK_CAPACITY),
      );
      const writer = AudioRingWriter.create(capacity);
      audio.push({ blockId, writer, scratch: new Float32Array(capacity) });
      audioSabs[blockId] = writer.sab;
    }
  }
  return { subscribers, audio, audioSabs };
}

function toProtocolState(s: string): RuntimeState {
  return s.toLowerCase() as RuntimeState;
}

function nextPow2(n: number): number {
  if (!Number.isFinite(n) || n <= 1) return 1;
  let p = 1;
  while (p < n) p <<= 1;
  return p;
}

function throwNoRuntime(): never {
  throw new Error('RunnerCore: no flowgraph loaded');
}

function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  return String(err);
}
