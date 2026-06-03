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
import { fetchSource, patchSource, tune, type SourceConfig } from '$lib/api/source';
import { untrack } from 'svelte';
import { decoders } from '$lib/decoders/store.svelte';
import { tuneOffsetRatioFor } from '$lib/controls/optionsModel';
import {
  fetchPipelineStatus,
  startPipeline,
  stopPipeline,
  type PipelineStatus,
} from '$lib/api/pipeline';
import { fetchUiSinks, type UiSink } from '$lib/api/uiSinks';
import { fetchPipelineBlocks, patchBlockParams, type PipelineBlock } from '$lib/api/pipelineBlocks';
import { fetchPresets, loadPreset, type PresetEntry } from '$lib/api/presets';
import { DEFAULT_PROFILE, fetchProfile, patchProfile, type Profile } from '$lib/api/profile';
import { replaceState } from '$app/navigation';
import { fetchSourceCapabilities, type SourceCapabilitiesResponse } from '$lib/api/sourceCaps';
import { defaultsFor, toSourceConfig } from '$lib/controls/optionsModel';
import { presetDocBySlug } from '$lib/presets/catalog';
import { clientControls } from '$lib/control/clientStore.svelte';
import { FrameClient, type ClientStatus } from '$lib/ws/client';
import { initFrameDecoder } from '$lib/ws/frame';
import { logs } from '$lib/logs/store.svelte';
import { browserRuntime } from '$lib/runner/browserRuntime.svelte';

export type PipelinePhase = 'idle' | 'loading' | 'ready' | 'busy' | 'error';

class PipelineStore {
  phase = $state<PipelinePhase>('idle');
  status = $state<PipelineStatus>('stopped');
  wsStatus = $state<ClientStatus | 'idle'>('idle');
  errorMessage = $state<string | null>(null);

  flowgraph = $state<FlowgraphDoc | null>(null);
  source = $state<SourceConfig | null>(null);
  /** Node-advertised `ui:<name>` sinks (`/api/ui-sinks`), keyed by
   *  name. Populated on `init()` and re-fetched on preset/source
   *  patch. Browser-side sinks (decoder swapped browser) aren't here —
   *  see `uiSinks`. */
  private nodeUiSinks = $state<Record<string, UiSink>>({});

  /** All `ui:<name>` sinks the active preset exposes — node-advertised
   *  plus the browser-side ones the runner produced when a decoder is
   *  browser-placed. Views key off this; the unified transport means a
   *  given sink's `stream_id` is the same whichever side it ran on, so
   *  the store attaches identically. */
  uiSinks = $derived.by<Record<string, UiSink>>(() => {
    const merged: Record<string, UiSink> = { ...this.nodeUiSinks };
    for (const s of browserRuntime.uiSinks) {
      merged[s.name] = { name: s.name, stream_id: s.stream_id, payload_type: 'JsonEvent' };
    }
    return merged;
  });

  /** Server-composed blocks (spec + authored/node-live params) from
   *  `/api/pipeline/blocks`. Browser-placed blocks are present here too
   *  (list_blocks walks the composed doc) but their *values* are the
   *  authored ones — the server never sees a browser block's live
   *  param. See `blocks`. */
  private nodeBlocks = $state<Record<string, PipelineBlock>>({});

  /** All composed blocks for the UI — server specs/values with
   *  browser-placed blocks' live param overrides overlaid. The single
   *  loopback point for browser-side `reconfigureBlock` writes (NR
   *  preset, any browser BlockParams edit), mirroring the node+browser
   *  `uiSinks` merge so `<BlockParams>` reflects them without a server
   *  round-trip. */
  blocks = $derived.by<Record<string, PipelineBlock>>(() => {
    const overrides = browserRuntime.paramOverrides;
    const out: Record<string, PipelineBlock> = {};
    for (const [id, b] of Object.entries(this.nodeBlocks)) {
      const ov = overrides[id];
      out[id] = ov ? { ...b, values: { ...(b.values ?? {}), ...ov } } : b;
    }
    return out;
  });

  /** Browseable preset files the server can load. Populated on `init()`;
   *  stable for the session (the presets dir isn't watched). */
  presets = $state<PresetEntry[]>([]);

  /** Runtime profile applied before the env-split pass: `audio` gates
   *  the audio chain, `audio_split` slides the node↔browser cut along
   *  the audio spine (browser | balanced | server). Hydrated from server
   *  state on `init()` (so a reload preserves the user's choice — server
   *  is the source of truth, not localStorage). */
  profile = $state<Profile>({ ...DEFAULT_PROFILE });

  /** Hardware/software capabilities of the currently-active source.
   *  Refreshed whenever `source` changes. Hardware sources carry the
   *  full `DeviceCapabilities`; software sources just their type_name. */
  sourceCaps = $state<SourceCapabilitiesResponse | null>(null);

  /** Shared WebSocket client feeding `/ws/preset`. Always open while
   *  the store is alive — callers multiplex by stream id. */
  client = $state<FrameClient | undefined>(undefined);

  /** Teardown for the readback→source sync effect. The live driver
   *  readback (AGC-driven gain motion, etc.) now arrives via the store
   *  mirror (`readback` kind, folded by the server diag tick at 4 Hz)
   *  instead of a dedicated poll; an effect merges each new readback into
   *  `source.params` so the UI controls and the AI prompt reflect what
   *  the radio is actually doing, not the last set-point. */
  private readbackDispose: (() => void) | undefined;

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
      const [fg, src, st, sinks, blocks, presets, caps, profile] = await Promise.all([
        fetchFlowgraph(),
        fetchSource(),
        fetchPipelineStatus(),
        fetchUiSinks(),
        fetchPipelineBlocks(),
        fetchPresets(),
        fetchSourceCapabilities(),
        fetchProfile(),
      ]);
      this.flowgraph = fg;
      this.source = src;
      this.status = st;
      this.nodeUiSinks = indexByName(sinks);
      this.nodeBlocks = indexById(blocks);
      this.presets = presets;
      this.sourceCaps = caps;
      this.profile = profile;
      // Hydrate from URL params if present (`?audio=off` /
      // `?split=browser|balanced|server`); a one-shot apply on first
      // load so a shared link lands the user in the right state without
      // an extra round trip.
      await this.applyProfileFromUrl();
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
      // Unified transport: browser-side decoder events (worker drained
      // the env_split EventsSink) loopback into this same FrameClient
      // on the env_split-allocated stream_id, so the ft8/wspr/fldigi
      // stores consume node-WS and browser-local decode identically.
      browserRuntime.onDecodedEvents = (streamId, lines) => {
        this.client?.injectLocal(streamId, new TextEncoder().encode(lines.join('\n')));
      };
      // Stamp transcript segments with the live VFO frequency so the
      // transcript doubles as a band log. Injected (not imported by
      // browserRuntime) to avoid a tuning→dispatch→browserRuntime cycle.
      browserRuntime.vfoHzProvider = () => {
        const center = (this.source?.params as Record<string, unknown> | undefined)?.center_freq_hz;
        if (typeof center !== 'number' || !Number.isFinite(center)) return null;
        return center + (this.currentVfoOffset() ?? 0);
      };
      this.phase = 'ready';
      this.startReadbackSync();
      logs.push('client', 'info', `pipeline init: status=${st}, source=${src.type}`);
    } catch (err) {
      this.phase = 'error';
      this.errorMessage = err instanceof Error ? err.message : String(err);
      logs.push('client', 'error', `pipeline init failed: ${this.errorMessage}`);
    }
  }

  /** Merge live driver readback from the store mirror (`readback` kind,
   *  Replace policy) into `source.params`. Idempotent — a second call
   *  leaves the existing effect alone. Replaces the old 1 Hz
   *  `/api/source/readback` poll: the server diag tick folds readback
   *  into the store, the mirror replicates it over WS, and this effect
   *  fires on each change. Stopped via `stopReadbackPoll`; in practice
   *  the singleton store never tears down. */
  private startReadbackSync(): void {
    if (this.readbackDispose) return;
    this.readbackDispose = $effect.root(() => {
      $effect(() => {
        // Track only the mirror's readback record; the merge below reads
        // `this.source` under `untrack` so writing it can't self-trigger.
        const rb = decoders.kind('readback').current['current']?.data as SoapyReadback | undefined;
        if (!rb) return;
        untrack(() => {
          if (!this.source) return;
          const next = applyReadback(this.source, rb);
          // Only assign when something actually changed — keeps reactivity
          // graphs (AI prompt rebuild, control rerender) from firing on a
          // stable signal.
          if (!sourceEquals(this.source, next)) this.source = next;
        });
      });
    });
  }

  /** Tear down the readback sync effect. Production tears down on tab
   *  close; kept for symmetry / test cleanup. */
  stopReadbackPoll(): void {
    this.readbackDispose?.();
    this.readbackDispose = undefined;
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
    logs.push(
      'client',
      'info',
      `applySensibleSourceDefaults: caps=${caps ? 'hardware' : 'none'} type=${this.source?.type ?? 'none'}`,
    );
    if (!caps || this.source?.type !== 'SoapySource') return;

    const baseline = defaultsFor(caps);
    if (!baseline) return;

    // Carry the user's current centre frequency through the source
    // restart unchanged — *iff* it's in the device's advertised range.
    // If it isn't (e.g. they swapped from an SDR that reaches 1 GHz
    // to one that caps at 30 MHz), fall back to the active preset's
    // src.center_freq_hz hint so they land inside that mode's band
    // rather than at the device's edge. If the preset doesn't define
    // a hint either, finally fall back to the caps' baseline midpoint.
    const current = this.source.params as Record<string, unknown>;
    const currentFreq = typeof current.center_freq_hz === 'number' ? current.center_freq_hz : null;
    const { min, max } = baseline.freq_range;
    const inRange =
      currentFreq !== null &&
      Number.isFinite(currentFreq) &&
      currentFreq >= min &&
      currentFreq <= max;
    const presetHint = this.flowgraph?.name ? presetSrcFreqHint(this.flowgraph.name) : null;
    const keptFreq = inRange
      ? (currentFreq as number)
      : presetHint !== null && presetHint >= min && presetHint <= max
        ? presetHint
        : baseline.center_freq_hz;

    const reset = toSourceConfig(caps, { ...baseline, center_freq_hz: keptFreq });
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
      // `VoiceTranscribe` keeps its `model_id` / `prompt` knobs as
      // block params (canonical declared interface, surfaced in
      // SettingsPanel), but the actual whisper engine lives in a JS
      // Worker — fan the change there so the operator's pick takes
      // effect immediately. Future server-side transcriber will read
      // the block params directly and this hop disappears.
      const b = this.blocks[id];
      if (b?.type_name === 'VoiceTranscribe') {
        if (key === 'model_id') browserRuntime.setTranscribeModel(value as string);
        else if (key === 'prompt') browserRuntime.setTranscribePrompt(value as string);
      }
      return resp;
    }, `set ${id}.${key}`);
  }

  /** Apply a partial source-params delta.
   *
   *  Sends ONLY the changed keys to the `src` block-params route; the
   *  server merges them against its own authoritative `SourceConfig`.
   *  This is deliberately *not* a client-side `{ ...this.source.params,
   *  ...params }` merge + full `PATCH /api/source`: when the AI (or any
   *  other writer) mutates the source concurrently, the browser's
   *  `this.source` snapshot goes stale, and a client-merged full blob
   *  would silently re-assert stale unrelated keys (e.g. flipping `agc`
   *  / resetting `gain_db` while the user only toggled `dc`). Sending
   *  the minimal delta keeps each control isolated.
   *
   *  After the write we re-read the authoritative config from the server
   *  and overlay the driver readback, so the local mirror reflects real
   *  state (including concurrent changes) rather than an optimistic
   *  guess. SourceDialog's whole-config replace still uses
   *  `patchSource`. */
  async patchSourceParams(params: Record<string, unknown>): Promise<ReconfigureResponse | null> {
    if (!this.source) return null;
    return this.withBusy(async () => {
      const resp = await patchBlockParams('src', params);
      const server = await fetchSource();
      this.source = applyReadback(server, resp.source_readback);
      await this.refreshComposed();
      this.sourceCaps = await fetchSourceCapabilities();
      return resp;
    }, 'patch source params');
  }

  /** Tuning intent — "listen at `freqHz`" (optionally span `spanHz`).
   *  POSTs `/api/tune`, where the server applies the per-driver
   *  DC-spike dodge and the keep-or-snap math against the active
   *  channelizer. Every VFO origin (Nixie commit, ▲▼ buttons, Up/Down
   *  keys, spectrum click — all funnel through `tuneVfoTo`) flows
   *  here, so the dodge is uniform regardless of who asked. Looks up
   *  `tune_offset_ratio` from the active driver's preset; 0 when the
   *  source is software or the driver has no entry (= "just tune"). */
  async tune(freqHz: number, spanHz?: number): Promise<ReconfigureResponse | null> {
    if (!Number.isFinite(freqHz)) return null;
    const caps = this.sourceCaps;
    const offsetRatio = caps?.kind === 'hardware' ? tuneOffsetRatioFor(caps.capabilities) : 0;
    return this.withBusy(async () => {
      const resp = await tune({ freq_hz: freqHz, span_hz: spanHz, offset_ratio: offsetRatio });
      // Reconcile the optimistic mirror with what actually landed —
      // /api/tune writes both src.center_freq_hz and chan.freq_shift_hz
      // server-side, so a fresh fetchSource + refreshComposed picks up
      // both at once. The source-readback overlay covers AGC etc.
      const server = await fetchSource();
      this.source = applyReadback(server, resp.source_readback);
      await this.refreshComposed();
      return resp;
    }, 'tune');
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
   *  so local mirrors match the server's merged view.
   *
   *  Tuning persistence: a preset is the demod chain ("what mode"),
   *  separate from frequency ("where am I tuned"). Centre freq is
   *  preserved on the server side automatically (`compose_source`
   *  merges live source params over the preset's `src` hints).
   *  VFO offset is *not* — the new preset's channelizer block
   *  replaces the old one wholesale, defaulting `freq_shift_hz` to
   *  whatever the new JSON says (typically 0). We capture the
   *  current VFO before the swap and re-apply it after, clamping
   *  to the new channelizer's range. If the old VFO would land
   *  outside that range, we fall back to the new preset's chan
   *  hint so the user lands at the preset's intended starting
   *  position rather than at a clipped edge. */
  async loadPreset(name: string): Promise<ReconfigureResponse | null> {
    return this.withBusy(async () => {
      const oldVfo = this.currentVfoOffset();
      const resp = await loadPreset(name);
      this.flowgraph = await fetchFlowgraph();
      await this.refreshComposed();
      this.restoreVfo(name, oldVfo);
      // Reset the workspace main pane to the wide spectrum on every
      // preset change so a stale "advanced" selection from a previous
      // preset doesn't surprise you on the next one (D-mainPane Q1).
      // Channel checkbox is preset-independent and stays sticky.
      clientControls.set('client.workspace.mainPane', 'wide');
      return resp.reconfigure;
    }, `load preset ${name}`);
  }

  /** Replace the active runtime profile and re-compose the current
   *  preset. The server-side PATCH already runs `patch_flowgraph` so
   *  the running pipeline picks up the new shape; we re-fetch the
   *  composed view to keep the local mirror in sync. Also writes the
   *  non-default fields back to URL params so the address bar reflects
   *  what's actually running and a refresh / shared link reproduces
   *  the same state. */
  async setProfile(profile: Profile): Promise<void> {
    await this.withBusy(async () => {
      this.profile = await patchProfile(profile);
      this.flowgraph = await fetchFlowgraph();
      await this.refreshComposed();
      this.writeProfileToUrl();
    }, 'set profile');
  }

  /** One-shot: read `?audio=off` / `?split=browser|balanced|server` from
   *  the current URL on first load and call [`setProfile`] when they
   *  differ from the server's idea. Lets a shared link land the user
   *  in the right state without a manual second click. */
  private async applyProfileFromUrl(): Promise<void> {
    if (typeof window === 'undefined') return;
    const params = new URLSearchParams(window.location.search);
    const next: Profile = { ...this.profile };
    let touched = false;
    if (params.has('audio')) {
      next.audio = params.get('audio') !== 'off';
      touched = true;
    }
    if (params.has('split')) {
      const v = params.get('split');
      if (v === 'browser' || v === 'balanced' || v === 'server') {
        next.audio_split = v;
        touched = true;
      }
    }
    if (touched && !profileEquals(this.profile, next)) {
      await this.setProfile(next);
    }
  }

  private writeProfileToUrl(): void {
    if (typeof window === 'undefined') return;
    const url = new URL(window.location.href);
    if (this.profile.audio) {
      url.searchParams.delete('audio');
    } else {
      url.searchParams.set('audio', 'off');
    }
    // Only the non-default split rides the URL (keeps shared links clean).
    if (this.profile.audio_split !== 'balanced') {
      url.searchParams.set('split', this.profile.audio_split);
    } else {
      url.searchParams.delete('split');
    }
    // SvelteKit owns the navigation history; its `replaceState`
    // wrapper updates the URL without invoking a route load. Calling
    // `window.history.replaceState` directly works but logs a runtime
    // warning about racing with the router.
    replaceState(url, {});
  }

  /** Read the channelizer-style VFO offset from the current `blocks`
   *  mirror. Looks for any block exposing `freq_shift_hz` — today
   *  that's the `Channelizer` block. Returns `null` when no such
   *  block is present (e.g. headless / decoder-only presets). */
  private currentVfoOffset(): number | null {
    for (const block of Object.values(this.blocks)) {
      const v = (block.values as Record<string, unknown> | null | undefined)?.freq_shift_hz;
      if (typeof v === 'number' && Number.isFinite(v)) return v;
    }
    return null;
  }

  /** After a preset swap, write the captured VFO back to the new
   *  channelizer block (if any). Clamped to the new channelizer's
   *  ±sample_rate/2 advertised range; out-of-range falls through
   *  to the new preset's `chan.freq_shift_hz` hint, then 0. */
  private async restoreVfo(slug: string, oldVfo: number | null): Promise<void> {
    if (oldVfo === null) return;
    // Locate the new VFO block (anything advertising freq_shift_hz).
    const target = Object.values(this.blocks).find((b) =>
      b.spec.params.some((p) => p.key === 'freq_shift_hz'),
    );
    if (!target) return; // preset doesn't have a VFO; nothing to restore
    const values = (target.values as Record<string, unknown> | null | undefined) ?? {};
    const rate = numberOrNull(values.input_rate_hz) ?? numberOrNull(values.output_rate_hz);
    const half = rate !== null && rate > 0 ? rate / 2 : Infinity;
    let target_shift: number;
    if (Math.abs(oldVfo) <= half) {
      target_shift = oldVfo;
    } else {
      // Out of range — use the preset hint, then 0.
      const hint = presetVfoHint(slug);
      target_shift = hint ?? 0;
    }
    if (Math.abs(target_shift - (numberOrNull(values.freq_shift_hz) ?? 0)) < 0.5) {
      return; // already there; skip the patch round-trip
    }
    try {
      await patchBlockParams(target.id, { freq_shift_hz: target_shift });
      await this.refreshComposed();
    } catch (err) {
      logs.push(
        'client',
        'warn',
        `restoreVfo: patching ${target.id}.freq_shift_hz=${target_shift} failed: ${
          err instanceof Error ? err.message : String(err)
        }`,
      );
    }
  }

  /** Re-read derivatives of the composed preset (ui_sinks + blocks) in
   *  parallel. Called after every patch so local state stays coherent
   *  with what the server is actually running. */
  private async refreshComposed(): Promise<void> {
    const [sinks, blocks] = await Promise.all([fetchUiSinks(), fetchPipelineBlocks()]);
    this.nodeUiSinks = indexByName(sinks);
    this.nodeBlocks = indexById(blocks);
  }

  /** Pull every server mirror (source + flowgraph + status + composed +
   *  caps) in parallel. Used when an out-of-band mutator (ferrite-ctl,
   *  the AI sidecar) changed state via REST without going through this
   *  store, so the local mirror is stale. The page-level `$effect`
   *  fires this on every fresh `ai::activity` log entry — the same
   *  channel the activity panel reads, so we get refresh-on-CLI for
   *  free without inventing a new broadcast. Self-initiated UI patches
   *  set no `X-Ferrite-Command` header, so the middleware skips them
   *  and we don't double-refresh on our own writes. */
  async refreshFromServer(): Promise<void> {
    if (this.phase !== 'ready') return;
    try {
      const [fg, src, st, sinks, blocks, caps, profile] = await Promise.all([
        fetchFlowgraph(),
        fetchSource(),
        fetchPipelineStatus(),
        fetchUiSinks(),
        fetchPipelineBlocks(),
        fetchSourceCapabilities(),
        fetchProfile(),
      ]);
      this.flowgraph = fg;
      this.source = src;
      this.status = st;
      this.nodeUiSinks = indexByName(sinks);
      this.nodeBlocks = indexById(blocks);
      this.sourceCaps = caps;
      // Profile too: a CLI-driven `transcribe on/off` flips this axis
      // without touching `setProfile`. Pulling it here keeps the
      // tri-state Audio control honest and lets the transcribe→voice-NR
      // coupling in +page.svelte fire for the CLI path as well.
      this.profile = profile;
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      logs.push('client', 'warn', `pipeline refresh failed: ${msg}`);
    }
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

/** Shallow equality on a SourceConfig — used by the readback poll to
 *  skip reassignments when the driver state didn't move (so reactive
 *  consumers like the AI prompt rebuild only when something changed). */
function sourceEquals(a: SourceConfig, b: SourceConfig): boolean {
  if (a.type !== b.type) return false;
  const ka = Object.keys(a.params);
  const kb = Object.keys(b.params);
  if (ka.length !== kb.length) return false;
  for (const k of ka) {
    if (a.params[k] !== b.params[k]) return false;
  }
  return true;
}

function profileEquals(a: Profile, b: Profile): boolean {
  return a.audio === b.audio && a.transcribe === b.transcribe && a.audio_split === b.audio_split;
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

/** `Number(x)`-with-finite-check, returning null on failure. Used by
 *  the VFO restore path which reads block.values entries that come
 *  back as `unknown` from the catch-all params blob. */
function numberOrNull(v: unknown): number | null {
  if (typeof v !== 'number' || !Number.isFinite(v)) return null;
  return v;
}

/** Read the active preset's `src.center_freq_hz` hint from the
 *  build-time-bundled catalog (a record of every preset's raw
 *  FlowgraphDoc). The hint is the value the preset author wrote in
 *  the JSON — used for first-time landing and out-of-range fallback,
 *  never overrides a live in-range tune. Returns null if the slug
 *  isn't in the catalog or the preset doesn't declare a centre
 *  frequency. */
function presetSrcFreqHint(slug: string): number | null {
  const doc = presetDocBySlug.get(slug);
  const params = doc?.blocks?.src?.params as Record<string, unknown> | undefined;
  return numberOrNull(params?.center_freq_hz);
}

/** Read the active preset's channelizer `freq_shift_hz` hint —
 *  the VFO offset the preset's author intended as the starting
 *  point. Same caveat as [`presetSrcFreqHint`]: hint, not state. */
function presetVfoHint(slug: string): number | null {
  const doc = presetDocBySlug.get(slug);
  if (!doc) return null;
  // Find any block in the preset declaring freq_shift_hz — typically
  // `chan` / Channelizer.
  for (const block of Object.values(doc.blocks ?? {})) {
    const params = block.params as Record<string, unknown> | undefined;
    const v = numberOrNull(params?.freq_shift_hz);
    if (v !== null) return v;
  }
  return null;
}

export const pipeline = new PipelineStore();

/**
 * Axis context (center freq, sample rate) the spectrum/waterfall viz
 * components read to label and tune their displays. This is a stable
 * shim: the FFT display axes are derived here from the active preset
 * rather than carried on each viz component.
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
