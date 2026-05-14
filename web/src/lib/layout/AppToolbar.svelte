<script lang="ts">
  // Self-contained toolbar lifted out of `+page.svelte` and embedded
  // into the Spectrum panel-head — frees a full row of vertical real
  // estate, which is at a premium on small displays. Everything the
  // toolbar shows (wasm/ws/source status, FFT framerate, error chip,
  // start/stop, Source… and Flowgraph… dialogs) lives here so the page
  // shell stays empty above the workspace split.

  import HealthDots from './HealthDots.svelte';
  import SourceDialog from '$lib/controls/SourceDialog.svelte';
  import FlowgraphDialog from '$lib/controls/FlowgraphDialog.svelte';
  import { defaultsFor, toSourceConfig } from '$lib/controls/optionsModel';
  import type { SourceConfig } from '$lib/api/source';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';

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
  {#if pipeline.errorMessage}
    <span class="text-rose-400" title={pipeline.errorMessage}>error</span>
  {/if}
  <!-- Stop/Source/Flowgraph push to the right edge — they're terminal
       actions (mutate global state), the status chips on the left stay
       at the natural reading position. -->
  <div class="ml-auto flex flex-wrap items-center gap-x-2 gap-y-1">
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
      class="rounded border border-slate-700 px-2 py-0.5 text-[11px] hover:border-slate-600"
      onclick={() => (showSource = true)}
    >
      Source…
    </button>
    <button
      type="button"
      class="rounded border border-slate-700 px-2 py-0.5 text-[11px] hover:border-slate-600"
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
</style>
