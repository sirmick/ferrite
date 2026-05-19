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
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { applyControl } from '$lib/control/dispatch';
  import { activeAdvancedView } from '$lib/advanced/registry';
  import { browserRuntime } from '$lib/runner/browserRuntime.svelte';
  import { snapshotView } from '$lib/viz/viewRegistry';
  import { logs } from '$lib/logs/store.svelte';

  // Channel-detail pane toggle. Disabled when the active preset has no
  // Channelizer (no runtime-injected `ui:fft_narrow` sink).
  let hasNarrow = $derived(pipeline.uiSinks.fft_narrow !== undefined);
  let narrowVisible = $derived(clientControls.get('client.workspace.narrowVisible'));

  // Advanced-view toggle. The active preset's registered view (or
  // null); when null the button is disabled. Independent of Channel —
  // Advanced replaces only the wide FFT/waterfall column.
  let advancedView = $derived(
    activeAdvancedView({
      uiSinks: pipeline.uiSinks,
      voiceTranscribeIds: browserRuntime.voiceTranscribeIds,
    }),
  );
  let advancedVisible = $derived(clientControls.get('client.workspace.advancedVisible'));

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

  let sourceLabel = $derived.by(() => {
    const s = pipeline.source;
    if (!s) return '';
    const axes = currentAxes(pipeline);
    const freq = axes ? `${(axes.center_freq_hz / 1e6).toFixed(3)} MHz` : '';
    const caps = pipeline.sourceCaps;
    if (s.type === 'SoapySource') {
      const dev = caps?.kind === 'hardware' ? caps.capabilities.info.label : 'soapy';
      return `${dev} ${freq}`.trim();
    }
    if (s.type === 'FileSource') return 'file';
    if (s.type === 'SineSource') return `sine ${freq}`;
    return `${s.type} ${freq}`;
  });

  let shotBusy = $state(false);
  // Grab the main canvas the operator is looking at and persist it
  // server-side via POST /api/screenshot. Whichever wide pane is
  // mounted wins (waterfall when an advanced view hasn't replaced it,
  // else the spectrum); the registry returns '' for an unmounted pane.
  async function takeScreenshot() {
    if (shotBusy) return;
    shotBusy = true;
    try {
      let shot = snapshotView('wide-waterfall');
      if (shot.error || !shot.png_b64) shot = snapshotView('wide-spectrum');
      if (shot.error || !shot.png_b64) {
        logs.push('client', 'warn', `screenshot: no canvas to capture (${shot.error || 'empty'})`);
        return;
      }
      const res = await fetch('/api/screenshot', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ png_b64: shot.png_b64, label: sourceLabel }),
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
      shotBusy = false;
    }
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
  <span class="hidden md:inline">
    ws: {pipeline.wsStatus}
    {#if pipeline.wsStatus === 'open' && frameRate > 0}
      ({frameRate.toFixed(1)} fps · {fmtBw(wsBwBps)})
    {/if}
  </span>
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
    <button
      type="button"
      class="cursor-pointer rounded border bg-slate-800/60 px-2 py-0.5 text-[11px] text-slate-200 hover:border-sky-400 hover:text-slate-50 disabled:cursor-not-allowed disabled:opacity-40"
      class:channel-on={hasNarrow && narrowVisible}
      class:border-slate-500={!(hasNarrow && narrowVisible)}
      disabled={!hasNarrow}
      title={hasNarrow
        ? narrowVisible
          ? 'Hide channel-detail FFT / waterfall'
          : 'Show channel-detail FFT / waterfall'
        : 'Active preset has no channelizer — no channel-detail pane available'}
      onclick={() => void applyControl('client.workspace.narrowVisible', !narrowVisible)}
    >
      Channel
    </button>
    <button
      type="button"
      class="cursor-pointer rounded border bg-slate-800/60 px-2 py-0.5 text-[11px] text-slate-200 hover:border-sky-400 hover:text-slate-50 disabled:cursor-not-allowed disabled:opacity-40"
      class:channel-on={advancedView && advancedVisible}
      class:border-slate-500={!(advancedView && advancedVisible)}
      disabled={!advancedView}
      title={advancedView
        ? advancedVisible
          ? `Hide ${advancedView.label} advanced view — show spectrum`
          : `Show ${advancedView.label} advanced view — replaces the wide FFT / waterfall`
        : 'Active preset has no advanced view'}
      onclick={() => void applyControl('client.workspace.advancedVisible', !advancedVisible)}
    >
      {advancedView ? advancedView.label : 'Advanced'}
    </button>
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
  /* Sky-blue accent matches the VFO/channel marker on the spectrum. */
  .channel-on {
    border-color: rgb(125 211 252);
    color: rgb(125 211 252);
  }
  .channel-on:hover {
    border-color: rgb(186 230 253);
  }
</style>
