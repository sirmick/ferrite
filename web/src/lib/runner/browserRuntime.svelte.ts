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
import type { FlowgraphDoc } from '$lib/flowgraph';

import { FlowgraphRunner } from './runnerClient';

type AudioState = 'unavailable' | 'suspended' | 'running';
type RunnerState = 'idle' | 'loading' | 'loaded' | 'running' | 'error';

interface MountedAudioNode {
  blockId: string;
  node: AudioWorkletNode;
}

class BrowserRuntime {
  audioState = $state<AudioState>('suspended');
  runnerState = $state<RunnerState>('idle');
  /** Human-readable error surface. `null` when healthy. */
  errorMessage = $state<string | null>(null);
  /** Block ids currently instantiated on the browser side (for the
   *  audio panel and diagnostics). */
  loadedBlocks = $state<ReadonlyArray<string>>([]);

  private runner: FlowgraphRunner | undefined;
  private worker: Worker | undefined;
  private audioCtx: AudioContext | undefined;
  private workletReady: Promise<void> | undefined;
  private audioNodes: MountedAudioNode[] = [];
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
      this.runner = new FlowgraphRunner(this.worker, (text) => {
        logs.push('client', 'info', text);
      });
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
      await this.attachAudioNodes(result.audioSabs);
      this.runnerState = 'loaded';
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
    for (const [blockId, sab] of Object.entries(audioSabs)) {
      try {
        const node = new AudioWorkletNode(ctx, AUDIO_RING_PROCESSOR_NAME, {
          // The worklet's processorOptions matches `{ sab }` — see
          // `AudioRingConsumerProcessor` constructor.
          processorOptions: { sab },
          outputChannelCount: [1],
        });
        node.connect(ctx.destination);
        // Hook into the audio panel store — volume/mute flow down to
        // the worklet, peak/RMS flow back up for the meter. Survives
        // preset reloads: the store keeps the user's volume and
        // re-pushes it to whatever new node comes out of the next load.
        audioPanel.attach(node);
        this.audioNodes.push({ blockId, node });
        logs.push(
          'client',
          'info',
          `audio node attached: block=${blockId} sabBytes=${sab.byteLength} ctxState=${ctx.state}`,
        );
      } catch (err) {
        logs.push('client', 'error', `audio node ${blockId}: ${errorMessage(err)}`);
      }
    }
  }

  private async tearDownAudioNodes(): Promise<void> {
    for (const { node } of this.audioNodes) {
      audioPanel.detach(node);
      try {
        node.disconnect();
      } catch {
        /* best effort */
      }
    }
    this.audioNodes = [];
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
    this.runner = undefined;
    this.worker = undefined;
    this.audioCtx = undefined;
    this.audioNodes = [];
    this.workletReady = undefined;
    this.runnerState = 'idle';
    this.audioState = 'suspended';
    this.lastStructuralFingerprint = undefined;

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
