<script lang="ts">
  // Always-visible top bar. Carries the pipeline action cluster —
  // Start/Stop, Source…, Flowgraph… — plus the status chips
  // (HealthDots, ws/fps, source label, error chip). Renders
  // unconditionally so the operator can always reach Start, even
  // before the pipeline has any axes to display in RfQuickBar.
  //
  // Convenience controls that depend on a running pipeline (VFO,
  // centre, rate, zoom) live one row down in RfQuickBar.

  import HealthDots from './HealthDots.svelte';
  import SourceDialog from '$lib/controls/SourceDialog.svelte';
  import FlowgraphDialog from '$lib/controls/FlowgraphDialog.svelte';
  import { defaultsFor, toSourceConfig } from '$lib/controls/optionsModel';
  import type { SourceConfig } from '$lib/api/source';
  import { pipeline } from '$lib/pipeline.svelte';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import { activeAdvancedView } from '$lib/advanced/registry';
  import { browserRuntime } from '$lib/runner/browserRuntime.svelte';
  import { snapshotView, dataUrlToBase64 } from '$lib/viz/viewRegistry';
  import { logs } from '$lib/logs/store.svelte';

  // Right (channel-detail) pane selector. Disabled when the active
  // preset has no Channelizer (no runtime-injected `ui:fft_narrow`
  // sink). The `signals` option only appears when the preset advertises
  // a `ui:signals` sink (the SignalList block's strongest-signal stream).
  let hasNarrow = $derived(pipeline.uiSinks.fft_narrow !== undefined);
  let hasSignals = $derived(pipeline.uiSinks.signals !== undefined);
  let rightPane = $derived(clientControls.get('client.workspace.rightPane'));

  // Advanced-view toggle. The active preset's registered view (or
  // null); when null the button is disabled. Independent of Channel —
  // Advanced replaces only the wide FFT/waterfall column.
  let hasAudioSink = $derived(
    Object.values(pipeline.flowgraph?.blocks ?? {}).some((b) => b.type === 'AudioSink'),
  );
  let advancedView = $derived(
    activeAdvancedView({
      uiSinks: pipeline.uiSinks,
      voiceTranscribeIds: browserRuntime.voiceTranscribeIds,
      hasAudioSink,
    }),
  );
  let mainPane = $derived(clientControls.get('client.workspace.mainPane'));

  let frameRate = $state(0);
  let wsBwBps = $state(0);
  let showSource = $state(false);
  let showFlowgraph = $state(false);
  let startStopBusy = $state(false);

  // Sample two counters per second:
  //   - per-stream FFT frames (still useful — FPS shows the display
  //     pipeline is alive even if other streams are noisy)
  //   - total WS bytes-since-open via `client.bytesReceived` (covers
  //     IQ, audio, control, every multiplexed stream summed)
  // Diff against the previous sample for a 1 Hz rolling rate.
  $effect(() => {
    const c = pipeline.client;
    const fftSid = pipeline.uiSinks.fft?.stream_id;
    if (!c || fftSid === undefined) {
      frameRate = 0;
      wsBwBps = 0;
      return;
    }
    let frames = 0;
    let windowStart = performance.now();
    let lastBytes = c.bytesReceived;
    const unsub = c.subscribe(fftSid, () => {
      frames += 1;
    });
    const timer = setInterval(() => {
      const now = performance.now();
      const dt = (now - windowStart) / 1000;
      const bytesNow = c.bytesReceived;
      frameRate = dt > 0 ? frames / dt : 0;
      wsBwBps = dt > 0 ? Math.max(0, bytesNow - lastBytes) / dt : 0;
      frames = 0;
      lastBytes = bytesNow;
      windowStart = now;
    }, 1000);
    return () => {
      clearInterval(timer);
      unsub();
    };
  });

  function fmtBw(bps: number): string {
    if (bps >= 1e6) return `${(bps / 1e6).toFixed(1)} MB/s`;
    if (bps >= 1e3) return `${(bps / 1e3).toFixed(1)} kB/s`;
    return `${Math.round(bps)} B/s`;
  }

  // Show the device's actual label (e.g. "SDRplay RSPduo (224001D748)")
  // when the source caps are available, instead of a generic "soapy"
  // tag — at a glance you want to see which radio is plugged in, not
  // just which abstraction layer it talks through.
  // The active decoder/preset, shown next to the SDR so it's always
  // clear what's being decoded. Falls back to name, then nothing.
  let presetLabel = $derived(pipeline.flowgraph?.label ?? pipeline.flowgraph?.name ?? null);

  // Source identity only — no centre freq. The RfQuickBar's nixies
  // already show centre freq (SDR-centre + VFO), so duplicating it in
  // the top status was just noise eating horizontal space.
  let sourceLabel = $derived.by(() => {
    const s = pipeline.source;
    if (!s) return '';
    const caps = pipeline.sourceCaps;
    if (s.type === 'SoapySource') {
      return caps?.kind === 'hardware' ? caps.capabilities.info.label : 'soapy';
    }
    if (s.type === 'FileSource') return 'file';
    if (s.type === 'SineSource') return 'sine';
    return s.type;
  });

  let shotBusy = $state(false);
  /** Capture the entire browser tab (or whatever the user picks) via
   *  `getDisplayMedia`, grab one frame, POST to `/api/screenshot`.
   *  Used to be a single-canvas grab through `snapshotView` — that
   *  only ever caught the wide waterfall / spectrum and missed the
   *  toolbar, side panel, AI chat, dialogs, etc. The displayCapture
   *  route is pixel-perfect (WebGL waterfalls included, live) at the
   *  cost of one per-click browser permission prompt where the
   *  operator picks which surface to share. `preferCurrentTab` hints
   *  Chrome's picker to default to *this* tab so it's usually a
   *  single-click confirm.
   *
   *  Falls back to the canvas-only path on browsers that don't ship
   *  `getDisplayMedia` (Safari < 13, some embedded webviews) so the
   *  button isn't dead-on-arrival there. */
  async function takeScreenshot() {
    if (shotBusy) return;
    shotBusy = true;
    let stream: MediaStream | undefined;
    try {
      const png_b64 = (await captureViaDisplayMedia()) ?? captureCanvasFallback();
      if (!png_b64) {
        logs.push('client', 'warn', 'screenshot: no capture path succeeded');
        return;
      }
      const res = await fetch('/api/screenshot', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ png_b64, label: sourceLabel }),
      });
      if (!res.ok) {
        logs.push('client', 'warn', `screenshot: server ${res.status} ${await res.text()}`);
        return;
      }
      const { path } = (await res.json()) as { path: string };
      logs.push('client', 'info', `screenshot saved: ${path}`);
    } catch (e) {
      logs.push('client', 'warn', `screenshot failed: ${e instanceof Error ? e.message : e}`);
    } finally {
      stream?.getTracks().forEach((t) => t.stop());
      shotBusy = false;
    }
  }

  /** displayCapture-path. Returns base64 PNG (no `data:` prefix) on
   *  success, `null` when the API is unavailable. Rejects on every
   *  other failure (user-cancelled, no track, drawImage error) so the
   *  caller surfaces it through the logs path. */
  async function captureViaDisplayMedia(): Promise<string | null> {
    if (typeof navigator === 'undefined' || !navigator.mediaDevices?.getDisplayMedia) {
      return null;
    }
    // `preferCurrentTab` is Chrome-only and makes the picker default
    // to this tab — single-click confirm in the happy path. Other
    // browsers ignore it and show the standard surface picker. Cast
    // because the lib.dom typings haven't caught up.
    const constraints = {
      video: true,
      audio: false,
      preferCurrentTab: true,
    } as unknown as MediaStreamConstraints;
    const stream = await navigator.mediaDevices.getDisplayMedia(constraints);
    try {
      const track = stream.getVideoTracks()[0];
      if (!track) throw new Error('no video track in displayCapture stream');
      const video = document.createElement('video');
      video.muted = true;
      video.srcObject = stream;
      await video.play();
      // First decoded frame can be a tick after `play()` resolves;
      // wait one rAF so the canvas grabs an actual picture, not a
      // black hole.
      await new Promise<void>((r) => requestAnimationFrame(() => r()));
      const w = video.videoWidth || track.getSettings().width || 1;
      const h = video.videoHeight || track.getSettings().height || 1;
      const canvas = document.createElement('canvas');
      canvas.width = w;
      canvas.height = h;
      const ctx = canvas.getContext('2d');
      if (!ctx) throw new Error('2D canvas context unavailable');
      ctx.drawImage(video, 0, 0, w, h);
      const dataUrl = canvas.toDataURL('image/png');
      return dataUrlToBase64(dataUrl);
    } finally {
      stream.getTracks().forEach((t) => t.stop());
    }
  }

  /** Pre-displayMedia fallback: same single-canvas path the button
   *  used to take. Returns base64 PNG or '' on miss. */
  function captureCanvasFallback(): string {
    let shot = snapshotView('wide-waterfall');
    if (shot.error || !shot.png_b64) shot = snapshotView('wide-spectrum');
    return shot.png_b64 ?? '';
  }

  async function togglePipeline() {
    if (startStopBusy) return;
    startStopBusy = true;
    try {
      if (pipeline.status === 'running') {
        await pipeline.stop();
      } else {
        await pipeline.start();
      }
    } finally {
      startStopBusy = false;
    }
  }
</script>

<div
  class="flex w-full flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-[color:var(--color-muted)]"
>
  <HealthDots />
  <!-- ws status / fps chip lived here. The HealthDots already convey
       "are we connected and moving samples?" via the source / node /
       browser dots; the literal "ws: open" line was redundant noise on
       the always-visible top row. Removed 2026-05-21. -->
  {#if pipeline.wsStatus === 'open' && frameRate > 0}
    <span class="hidden font-mono text-[11px] text-[color:var(--color-muted)] md:inline">
      {frameRate.toFixed(1)} fps · {fmtBw(wsBwBps)}
    </span>
  {/if}
  {#if sourceLabel}
    <span class="font-mono text-[11px]">{sourceLabel}</span>
  {/if}
  {#if presetLabel}
    <span class="font-mono text-[11px] text-slate-300" title="active decoder / preset"
      >▸ {presetLabel}</span
    >
  {/if}
  {#if pipeline.errorMessage}
    <span class="text-rose-400" title={pipeline.errorMessage}>error</span>
  {/if}
  <!-- Stop/Source/Flowgraph push to the right edge — they're terminal
       actions (mutate global state), the status chips on the left stay
       at the natural reading position. -->
  <div class="ml-auto flex flex-wrap items-center gap-x-2 gap-y-1">
    <!-- Display: two adjacent column selectors under one label.
         · Main  — the wide column: FFT/Waterfall vs the active preset's
           mode-specific view (Journal / FT8 / ADS-B / APRS / Transcript;
           `activeAdvancedView` resolves one on every catalog preset).
           Resets to `wide` on preset load.
         · Right — the channel-detail column: Off / FFT-Waterfall, plus
           "Strongest Signals" when the preset advertises `ui:signals`.
         Per-dropdown words hide on narrow screens (the shapes are
         self-evident); the group keeps the `Display:` label. -->
    <div class="flex items-center gap-2 text-[11px] text-[color:var(--color-muted)]">
      <span class="hidden md:inline">Display:</span>
      <label class="flex items-center gap-1">
        <span class="hidden lg:inline">Main</span>
        <select
          class="cursor-pointer rounded border border-slate-500 bg-slate-800/60 px-1 py-0.5 text-[11px] text-slate-200 hover:border-sky-400 hover:text-slate-50 focus:outline-none disabled:cursor-not-allowed disabled:opacity-40"
          disabled={!advancedView}
          value={advancedView && mainPane === 'advanced' ? 'advanced' : 'wide'}
          onchange={(e) => {
            const v = (e.target as HTMLSelectElement).value;
            void applyControl('client.workspace.mainPane', v);
            // Auto-enable transcription when the user picks Transcript on
            // a preset where it isn't on yet — keeps the "Transcript is
            // always offered for audio presets" promise (registry D-V)
            // without paying the whisper-worker cost up front. Server
            // re-composes; voiceTranscribeIds populates; TranscriptView
            // mounts on the next refresh — no extra dance for the user.
            if (
              v === 'advanced' &&
              advancedView?.sink === 'transcribe' &&
              !pipeline.profile.transcribe
            ) {
              void pipeline.setProfile({ ...pipeline.profile, transcribe: true });
            }
          }}
          title={advancedView
            ? `Main column: wide FFT/Waterfall or ${advancedView.label}.`
            : 'Active preset has no advanced view'}
        >
          <option value="wide">FFT / Waterfall</option>
          {#if advancedView}
            <option value="advanced">{advancedView.label}</option>
          {/if}
        </select>
      </label>
      <label class="flex items-center gap-1" class:opacity-40={!hasNarrow}>
        <span class="hidden lg:inline">Right</span>
        <select
          class="cursor-pointer rounded border border-slate-500 bg-slate-800/60 px-1 py-0.5 text-[11px] text-slate-200 hover:border-sky-400 hover:text-slate-50 focus:outline-none disabled:cursor-not-allowed disabled:opacity-40"
          disabled={!hasNarrow}
          value={hasNarrow ? rightPane : 'off'}
          onchange={(e) =>
            void applyControl('client.workspace.rightPane', (e.target as HTMLSelectElement).value)}
          title={hasNarrow
            ? 'Right column: channel-detail FFT/Waterfall, strongest-signal list, or off'
            : 'Active preset has no channelizer — no channel-detail pane available'}
        >
          <option value="off">Off</option>
          <option value="channel">FFT / Waterfall</option>
          {#if hasSignals}
            <option value="signals">Strongest Signals</option>
          {/if}
        </select>
      </label>
    </div>
    <button
      type="button"
      class="rounded border px-2 py-0.5 text-[11px] font-semibold disabled:opacity-50"
      class:running={pipeline.status === 'running'}
      class:stopped={pipeline.status !== 'running'}
      disabled={startStopBusy || pipeline.phase === 'loading'}
      onclick={togglePipeline}
    >
      {pipeline.status === 'running' ? 'Stop' : 'Start'}
    </button>
    <button
      type="button"
      class="cursor-pointer rounded border border-slate-500 bg-slate-800/60 px-2 py-0.5 text-[11px] text-slate-200 hover:border-sky-400 hover:bg-slate-700 hover:text-slate-50 disabled:cursor-not-allowed disabled:opacity-40"
      disabled={shotBusy}
      title="Capture the current spectrum/waterfall and store it on the server"
      onclick={takeScreenshot}
    >
      {shotBusy ? 'Saving…' : 'Screenshot'}
    </button>
    <button
      type="button"
      class="cursor-pointer rounded border border-slate-500 bg-slate-800/60 px-2 py-0.5 text-[11px] text-slate-200 hover:border-sky-400 hover:bg-slate-700 hover:text-slate-50"
      onclick={() => (showSource = true)}
    >
      Source…
    </button>
    <button
      type="button"
      class="cursor-pointer rounded border border-slate-500 bg-slate-800/60 px-2 py-0.5 text-[11px] text-slate-200 hover:border-sky-400 hover:bg-slate-700 hover:text-slate-50"
      onclick={() => (showFlowgraph = true)}
    >
      Flowgraph…
    </button>
  </div>
</div>

<SourceDialog
  bind:open={showSource}
  source={pipeline.source}
  onClose={() => (showSource = false)}
  onPickDevice={(caps) => {
    const state = defaultsFor(caps);
    if (state) void pipeline.patchSource(toSourceConfig(caps, state));
  }}
  onApply={(cfg: SourceConfig) => void pipeline.patchSource(cfg)}
/>

<FlowgraphDialog bind:open={showFlowgraph} onClose={() => (showFlowgraph = false)} />

<style>
  /* Lifted verbatim from the old +page.svelte top header so the
     start/stop button keeps its existing colour cue. */
  .running {
    border-color: rgb(248 113 113);
    color: rgb(248 113 113);
  }
  .running:hover {
    border-color: rgb(252 165 165);
  }
  .stopped {
    border-color: rgb(134 239 172);
    color: rgb(134 239 172);
  }
  .stopped:hover {
    border-color: rgb(187 247 208);
  }
  /* .channel-on removed with the old Channel/Advanced toggles — the
     new Main: dropdown + Channel checkbox don't need a custom "on"
     state (the native controls show it). */
</style>
