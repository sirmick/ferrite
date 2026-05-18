// Main-thread owner of the browser-side runtime.
//
// Responsibilities:
//   1. Spawn the runner Worker and wrap it in [`FlowgraphRunner`].
//   2. Own an `AudioContext` that holds one `AudioWorkletNode` per
//      `AudioSink` block in the loaded flowgraph. The worklet reads
//      from the `SharedArrayBuffer` the runner allocates; we just plumb
//      the SAB through on load and connect the node to `destination`.
//   3. Track `pipeline.flowgraph` and `pipeline.status` and translate
//      them into `runner.load/start/stop`. Structural-fingerprint diff
//      avoids reloading (and glitching audio) when only block *params*
//      change — VFO drags, gain tweaks, etc. leave the runner alone.
//   4. Unlock the AudioContext on first user gesture (Chrome refuses
//      `.resume()` before then).
//
// Reactivity: the class exposes `$state` fields and `wire(pipelineLike)`
// returns a cleanup that sets up `$effect`s on the caller's scope. The
// page mounts us once in `onMount` and drops the returned disposer on
// unmount.

import { audioPanel } from '$lib/audio/audioPanel.svelte';
import { AUDIO_RING_PROCESSOR_NAME } from '$lib/audio/audioRingProcessor';
// URL imports — Vite transforms these into content-hashed asset URLs at
// build time, so the Worker and Worklet bundles are emitted alongside
// the page JS without a hand-written build step.
import audioWorkletUrl from '$lib/audio/audioRingProcessor?worker&url';
import { logs } from '$lib/logs/store.svelte';
import { clientControls } from '$lib/control/clientStore.svelte';
import { nrBundle } from '$lib/presets/nrPresets';
import { transcript } from '$lib/transcribe/store.svelte';
import type { FlowgraphDoc } from '$lib/flowgraph';

import { FlowgraphRunner } from './runnerClient';

type AudioState = 'unavailable' | 'suspended' | 'running';
type RunnerState = 'idle' | 'loading' | 'loaded' | 'running' | 'error';

interface MountedAudioNode {
  blockId: string;
  node: AudioWorkletNode;
  /** Non-null when the node is a member of a stereo pair routed through
   *  a ChannelMerger — disconnected alongside the node on teardown. */
  merger?: ChannelMergerNode;
}

/** Inspect an audio-sink block id and return the channel role a preset
 *  is asking for, or null when the id doesn't carry a stereo hint. We
 *  accept `_l` / `_left` / `_r` / `_right` suffixes (case-insensitive)
 *  — the rest of the id is author-chosen. */
function blockIdRole(blockId: string): 'left' | 'right' | null {
  const id = blockId.toLowerCase();
  if (id.endsWith('_l') || id.endsWith('_left')) return 'left';
  if (id.endsWith('_r') || id.endsWith('_right')) return 'right';
  return null;
}

function findRoleEntry(
  sabs: Readonly<Record<string, SharedArrayBuffer>>,
  want: 'left' | 'right',
): { blockId: string; sab: SharedArrayBuffer } | undefined {
  for (const [blockId, sab] of Object.entries(sabs)) {
    if (blockIdRole(blockId) === want) return { blockId, sab };
  }
  return undefined;
}

class BrowserRuntime {
  audioState = $state<AudioState>('suspended');
  runnerState = $state<RunnerState>('idle');
  /** Human-readable error surface. `null` when healthy. */
  errorMessage = $state<string | null>(null);
  /** Block ids currently instantiated on the browser side (for the
   *  audio panel and diagnostics). */
  loadedBlocks = $state<ReadonlyArray<string>>([]);
  /** `VoiceTranscribe` tap block ids on the browser side (the
   *  `transcribeSabs` keys). This is the *only* client-readable signal
   *  that transcription is live: the block is server-injected then
   *  env-split to the browser, so it appears in neither
   *  `pipeline.flowgraph` (raw preset) nor `pipeline.blocks`
   *  (`/api/pipeline/blocks` is node-only). The audio tri-state and the
   *  Transcript advanced view gate on this. */
  voiceTranscribeIds = $state<ReadonlyArray<string>>([]);
  /** Id of the `audio_nr` block (AudioNrMono/Stereo) in the loaded
   *  graph, or null. Used to push the persisted NR preset after every
   *  (re)load so it survives the transcribe-triggered re-compose. */
  private audioNrId: string | null = null;
  /** Live param values applied to browser-placed blocks via
   *  `reconfigureBlock`, keyed by block id → {param: value}. The server
   *  REST `/api/pipeline/blocks` can't see these (browser side), so
   *  `pipeline.blocks` overlays this map — the single place browser
   *  block state loops back to the UI, mirroring the uiSinks merge.
   *  Cleared on (re)load: the new graph starts from authored params. */
  paramOverrides = $state<Record<string, Record<string, unknown>>>({});
  /** Browser-side `ui:<name>` Events sinks from the last load (name +
   *  stream_id, matching the node half's allocation). `pipeline`
   *  merges these into `uiSinks` so advanced views attach when the
   *  decoder ran browser-side. Empty when nothing is loaded. */
  uiSinks = $state<ReadonlyArray<{ name: string; stream_id: number }>>([]);
  /** Set by `pipeline`: receives browser-side decoder events the
   *  worker drained, to loopback into the main-thread FrameClient.
   *  Unset → events are dropped (no consumer; harmless). */
  onDecodedEvents: ((streamId: number, lines: string[]) => void) | undefined;
  /** Set by the page: resolves the live VFO absolute frequency (Hz) so
   *  transcript segments are stamped with the band they were heard on.
   *  Injected (not imported) to avoid a tuning→dispatch→browserRuntime
   *  import cycle. Unset → segments stamp `vfoHz: null`. */
  vfoHzProvider: (() => number | null) | undefined;

  private runner: FlowgraphRunner | undefined;
  private worker: Worker | undefined;
  private audioCtx: AudioContext | undefined;
  private workletReady: Promise<void> | undefined;
  private audioNodes: MountedAudioNode[] = [];
  /** One transcription Worker per `VoiceTranscribe` block, keyed by
   *  block id. Reads the tap SAB directly; not an audio node. */
  private transcribeWorkers = new Map<string, Worker>();
  private lastStructuralFingerprint: string | undefined;
  /** Serialise concurrent reload/start/stop so a rapid preset swap
   *  doesn't interleave two lifecycles against one runner. */
  private inflight: Promise<unknown> = Promise.resolve();
  /** Latest desired pipeline state the page asked for via `syncStatus`.
   *  Decoupled from `runnerState` so a status-before-load race doesn't
   *  drop the `start`: after every reload we check this flag and start
   *  if the user wants to be running. */
  private desiredRunning = false;

  /** Spawn the worker + AudioContext. Safe to call once at app start;
   *  subsequent calls are no-ops. */
  init(): void {
    if (this.worker) return;
    try {
      this.worker = new Worker(new URL('./runnerWorker.ts', import.meta.url), {
        type: 'module',
        name: 'ferrite-runner',
      });
      this.worker.addEventListener('error', (ev) => {
        // Worker-level errors (module parse failure, uncaught throw)
        // never reach FlowgraphRunner — surface them here.
        const msg = ev.message || 'worker error';
        this.errorMessage = `runner worker: ${msg}`;
        this.runnerState = 'error';
        logs.push('client', 'error', `runner worker: ${msg}`);
      });
      this.runner = new FlowgraphRunner(
        this.worker,
        (text) => {
          logs.push('client', 'info', text);
        },
        (streamId, lines) => this.onDecodedEvents?.(streamId, lines),
        (blockId, rateHz) => this.forwardTranscribeRate(blockId, rateHz),
      );
    } catch (err) {
      this.errorMessage = errorMessage(err);
      this.runnerState = 'error';
      logs.push('client', 'error', `spawn runner worker: ${this.errorMessage}`);
      return;
    }

    // AudioContext creation doesn't need a gesture; resume does. We
    // start suspended and flip to 'running' once unlockAudio succeeds.
    try {
      this.audioCtx = new AudioContext({ latencyHint: 'interactive' });
      // `audioWorklet` is absent on some older browsers and in
      // non-secure contexts (the API requires HTTPS or localhost). Bail
      // cleanly rather than throwing on `undefined.addModule`. COOP/COEP
      // are set in dev by the vite plugin (`coop-coep.ts`), so on
      // localhost this is essentially a feature-detect for AudioWorklet.
      if (!this.audioCtx.audioWorklet) {
        this.audioState = 'unavailable';
        this.errorMessage =
          'AudioWorklet unavailable — requires a secure context (https/localhost) and COOP/COEP headers';
        logs.push('client', 'error', this.errorMessage);
        return;
      }
      this.audioState = this.audioCtx.state === 'running' ? 'running' : 'suspended';
      this.audioCtx.addEventListener('statechange', () => {
        if (!this.audioCtx) return;
        this.audioState = this.audioCtx.state === 'running' ? 'running' : 'suspended';
      });
      // Load the worklet module once. Every audio node we later
      // construct reuses this registration.
      this.workletReady = this.audioCtx.audioWorklet.addModule(audioWorkletUrl).catch((err) => {
        this.errorMessage = `audio worklet load: ${errorMessage(err)}`;
        logs.push('client', 'error', this.errorMessage);
        throw err;
      });
    } catch (err) {
      this.audioState = 'unavailable';
      this.errorMessage = `audio context: ${errorMessage(err)}`;
      logs.push('client', 'error', this.errorMessage);
    }
  }

  /** Resume the AudioContext. Must be called from a user-gesture event
   *  handler (click, keydown, touchstart). Idempotent. */
  async unlockAudio(): Promise<void> {
    const ctx = this.audioCtx;
    if (!ctx) return;
    if (ctx.state === 'running') return;
    try {
      await ctx.resume();
    } catch (err) {
      // Some browsers throw InvalidStateError if the user hasn't
      // actually interacted — caller is expected to retry on the next
      // gesture.
      logs.push('client', 'warn', `audioctx resume: ${errorMessage(err)}`);
    }
  }

  /** Structural fingerprint of `doc`: everything except block `params`.
   *  Two docs that differ only in param values map to the same
   *  fingerprint, so the runner doesn't tear down for a VFO drag. */
  private static structuralFingerprint(doc: FlowgraphDoc): string {
    const stripped: Record<string, unknown> = {
      name: doc.name,
      wires: doc.wires,
    };
    const blocks: Record<string, unknown> = {};
    for (const [id, raw] of Object.entries(doc.blocks ?? {})) {
      const block = raw as { type?: string; placement?: string };
      blocks[id] = { type: block.type, placement: block.placement };
    }
    stripped.blocks = blocks;
    return JSON.stringify(stripped);
  }

  /** Called by the page when `pipeline.flowgraph` changes. Reloads only
   *  on structural changes (block type/placement, wires, name). */
  syncFlowgraph(doc: FlowgraphDoc, wsUrl: string): void {
    if (!this.runner) return;
    const fp = BrowserRuntime.structuralFingerprint(doc);
    if (fp === this.lastStructuralFingerprint) return;
    this.lastStructuralFingerprint = fp;
    this.enqueue(() => this.reload(doc, wsUrl));
  }

  /** Apply a params delta to a browser-placed block. Routes through
   *  the worker's Rust runtime via `FlowgraphRunner.reconfigureBlock`.
   *  Surface-level wrapper so `applyControl(\`browser.${id}.${key}\`, v)`
   *  has a single destination regardless of which block. Errors surface
   *  into the logs store; caller gets a fulfilled void. */
  async reconfigureBlock(blockId: string, delta: Record<string, unknown>): Promise<void> {
    const runner = this.runner;
    if (!runner) return;
    try {
      const res = await runner.reconfigureBlock(blockId, delta);
      // Loop the applied values back so the UI mirror reflects a
      // browser-placed block's live params (server REST never sees
      // them). `pipeline.blocks` overlays `paramOverrides`, mirroring
      // the node+browser `uiSinks` merge.
      if (res.changes.length > 0) {
        const next = { ...this.paramOverrides };
        for (const c of res.changes) {
          next[c.block_id] = { ...(next[c.block_id] ?? {}), [c.param_key]: c.new_value };
        }
        this.paramOverrides = next;
      }
    } catch (err) {
      const msg = errorMessage(err);
      logs.push('client', 'error', `browser-block reconfigure ${blockId}: ${msg}`);
    }
  }

  /** Push the persisted NR preset (`client.audio.nrPreset`) to the
   *  `audio_nr` block. Called post-load (re-applies over the preset's
   *  authored params, surviving every re-compose) and by ProfileChips
   *  on a live pick. `auto`/no block → no-op (keep authored NR). */
  applyNrPreset(): void {
    if (!this.audioNrId) return;
    // The browser runner only has a graph when loaded/running. A pick
    // made while stopped or mid-reload would hit "no flowgraph loaded"
    // — skip it; reload()'s post-load call applies the persisted
    // selection once the graph is up, so nothing is lost.
    if (this.runnerState !== 'loaded' && this.runnerState !== 'running') return;
    const bundle = nrBundle(clientControls.get('client.audio.nrPreset') as string);
    if (bundle) void this.reconfigureBlock(this.audioNrId, bundle);
  }

  /** Called by the page when `pipeline.status` changes. Idempotent.
   *
   *  Stores the desired state so a later reload can auto-start if the
   *  caller got here before the first load completed (status-before-
   *  load race). Without that, the `start` effect only fires when
   *  `pipeline.status` actually changes — if it was already `running`
   *  when reload finished, we'd never start. */
  syncStatus(running: boolean): void {
    this.desiredRunning = running;
    if (!this.runner) return;
    this.enqueue(() => (running ? this.startInner() : this.stopInner()));
  }

  private async reload(doc: FlowgraphDoc, wsUrl: string): Promise<void> {
    if (!this.runner) return;
    this.runnerState = 'loading';
    this.errorMessage = null;
    try {
      // Always stop before loading — `RunnerCore.load` rejects when
      // something is already loaded. The first call is a no-op because
      // we haven't loaded anything yet; the worker tolerates stop-on-empty.
      await this.tearDownAudioNodes();
      this.tearDownTranscribeWorkers();
      try {
        await this.runner.stop();
      } catch {
        // Worker treats stop-on-empty as a no-op internally, but
        // swallow any boundary case — we're about to reload anyway.
      }
      // `pipeline.flowgraph` is a Svelte 5 `$state` Proxy; structured-
      // clone (which postMessage uses) throws on Proxies with "Proxy
      // object could not be cloned". Snapshotting produces a plain
      // deep-clone the worker can receive.
      const plain = $state.snapshot(doc) as FlowgraphDoc;
      const result = await this.runner.load(plain, wsUrl);
      this.loadedBlocks = result.blocks;
      this.uiSinks = result.uiSinks;
      await this.attachAudioNodes(result.audioSabs);
      this.attachTranscribeWorkers(result.transcribeSabs);
      this.voiceTranscribeIds = Object.keys(result.transcribeSabs);
      this.audioNrId =
        Object.entries(plain.blocks ?? {}).find(([, b]) =>
          (b.type ?? '').startsWith('AudioNr'),
        )?.[0] ?? null;
      // New graph starts from the server's authored params — drop any
      // overrides from the previous one; applyNrPreset() below repopulates.
      this.paramOverrides = {};
      // Mark loaded *before* re-applying the NR preset so the
      // runner-state guard in applyNrPreset() passes here (the
      // legitimate post-load apply) but still rejects clicks made
      // while the runner has no graph (loading / stopped).
      this.runnerState = 'loaded';
      // Re-apply the persisted NR preset on top of the preset's
      // authored params (same post-load fan-out as the transcribe
      // prompt) so it survives this and every re-compose.
      this.applyNrPreset();
      const audioCount = Object.keys(result.audioSabs).length;
      logs.push(
        'client',
        'info',
        `runner loaded: blocks=${result.blocks.length} audioSinks=${audioCount} audioState=${this.audioState}`,
      );
      // Catch the status-before-load case: `syncStatus(true)` may have
      // been called before `reload` landed, returning early because
      // `runnerState` was `loading`. Now that load is done, honour the
      // caller's last-expressed desire.
      if (this.desiredRunning) {
        await this.startInner();
      }
    } catch (err) {
      this.errorMessage = errorMessage(err);
      this.runnerState = 'error';
      logs.push('client', 'error', `runner load: ${this.errorMessage}`);
    }
  }

  private async startInner(): Promise<void> {
    if (!this.runner) return;
    if (this.runnerState === 'running') return; // already started; no-op
    if (this.runnerState !== 'loaded') return; // not loaded yet — reload's tail will pick it up
    try {
      await this.runner.start();
      this.runnerState = 'running';
      logs.push('client', 'info', `runner started (audio=${this.audioState})`);
    } catch (err) {
      this.errorMessage = errorMessage(err);
      this.runnerState = 'error';
      logs.push('client', 'error', `runner start: ${this.errorMessage}`);
    }
  }

  private async stopInner(): Promise<void> {
    if (!this.runner) return;
    if (this.runnerState === 'idle' || this.runnerState === 'error') return;
    try {
      await this.runner.stop();
      this.runnerState = 'loaded';
    } catch (err) {
      this.errorMessage = errorMessage(err);
      logs.push('client', 'warn', `runner stop: ${this.errorMessage}`);
    }
  }

  private async attachAudioNodes(
    audioSabs: Readonly<Record<string, SharedArrayBuffer>>,
  ): Promise<void> {
    const ctx = this.audioCtx;
    if (!ctx || !this.workletReady) return;
    await this.workletReady;

    // Partition by block-id suffix so a preset can declare stereo by
    // naming its two AudioSinks `…_l`/`…_left` and `…_r`/`…_right`. If
    // exactly one of each role is present we wire them through a
    // ChannelMergerNode; everything else routes mono straight to
    // destination (and fans out to all output channels, as today).
    const leftEntry = findRoleEntry(audioSabs, 'left');
    const rightEntry = findRoleEntry(audioSabs, 'right');
    const stereoPair =
      leftEntry && rightEntry && leftEntry.blockId !== rightEntry.blockId
        ? { left: leftEntry, right: rightEntry }
        : null;

    let merger: ChannelMergerNode | undefined;
    if (stereoPair) {
      merger = ctx.createChannelMerger(2);
      merger.connect(ctx.destination);
    }

    for (const [blockId, sab] of Object.entries(audioSabs)) {
      try {
        const node = new AudioWorkletNode(ctx, AUDIO_RING_PROCESSOR_NAME, {
          // The worklet's processorOptions matches `{ sab }` — see
          // `AudioRingConsumerProcessor` constructor.
          processorOptions: { sab },
          outputChannelCount: [1],
        });

        let role: 'mono' | 'left' | 'right' = 'mono';
        if (stereoPair && blockId === stereoPair.left.blockId) {
          node.connect(merger!, 0, 0);
          role = 'left';
        } else if (stereoPair && blockId === stereoPair.right.blockId) {
          node.connect(merger!, 0, 1);
          role = 'right';
        } else {
          // Mono fan-out to destination works as before — the worklet
          // emits one channel and Web Audio copies it to each speaker
          // channel via the default upmix rules.
          node.connect(ctx.destination);
        }

        // Hook into the audio panel store — volume/mute flow down to
        // the worklet, peak/RMS flow back up for the meter. Survives
        // preset reloads: the store keeps the user's volume and
        // re-pushes it to whatever new node comes out of the next load.
        audioPanel.attach(node, role);
        this.audioNodes.push({ blockId, node, merger });
        logs.push(
          'client',
          'info',
          `audio node attached: block=${blockId} role=${role} sabBytes=${sab.byteLength} ctxState=${ctx.state}`,
        );
      } catch (err) {
        logs.push('client', 'error', `audio node ${blockId}: ${errorMessage(err)}`);
      }
    }
  }

  private async tearDownAudioNodes(): Promise<void> {
    const mergers = new Set<ChannelMergerNode>();
    for (const { node, merger } of this.audioNodes) {
      audioPanel.detach(node);
      try {
        node.disconnect();
      } catch {
        /* best effort */
      }
      if (merger) mergers.add(merger);
    }
    for (const m of mergers) {
      try {
        m.disconnect();
      } catch {
        /* best effort */
      }
    }
    this.audioNodes = [];
  }

  /** Spin up one transcription Worker per `VoiceTranscribe` tap SAB.
   *  The Worker reads the ring on its own clock; we just forward
   *  results into the transcript store. Worklet-less sibling of
   *  `attachAudioNodes`. */
  private attachTranscribeWorkers(
    transcribeSabs: Readonly<Record<string, SharedArrayBuffer>>,
  ): void {
    const ids = Object.keys(transcribeSabs);
    if (ids.length === 0) {
      transcript.setStatus('idle');
      return;
    }
    for (const [blockId, sab] of Object.entries(transcribeSabs)) {
      try {
        // Worker options must be statically analyzable for Vite's
        // worker plugin — a template-literal `name` trips "value is
        // not static" (and broke every audio preset once VoiceTranscribe
        // became ubiquitous). The name is only a devtools label; keep
        // it constant and disambiguate per block via the message
        // payloads / `transcript` store keying instead.
        const w = new Worker(new URL('../transcribe/transcribeWorker.ts', import.meta.url), {
          type: 'module',
          name: 'ferrite-transcribe',
        });
        w.onmessage = (ev: MessageEvent) => this.onTranscribeMessage(ev.data);
        w.onerror = (ev) => transcript.setStatus('error', `worker: ${ev.message || 'load failed'}`);
        w.postMessage({ type: 'init', sab, blockId });
        // Seed the worker with the persisted prompt override (empty →
        // it uses its built-in ham corpus).
        const p = clientControls.get('client.transcribe.prompt');
        if (p) w.postMessage({ type: 'prompt', text: p });
        this.transcribeWorkers.set(blockId, w);
        logs.push('client', 'info', `transcribe worker attached: block=${blockId}`);
      } catch (err) {
        transcript.setStatus('error', errorMessage(err));
        logs.push('client', 'error', `transcribe worker ${blockId}: ${errorMessage(err)}`);
      }
    }
  }

  private tearDownTranscribeWorkers(): void {
    for (const w of this.transcribeWorkers.values()) {
      try {
        w.postMessage({ type: 'stop' });
        w.terminate();
      } catch {
        /* best effort */
      }
    }
    this.transcribeWorkers.clear();
    transcript.setStatus('idle');
  }

  private forwardTranscribeRate(blockId: string, rateHz: number): void {
    this.transcribeWorkers.get(blockId)?.postMessage({ type: 'rate', rateHz });
  }

  /** Push an edited whisper `initial_prompt` to every live
   *  transcription Worker. Called by `applyControl` when
   *  `client.transcribe.prompt` changes (off-thread fan-out, same
   *  pattern as audio volume → worklet). */
  setTranscribePrompt(text: string): void {
    for (const w of this.transcribeWorkers.values()) {
      w.postMessage({ type: 'prompt', text });
    }
  }

  /** Worker → store bridge. Segment / status / glitch-counter messages
   *  land here and update the reactive transcript store the panel
   *  renders. */
  private onTranscribeMessage(msg: { type: string; [k: string]: unknown }): void {
    if (msg.type === 'segment') {
      const vfoHz = this.vfoHzProvider?.() ?? null;
      transcript.push({
        atMs: msg.atMs as number,
        vfoHz,
        t0: msg.t0 as number,
        t1: msg.t1 as number,
        text: msg.text as string,
        tokens: (msg.tokens as { text: string; p: number }[]) ?? [],
        confidence: msg.confidence as number,
        noSpeechProb: msg.noSpeechProb as number,
        cont: msg.cont as boolean,
        gapMs: msg.gapMs as number,
      });
      // The recognised text itself → decoder log (server files
      // `[transcribe]`-prefixed client lines under `decoder::transcribe`)
      // so the embedded AI / `ferrite-ctl decoder recent` can read the
      // transcription with no screenshot or DOM access.
      const fLbl = vfoHz != null ? `${(vfoHz / 1e6).toFixed(4)}MHz` : '—';
      logs.push('client', 'info', `[transcribe] ${fLbl} ${(msg.text as string).trim()}`);
    } else if (msg.type === 'status') {
      transcript.setStatus(msg.status as never, String(msg.detail ?? ''));
      transcript.modelName = String(msg.model ?? '');
    } else if (msg.type === 'dropped') {
      transcript.droppedSamples = msg.total as number;
      // Loud, greppable backend line — this is "a section went missing".
      logs.push(
        'client',
        'warn',
        `[transcribe] DROP ${(msg.shedS as number).toFixed(1)}s utterance — ` +
          `whisper behind, queue full; total_shed=${msg.total as number}`,
      );
    } else if (msg.type === 'stat') {
      const segS = msg.segS as number;
      const inferMs = msg.inferMs as number;
      const rtf = segS > 0 ? inferMs / (segS * 1000) : 0;
      logs.push(
        'client',
        rtf > 1 ? 'warn' : 'info',
        `[transcribe] seg=${segS.toFixed(1)}s infer=${inferMs}ms ` +
          `rtf=${rtf.toFixed(2)}x queue=${msg.queued as number}`,
      );
    } else if (msg.type === 'telemetry') {
      transcript.setTelemetry({
        gateOpen: msg.gateOpen as boolean,
        level: msg.level as number,
        threshold: msg.threshold as number,
        queued: msg.queued as number,
        lagMs: msg.lagMs as number,
      });
    }
  }

  /** Run lifecycle ops one-at-a-time so a fast preset flip doesn't
   *  interleave load/start/stop. Errors stay in-channel — the next op
   *  still runs. */
  private enqueue<T>(op: () => Promise<T>): void {
    this.inflight = this.inflight.then(op, op);
    this.inflight.catch(() => {
      /* errors are surfaced into errorMessage already */
    });
  }

  /** Tear everything down. Called from the page's onMount cleanup.
   *
   *  Clears the singleton's fields *synchronously* at the top so a
   *  concurrent `init()` (HMR remount, route change) sees empty slots
   *  and fully re-initialises. The slow async cleanup runs against
   *  captured local refs — it can't race with the new runtime the
   *  next `init()` builds. Without this the HMR sequence
   *  (unmount → init → async-teardown-completes) would nuke the new
   *  runner's refs and leave `this.runner` permanently undefined. */
  async teardown(): Promise<void> {
    const oldRunner = this.runner;
    const oldAudioCtx = this.audioCtx;
    const oldNodes = this.audioNodes;
    this.tearDownTranscribeWorkers();
    this.runner = undefined;
    this.worker = undefined;
    this.audioCtx = undefined;
    this.audioNodes = [];
    this.workletReady = undefined;
    this.runnerState = 'idle';
    this.audioState = 'suspended';
    this.lastStructuralFingerprint = undefined;
    this.loadedBlocks = [];
    this.voiceTranscribeIds = [];
    this.audioNrId = null;
    this.paramOverrides = {};
    this.uiSinks = [];

    // Slow cleanup on the captured refs — safe to interleave with a
    // fresh `init()` because the singleton no longer points at any of
    // these objects.
    for (const { node } of oldNodes) {
      audioPanel.detach(node);
      try {
        node.disconnect();
      } catch {
        /* best effort */
      }
    }
    try {
      await oldRunner?.stop();
    } catch {
      /* best-effort */
    }
    oldRunner?.terminate();
    try {
      await oldAudioCtx?.close();
    } catch {
      /* best-effort */
    }
  }
}

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

export const browserRuntime = new BrowserRuntime();
