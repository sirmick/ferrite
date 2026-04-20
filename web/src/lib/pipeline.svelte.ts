// Singleton pipeline store — owns the live preset, source config, and
// the FrameClient feeding `/ws/preset`. The server holds exactly one
// pipeline; this store mirrors its lifecycle (status + start/stop) and
// exposes the flowgraph/source docs plus a handful of derived display
// axes the visualizers read.
//
// Lifecycle model: `init()` fetches the preset + source + status,
// opens the WebSocket, and leaves the pipeline in whatever state the
// server is in (the CLI may have auto-started it). `start()` / `stop()`
// flip the server-side state and surface the new status. The WebSocket
// stays open across start/stop cycles — a subscriber connected while
// stopped picks up frames the moment the pipeline spins up.

import type { FlowgraphDoc } from '$lib/flowgraph';
import { ApiError, wsUrlFor } from '$lib/api/errors';
import { fetchFlowgraph, patchFlowgraph, type ReconfigureResponse } from '$lib/api/flowgraph';
import { fetchSource, patchSource, type SourceConfig } from '$lib/api/source';
import {
  fetchPipelineStatus,
  startPipeline,
  stopPipeline,
  type PipelineStatus,
} from '$lib/api/pipeline';
import { FrameClient, type ClientStatus } from '$lib/ws/client';
import { initFrameDecoder } from '$lib/ws/frame';
import { logs } from '$lib/logs/store.svelte';

export type PipelinePhase = 'idle' | 'loading' | 'ready' | 'busy' | 'error';

class PipelineStore {
  phase = $state<PipelinePhase>('idle');
  status = $state<PipelineStatus>('stopped');
  wsStatus = $state<ClientStatus | 'idle'>('idle');
  errorMessage = $state<string | null>(null);

  flowgraph = $state<FlowgraphDoc | null>(null);
  source = $state<SourceConfig | null>(null);

  /** Shared WebSocket client feeding `/ws/preset`. Always open while
   *  the store is alive — callers multiplex by stream id. */
  client = $state<FrameClient | undefined>(undefined);

  /**
   * Load the initial state from the server and open the WS client.
   * Safe to call once on app mount. Idempotent — a second call is a
   * no-op if the client is already open.
   */
  async init(): Promise<void> {
    if (this.client) return;
    this.phase = 'loading';
    this.errorMessage = null;
    try {
      await initFrameDecoder();
      const [fg, src, st] = await Promise.all([
        fetchFlowgraph(),
        fetchSource(),
        fetchPipelineStatus(),
      ]);
      this.flowgraph = fg;
      this.source = src;
      this.status = st;
      this.client = new FrameClient({
        url: wsUrlFor('/ws/preset'),
        onStatus: (s) => {
          this.wsStatus = s;
          logs.push('client', 'info', `ws ${s}`);
        },
        onDecodeError: (err) => {
          this.errorMessage = `decode error: ${err.message}`;
          logs.push('client', 'error', `ws decode: ${err.message}`);
        },
      });
      this.phase = 'ready';
      logs.push('client', 'info', `pipeline init: status=${st}, source=${src.type}`);
    } catch (err) {
      this.phase = 'error';
      this.errorMessage = err instanceof Error ? err.message : String(err);
      logs.push('client', 'error', `pipeline init failed: ${this.errorMessage}`);
    }
  }

  async start(): Promise<void> {
    await this.withBusy(async () => {
      this.status = await startPipeline();
    }, 'start');
  }

  async stop(): Promise<void> {
    await this.withBusy(async () => {
      this.status = await stopPipeline();
    }, 'stop');
  }

  /** Patch the source config. If the pipeline is running, the server
   *  reconfigures it in place. Updates the local mirror on success. */
  async patchSource(next: SourceConfig): Promise<ReconfigureResponse | null> {
    return this.withBusy(async () => {
      const resp = await patchSource(next);
      this.source = next;
      return resp;
    }, 'patch source');
  }

  /** Patch just the source's `params` object, preserving `type`. Shape
   *  merges over the current `params`. */
  async patchSourceParams(params: Record<string, unknown>): Promise<ReconfigureResponse | null> {
    const current = this.source;
    if (!current) return null;
    const next: SourceConfig = {
      type: current.type,
      params: { ...current.params, ...params },
    };
    return this.patchSource(next);
  }

  async patchFlowgraph(doc: FlowgraphDoc): Promise<ReconfigureResponse | null> {
    return this.withBusy(async () => {
      const resp = await patchFlowgraph(doc);
      this.flowgraph = doc;
      return resp;
    }, 'patch flowgraph');
  }

  /** Close the WebSocket. Call on unmount; does NOT stop the server
   *  pipeline (the server-side lifecycle is the user's to drive). */
  teardown(): void {
    const c = this.client;
    this.client = undefined;
    this.wsStatus = 'idle';
    if (c) c.close();
  }

  private async withBusy<T>(fn: () => Promise<T>, label: string): Promise<T | null> {
    const prev = this.phase;
    this.phase = 'busy';
    this.errorMessage = null;
    try {
      const out = await fn();
      this.phase = 'ready';
      return out;
    } catch (err) {
      this.phase = 'error';
      const msg =
        err instanceof ApiError ? err.message : err instanceof Error ? err.message : String(err);
      this.errorMessage = msg;
      logs.push('client', 'error', `${label} failed: ${msg}`);
      // Leave the previous mirror in place — caller decides what to
      // show. Return null so the type stays Promise<T | null>.
      void prev;
      return null;
    }
  }
}

export const pipeline = new PipelineStore();

/**
 * Compatibility surface for viz components that haven't migrated to
 * the preset-first world yet. Exposes just enough of the legacy
 * `SessionState` shape (center freq, sample rate) to unblock tuning
 * while the FFT display axes move to the flowgraph in a follow-up.
 */
export interface PipelineAxes {
  center_freq_hz: number;
  sample_rate_hz: number;
}

export function currentAxes(store = pipeline): PipelineAxes | null {
  const p = store.source?.params;
  if (!p) return null;
  const center = typeof p.center_freq_hz === 'number' ? p.center_freq_hz : null;
  const rate = typeof p.sample_rate_hz === 'number' ? p.sample_rate_hz : null;
  if (center === null || rate === null) return null;
  return { center_freq_hz: center, sample_rate_hz: rate };
}
