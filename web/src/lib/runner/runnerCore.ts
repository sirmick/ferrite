// RunnerCore — the worker-agnostic half of the flowgraph runner.
//
// Hosts one Rust-side `RuntimeHandle` at a time. The Worker glue
// (`runnerWorker.ts`) forwards each inbound message to `handle`, which
// dispatches against the lifecycle and produces one response envelope.
// Kept free of Worker globals so the dispatch logic is unit-testable
// under jsdom.
//
// Transport shape: the WebSocket is JS-owned (one multiplexed
// `FrameClient` per session). Each `WsBridgeRx` block in the graph is
// wired to its configured `stream_id` subscription, and every incoming
// IQ frame lands via `RuntimeHandle.pushIq`. Audio flows the other way
// — each `tick` is followed by `drainAudio` into a SAB-backed
// `AudioRingWriter` that an `AudioWorkletNode` on the main thread
// drains on the audio clock.

import type { FlowgraphDoc } from '../flowgraph.js';

import { AudioRingWriter } from '../audio/ringBuffer.js';
import type { FrameClient } from '../ws/client.js';
import { PayloadType } from '../ws/frame.js';
import { viewF32, viewIqF32 } from '../ws/source.js';
import type {
  LoadResult,
  ReconfigureChange,
  ReconfigureResult,
  RunnerRequest,
  RunnerResponse,
  RuntimeState,
} from './protocol.js';
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
/** Tap-ring capacity for the transcription SAB. Sized to bridge a full
 *  whisper inference stall: the transcribe Worker is single-threaded
 *  and `whisper_full` is a blocking call (seconds for a multi-second
 *  utterance), during which it can't drain this ring while the runner
 *  keeps writing it. 262 144 samples ≈ 5.5 s @ 48 kHz / ~16 s @ 16 kHz
 *  — comfortably longer than one inference, so continuous speech isn't
 *  shredded by ring overflow. ~1 MB SAB; cheap for the robustness. */
const DEFAULT_TRANSCRIBE_CAPACITY = 262144;

interface AudioBinding {
  readonly blockId: string;
  readonly writer: AudioRingWriter;
  readonly scratch: Float32Array;
  /** Samples drained from this block since the last diag report. */
  drained: number;
  /** Cumulative `AudioSink::dropped_samples` snapshot taken at the last
   *  diag report, so the next tick can print the delta. */
  droppedBaseline: bigint;
}

/** A `VoiceTranscribe` tap binding. Same drain-into-SAB shape as
 *  `AudioBinding`, but the SAB is consumed by a transcription Worker
 *  (Silero VAD + whisper.cpp) instead of an `AudioWorkletNode`. The
 *  source sample rate is posted out-of-band once, the first tick it's
 *  known, so the Worker can resample to whisper's 16 kHz mono. */
interface TranscribeBinding {
  readonly blockId: string;
  readonly writer: AudioRingWriter;
  readonly scratch: Float32Array;
  /** True once the negotiated input rate has been posted to the main
   *  thread — it's static for the life of the graph, so post once. */
  rateSent: boolean;
}

/** Per-bridge inbound counter. Incremented from the WS subscription
 *  callback each time a frame lands. */
interface BridgeCounter {
  readonly blockId: string;
  readonly streamId: number;
  samples: number;
  /** Cumulative `WsBridgeRx::dropped_samples` snapshot at the last
   *  diag report — printed as a delta so ring overflow is visible. */
  droppedBaseline: bigint;
  /** Last rate advertised on a frame — used to skip redundant
   *  `setBridgeRxRate` calls across the wasm boundary. */
  lastRateHz: number;
}

/** A browser-side `ui:<name>` Events sink. env_split names it
 *  `__ui_<name>_<stream_id>`; the runner drains it after each tick and
 *  loopbacks the JSON lines to the main thread keyed by `streamId`
 *  (which equals the node half's allocation, so stores attach the same
 *  way regardless of which side the decoder ran on). */
interface UiEventsBinding {
  readonly blockId: string;
  readonly name: string;
  readonly streamId: number;
}

interface LoadedState {
  rt: RuntimeHandle;
  client: FrameClient;
  subscribers: Array<() => void>;
  audio: AudioBinding[];
  transcribe: TranscribeBinding[];
  uiEvents: UiEventsBinding[];
  tickTimer: ReturnType<typeof setTimeout> | null;
  running: boolean;
  bridges: BridgeCounter[];
  ticks: number;
  diagTimer: ReturnType<typeof setInterval> | null;
}

export class RunnerCore {
  private loaded: LoadedState | null = null;
  private lastState: RuntimeState = 'created';
  // Most recent tick-error message that's been surfaced via postDiag.
  // Persistent failures get logged once per distinct message instead of
  // silently spinning at tick rate.
  private lastTickErrorMsg: string | null = null;

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
        case 'reconfigureBlock':
          return {
            id: req.id,
            ok: true,
            kind: 'reconfigureBlock',
            data: this.doReconfigureBlock(req.blockId, req.delta),
          };
      }
    } catch (err) {
      return { id: req.id, ok: false, error: errorMessage(err) };
    }
  }

  /** Hand a params delta to `RuntimeHandle.reconfigureBlock`, which
   *  (as of the wasm.rs wrapper) tries the block's `apply_live_params`
   *  hot path first and falls back to a block-scoped rebuild on
   *  Ok(false). Same decision the server's non-src `apply_block_params`
   *  makes — keeps the two halves consistent and lets browser-placed
   *  blocks update in place without a full load/start cycle.
   *
   *  The JSON response body (the applied changes) is returned so the
   *  main thread can loop browser-placed block params back into the
   *  UI mirror — the server REST `/api/pipeline/blocks` never sees a
   *  browser block's live value. */
  private doReconfigureBlock(blockId: string, delta: Record<string, unknown>): ReconfigureResult {
    const state = this.loaded ?? throwNoRuntime();
    const json = state.rt.reconfigureBlock(blockId, JSON.stringify(delta));
    try {
      const parsed = JSON.parse(json) as { changes?: ReconfigureChange[] };
      return { changes: parsed.changes ?? [] };
    } catch {
      return { changes: [] };
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
      const { subscribers, audio, audioSabs, transcribe, transcribeSabs, bridges, uiEvents } =
        wireBlocks(splitDoc, client, rt);
      const blocks = Object.keys(splitDoc.blocks ?? {});
      this.loaded = {
        rt,
        client,
        subscribers,
        audio,
        transcribe,
        uiEvents,
        tickTimer: null,
        running: false,
        bridges,
        ticks: 0,
        diagTimer: null,
      };
      this.lastState = 'initialized';
      return {
        blocks,
        audioSabs,
        transcribeSabs,
        uiSinks: uiEvents.map((u) => ({ name: u.name, stream_id: u.streamId })),
      };
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
    this.startDiagTimer(state);
  }

  private scheduleTick(state: LoadedState): void {
    if (!state.running || state.tickTimer !== null) return;
    const interval = this.env.tickIntervalMs ?? DEFAULT_TICK_MS;
    state.tickTimer = setTimeout(() => {
      state.tickTimer = null;
      if (this.loaded !== state || !state.running) return;
      try {
        state.rt.tick();
        state.ticks += 1;
        for (const a of state.audio) {
          const n = state.rt.drainAudio(a.blockId, a.scratch);
          if (n > 0) a.writer.write(a.scratch.subarray(0, n));
          a.drained += n;
        }
        // VoiceTranscribe tap → SAB the transcription Worker reads.
        // Empty when mode is `off` (the block never fills its ring).
        for (const t of state.transcribe) {
          const n = state.rt.drainTranscribe(t.blockId, t.scratch);
          if (n > 0) t.writer.write(t.scratch.subarray(0, n));
          if (!t.rateSent) {
            const rateHz = state.rt.transcribeInputRate(t.blockId);
            if (rateHz > 0) {
              postTranscribeRate(t.blockId, rateHz);
              t.rateSent = true;
            }
          }
        }
        // Loopback browser-side decoder events to the main thread on
        // the same stream_id the node WS path would have used — the
        // unified transport, so stores/views don't care which side
        // the decoder ran on.
        for (const ue of state.uiEvents) {
          const lines = state.rt.drainEvents(ue.blockId);
          if (lines.length > 0) postEvents(ue.streamId, lines as string[]);
        }
      } catch (err) {
        // Tick errors are isolated to this iteration so the loop keeps
        // running, but persistent failures need to be visible — a
        // silent loop running forever is the wrong default. Dedup by
        // message so a stuck-block doesn't spam at tick rate.
        const msg = err instanceof Error ? err.message : String(err);
        if (msg !== this.lastTickErrorMsg) {
          this.lastTickErrorMsg = msg;
          postDiag(`runner: tick error: ${msg}`);
        }
      }
      this.scheduleTick(state);
    }, interval);
  }

  /** Emit a one-line diag to the main thread every second. Summarises
   *  what the tick loop is actually doing so the page can tell where
   *  the audio chain is stalled without opening devtools on the worker. */
  private startDiagTimer(state: LoadedState): void {
    if (state.diagTimer !== null) return;
    state.diagTimer = setInterval(() => {
      if (this.loaded !== state || !state.running) return;
      // Re-run rate propagation so any WsBridgeRx that received a
      // frame with a new `sampleRateHz` this second updates its
      // downstream resamp/demod. Cheap for rate-stable graphs.
      try {
        state.rt.refreshRates();
      } catch {
        /* best-effort; don't block the diag loop on a rebuild error */
      }
      const bridgeSummary = state.bridges
        .map((b) => {
          const total = safeBigint(() => state.rt.iqDroppedSamples(b.blockId));
          const drops = total - b.droppedBaseline;
          b.droppedBaseline = total;
          const dropTag = drops > 0n ? ` drop=${drops}` : '';
          return `${b.blockId}(#${b.streamId})=${b.samples}${dropTag}`;
        })
        .join(' ');
      const audioSummary = state.audio
        .map((a) => {
          const total = safeBigint(() => state.rt.audioDroppedSamples(a.blockId));
          const drops = total - a.droppedBaseline;
          a.droppedBaseline = total;
          const dropTag = drops > 0n ? ` drop=${drops}` : '';
          return `${a.blockId}=${a.drained}${dropTag}`;
        })
        .join(' ');
      // A human log line (not flowdiag — that's the dedicated channel
      // below). When the graph is entirely node-side (the common case)
      // there are no browser bridges/audio, so this would be a constant
      // `rx[] audio[]` once a second — pure noise. Only emit it when
      // there's something bound on the browser side to report on.
      if (state.bridges.length > 0 || state.audio.length > 0) {
        const text = `runner: runner 1s: ticks=${state.ticks} rx[${bridgeSummary}] audio[${audioSummary}]`;
        postDiag(text);
      }
      try {
        const json = state.rt.diagSnapshot();
        // Skip the empty `{"blocks":[]}` snapshot a node-only graph
        // produces every tick — nothing for the Flow view to show.
        let hasBlocks = false;
        try {
          hasBlocks = (JSON.parse(json) as { blocks?: unknown[] }).blocks?.length ? true : false;
        } catch {
          hasBlocks = true; // unparseable — forward so the anomaly is visible
        }
        if (hasBlocks) {
          // Dedicated flowdiag channel → Flow store directly. NOT the
          // log stream: no LogPanel clutter, no server round-trip,
          // independent of `RUST_LOG`.
          postFlowdiag(json);
        }
      } catch {
        /* best-effort */
      }
      state.ticks = 0;
      for (const b of state.bridges) b.samples = 0;
      for (const a of state.audio) a.drained = 0;
    }, 1000);
  }

  private doStop(): void {
    const state = this.loaded;
    if (!state) return;
    state.running = false;
    if (state.tickTimer !== null) {
      clearTimeout(state.tickTimer);
      state.tickTimer = null;
    }
    if (state.diagTimer !== null) {
      clearInterval(state.diagTimer);
      state.diagTimer = null;
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

/** Unsolicited "log from the worker" — the main thread's
 *  `FlowgraphRunner` recognises `kind: 'diag'` and forwards the text
 *  to its `onDiag` callback. A no-op outside a Worker context (tests). */
function postDiag(text: string): void {
  const g = globalThis as {
    postMessage?: (msg: unknown) => void;
  };
  if (typeof g.postMessage === 'function') {
    g.postMessage({ kind: 'diag', text });
  }
}

/** Out-of-band flow-diagnostics snapshot (same channel shape as
 *  `postDiag`): `FlowgraphRunner` recognises `kind: 'flowdiag'` and
 *  forwards the raw `DiagSnapshot` JSON to its `onFlowdiag` callback,
 *  which feeds the Flow store directly — never the log stream. No-op
 *  outside a Worker (tests). */
function postFlowdiag(json: string): void {
  const g = globalThis as {
    postMessage?: (msg: unknown) => void;
  };
  if (typeof g.postMessage === 'function') {
    g.postMessage({ kind: 'flowdiag', json });
  }
}

/** Out-of-band event loopback (same channel shape as `postDiag`):
 *  `FlowgraphRunner` recognises `kind: 'events'` and forwards to its
 *  `onEvents` callback, which injects them into the main-thread
 *  `FrameClient` on `streamId`. No-op outside a Worker (tests). */
function postEvents(streamId: number, lines: string[]): void {
  const g = globalThis as {
    postMessage?: (msg: unknown) => void;
  };
  if (typeof g.postMessage === 'function') {
    g.postMessage({ kind: 'events', streamId, lines });
  }
}

/** One-shot out-of-band notify (same channel shape as `postDiag`):
 *  the negotiated input sample rate of a `VoiceTranscribe` block, sent
 *  the first tick it's known. `FlowgraphRunner` forwards it to the
 *  matching transcription Worker so it can resample to 16 kHz. No-op
 *  outside a Worker (tests). */
function postTranscribeRate(blockId: string, rateHz: number): void {
  const g = globalThis as {
    postMessage?: (msg: unknown) => void;
  };
  if (typeof g.postMessage === 'function') {
    g.postMessage({ kind: 'transcribeRate', blockId, rateHz });
  }
}

/** `__ui_<name>_<stream_id>` — env_split's id for a browser-side
 *  `ui:<name>` Events sink. The name may contain underscores; the
 *  stream_id is the trailing run of digits. */
const UI_EVENTS_ID = /^__ui_(.+)_(\d+)$/;

function wireBlocks(
  doc: FlowgraphDoc,
  client: FrameClient,
  rt: RuntimeHandle,
): {
  subscribers: Array<() => void>;
  audio: AudioBinding[];
  audioSabs: Record<string, SharedArrayBuffer>;
  transcribe: TranscribeBinding[];
  transcribeSabs: Record<string, SharedArrayBuffer>;
  bridges: BridgeCounter[];
  uiEvents: UiEventsBinding[];
} {
  const subscribers: Array<() => void> = [];
  const audio: AudioBinding[] = [];
  const audioSabs: Record<string, SharedArrayBuffer> = {};
  const transcribe: TranscribeBinding[] = [];
  const transcribeSabs: Record<string, SharedArrayBuffer> = {};
  const bridges: BridgeCounter[] = [];
  const uiEvents: UiEventsBinding[] = [];
  for (const [blockId, raw] of Object.entries(doc.blocks ?? {})) {
    const block = raw as { type?: string; params?: Record<string, unknown> };
    const uiMatch = block.type === 'EventsSink' ? UI_EVENTS_ID.exec(blockId) : null;
    if (uiMatch) {
      // Browser-side ui: Events sink (env_split). Drained per-tick in
      // the tick loop and loopbacked to the main thread.
      uiEvents.push({ blockId, name: uiMatch[1], streamId: Number(uiMatch[2]) });
      continue;
    }
    if (block.type === 'WsBridgeRx') {
      const streamId = Number(block.params?.stream_id ?? 0);
      const counter: BridgeCounter = {
        blockId,
        streamId,
        samples: 0,
        droppedBaseline: 0n,
        lastRateHz: 0,
      };
      bridges.push(counter);
      const unsub = client.subscribe(streamId, (frame) => {
        if (frame.header.payloadType !== PayloadType.IqF32) return;
        const samples = viewIqF32(frame.payload);
        counter.samples += samples.length / 2; // complex pairs → IQ samples
        // When the producer advertises its runtime output rate on the
        // frame, forward it to the Rx block so the next
        // `refreshRates()` sweep propagates the live rate to
        // downstream resamp/demod blocks. Skip the wasm roundtrip if
        // the rate hasn't changed.
        const rate = frame.header.sampleRateHz;
        if (rate > 0 && rate !== counter.lastRateHz) {
          counter.lastRateHz = rate;
          try {
            rt.setBridgeRxRate(blockId, rate);
          } catch {
            /* block type/id guaranteed by this code path; swallow */
          }
        }
        rt.pushIq(blockId, samples);
      });
      subscribers.push(unsub);
    } else if (block.type === 'WsBridgeRxF32') {
      // Real-audio bridge — same shape as WsBridgeRx but samples are
      // mono `f32` (not interleaved pairs), routed to `pushF32` so the
      // runtime's `WsBridgeRxF32` block fills its `AudioRing`.
      const streamId = Number(block.params?.stream_id ?? 0);
      const counter: BridgeCounter = {
        blockId,
        streamId,
        samples: 0,
        droppedBaseline: 0n,
        lastRateHz: 0,
      };
      bridges.push(counter);
      const unsub = client.subscribe(streamId, (frame) => {
        if (frame.header.payloadType !== PayloadType.F32) return;
        const samples = viewF32(frame.payload);
        counter.samples += samples.length;
        const rate = frame.header.sampleRateHz;
        if (rate > 0 && rate !== counter.lastRateHz) {
          counter.lastRateHz = rate;
          try {
            rt.setBridgeRxRate(blockId, rate);
          } catch {
            /* block type/id guaranteed by this code path; swallow */
          }
        }
        rt.pushF32(blockId, samples);
      });
      subscribers.push(unsub);
    } else if (block.type === 'AudioSink') {
      const capacity = nextPow2(
        Number(block.params?.buffer_samples ?? DEFAULT_AUDIO_SINK_CAPACITY),
      );
      const writer = AudioRingWriter.create(capacity);
      audio.push({
        blockId,
        writer,
        scratch: new Float32Array(capacity),
        drained: 0,
        droppedBaseline: 0n,
      });
      audioSabs[blockId] = writer.sab;
    } else if (block.type === 'VoiceTranscribe') {
      const capacity = nextPow2(
        Number(block.params?.buffer_samples ?? DEFAULT_TRANSCRIBE_CAPACITY),
      );
      const writer = AudioRingWriter.create(capacity);
      transcribe.push({
        blockId,
        writer,
        scratch: new Float32Array(capacity),
        rateSent: false,
      });
      transcribeSabs[blockId] = writer.sab;
    }
  }
  return { subscribers, audio, audioSabs, transcribe, transcribeSabs, bridges, uiEvents };
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

/** Wraps a wasm drop-counter call so a transient throw (block went
 *  away mid-reconfigure) doesn't kill the diag loop for the session. */
function safeBigint(fn: () => bigint): bigint {
  try {
    return fn();
  } catch {
    return 0n;
  }
}
