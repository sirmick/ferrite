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
import {
  fetchFlowgraph,
  patchFlowgraph,
  type ReconfigureResponse,
  type SoapyReadback,
} from '$lib/api/flowgraph';
import { fetchSource, patchSource, type SourceConfig } from '$lib/api/source';
import {
  fetchPipelineStatus,
  startPipeline,
  stopPipeline,
  type PipelineStatus,
} from '$lib/api/pipeline';
import { fetchUiSinks, type UiSink } from '$lib/api/uiSinks';
import { fetchPipelineBlocks, patchBlockParams, type PipelineBlock } from '$lib/api/pipelineBlocks';
import { fetchPresets, loadPreset, type PresetEntry } from '$lib/api/presets';
import { fetchSourceCapabilities, type SourceCapabilitiesResponse } from '$lib/api/sourceCaps';
import { defaultsFor, toSourceConfig } from '$lib/controls/optionsModel';
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
  /** Server-allocated stream_ids for every `ui:<name>` sink, keyed by
   *  name. Populated on `init()` and re-fetched on preset/source patch. */
  uiSinks = $state<Record<string, UiSink>>({});

  /** Composed-preset blocks with their spec and current params, keyed by
   *  block id. Populated on `init()` and re-fetched after any patch.
   *  Feeds the receiver panel and the generic `<BlockParams>` component. */
  blocks = $state<Record<string, PipelineBlock>>({});

  /** Browseable preset files the server can load. Populated on `init()`;
   *  stable for the session (the presets dir isn't watched). */
  presets = $state<PresetEntry[]>([]);

  /** Hardware/software capabilities of the currently-active source.
   *  Refreshed whenever `source` changes. Hardware sources carry the
   *  full `DeviceCapabilities`; software sources just their type_name. */
  sourceCaps = $state<SourceCapabilitiesResponse | null>(null);

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
      const [fg, src, st, sinks, blocks, presets, caps] = await Promise.all([
        fetchFlowgraph(),
        fetchSource(),
        fetchPipelineStatus(),
        fetchUiSinks(),
        fetchPipelineBlocks(),
        fetchPresets(),
        fetchSourceCapabilities(),
      ]);
      this.flowgraph = fg;
      this.source = src;
      this.status = st;
      this.uiSinks = indexByName(sinks);
      this.blocks = indexById(blocks);
      this.presets = presets;
      this.sourceCaps = caps;
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
    // Reset-on-Start: the user's job is to pick the centre frequency,
    // the system's job is to pick a working rate/BW/gain/antenna. Any
    // live edits they've made via InputControls (e.g. accidentally
    // parking sample_rate at the SDRplay cliff) are deliberately
    // forgotten; the only thing carried across a start is the current
    // centre frequency, clamped to the device's advertised range.
    //
    // No-op when the source is not a SoapySDR device (e.g. SineSource)
    // or when the capability probe hasn't populated yet — in those
    // cases the stored params go through verbatim.
    await this.applySensibleSourceDefaults();
    await this.withBusy(async () => {
      this.status = await startPipeline();
    }, 'start');
  }

  /** Reset the source config to `defaultsFor(caps)`, carrying over the
   *  current centre frequency clamped into the advertised range. Called
   *  from `start()`; exposed as a method so a future "reset now"
   *  button can reuse it. */
  private async applySensibleSourceDefaults(): Promise<void> {
    const caps = this.sourceCaps?.kind === 'hardware' ? this.sourceCaps.capabilities : null;
    // Extra noisy so you can see it in devtools Console without the
    // in-page log filter.
    console.warn(
      '[reset-on-start]',
      'caps-kind=' + (this.sourceCaps?.kind ?? 'undefined'),
      'source-type=' + (this.source?.type ?? 'undefined'),
    );
    logs.push(
      'client',
      'info',
      `applySensibleSourceDefaults: caps=${caps ? 'hardware' : 'none'} type=${this.source?.type ?? 'none'}`,
    );
    if (!caps || this.source?.type !== 'SoapySource') {
      console.warn('[reset-on-start] bailing — guard not satisfied');
      return;
    }

    const baseline = defaultsFor(caps);
    if (!baseline) return;

    // Carry the user's current centre frequency, clamped to the advertised
    // range. If they swapped devices and the old freq is out of band,
    // fall back to the baseline midpoint rather than picking an
    // unreachable value.
    const current = this.source.params as Record<string, unknown>;
    const currentFreq = typeof current.center_freq_hz === 'number' ? current.center_freq_hz : null;
    const { min, max } = baseline.freq_range;
    const keptFreq =
      currentFreq !== null && Number.isFinite(currentFreq)
        ? Math.min(max, Math.max(min, currentFreq))
        : baseline.center_freq_hz;

    const reset = toSourceConfig(caps, { ...baseline, center_freq_hz: keptFreq });
    console.warn('[reset-on-start] sending PATCH', reset.params);
    logs.push(
      'client',
      'info',
      `applySensibleSourceDefaults: resetting to rate=${reset.params.sample_rate_hz} bw=${reset.params.bandwidth_hz ?? 'auto'} freq=${reset.params.center_freq_hz}`,
    );
    await this.patchSource(reset);
  }

  async stop(): Promise<void> {
    await this.withBusy(async () => {
      this.status = await stopPipeline();
    }, 'stop');
  }

  /** Patch the source config. If the pipeline is running, the server
   *  reconfigures it in place. Updates the local mirror on success.
   *
   *  When the server returns a `source_readback` (SoapySource on a
   *  running pipeline), the readback values overwrite the optimistic
   *  ones in `source.params`. Drivers silently clamp BW to the ladder,
   *  AGC varies IFGR under the user's gain knob, and tune-step rounding
   *  shifts the centre freq by a few kHz — without reconciliation the
   *  UI would keep showing whatever the user last wrote. */
  async patchSource(next: SourceConfig): Promise<ReconfigureResponse | null> {
    return this.withBusy(async () => {
      const resp = await patchSource(next);
      this.source = applyReadback(next, resp.source_readback);
      await this.refreshComposed();
      this.sourceCaps = await fetchSourceCapabilities();
      return resp;
    }, 'patch source');
  }

  /** Route one DSP knob change to the server's generic block-params
   *  endpoint. `id` is the block id inside the composed preset; `key`
   *  names the param on that block. The server merges the delta into
   *  either `SourceConfig.params` (for `src`) or
   *  `FlowgraphDoc.blocks[id].params` and hot-reconfigures.
   *
   *  This is the single entry point the receiver panel and
   *  `<BlockParams>` component use — every slider, toggle, and select
   *  calls through here. When the browser-half runtime lands the
   *  dispatcher will branch on `PipelineBlock.placement` and call
   *  `RuntimeHandle.reconfigureBlock` directly for browser-side blocks;
   *  today every placement routes through REST. */
  async setBlockParam(
    id: string,
    key: string,
    value: unknown,
  ): Promise<ReconfigureResponse | null> {
    return this.withBusy(async () => {
      const resp = await patchBlockParams(id, { [key]: value });
      await this.refreshComposed();
      return resp;
    }, `set ${id}.${key}`);
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
      await this.refreshComposed();
      return resp;
    }, 'patch flowgraph');
  }

  /** Load preset `name` from the server-side presets dir and swap it
   *  in. The server returns the full reconfigure plan alongside the
   *  doc's canonical name; we re-fetch the flowgraph + composed state
   *  so local mirrors match the server's merged view. */
  async loadPreset(name: string): Promise<ReconfigureResponse | null> {
    return this.withBusy(async () => {
      const resp = await loadPreset(name);
      this.flowgraph = await fetchFlowgraph();
      await this.refreshComposed();
      return resp.reconfigure;
    }, `load preset ${name}`);
  }

  /** Re-read derivatives of the composed preset (ui_sinks + blocks) in
   *  parallel. Called after every patch so local state stays coherent
   *  with what the server is actually running. */
  private async refreshComposed(): Promise<void> {
    const [sinks, blocks] = await Promise.all([fetchUiSinks(), fetchPipelineBlocks()]);
    this.uiSinks = indexByName(sinks);
    this.blocks = indexById(blocks);
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

/**
 * Merge the server's post-apply readback into the optimistic source
 * config. Only keys the server actually returned are overwritten —
 * undefined fields mean "driver didn't report this, keep the optimistic
 * value." Leaves `type` alone; readback is always about params.
 */
function applyReadback(next: SourceConfig, readback: SoapyReadback | undefined): SourceConfig {
  if (!readback) return next;
  const merged: Record<string, unknown> = { ...next.params };
  for (const [k, v] of Object.entries(readback)) {
    if (v !== undefined) merged[k] = v;
  }
  return { type: next.type, params: merged };
}

function indexByName(sinks: UiSink[]): Record<string, UiSink> {
  const out: Record<string, UiSink> = {};
  for (const s of sinks) out[s.name] = s;
  return out;
}

function indexById(blocks: PipelineBlock[]): Record<string, PipelineBlock> {
  const out: Record<string, PipelineBlock> = {};
  for (const b of blocks) out[b.id] = b;
  return out;
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
  const p = (store.source?.params ?? {}) as Record<string, unknown>;
  // Fall back to the flowgraph's `src` block when the current
  // SourceConfig is empty — lets the Nixie/VFO show up immediately on
  // preset load, before the user has opened the Source dialog and
  // picked hardware. The preset declares the target tune in its `src`
  // params; once the user picks a device the readback overwrites
  // pipeline.source and that takes precedence.
  const srcBlock = store.flowgraph?.blocks?.src?.params as Record<string, unknown> | undefined;
  const pick = (key: 'center_freq_hz' | 'sample_rate_hz'): number | null => {
    const v = p[key] ?? srcBlock?.[key];
    return typeof v === 'number' ? v : null;
  };
  const center = pick('center_freq_hz');
  const rate = pick('sample_rate_hz');
  if (center === null || rate === null) return null;
  return { center_freq_hz: center, sample_rate_hz: rate };
}
