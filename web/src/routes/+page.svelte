<script lang="ts">
  import Workspace from '$lib/layout/Workspace.svelte';
  import LogPanel from '$lib/layout/LogPanel.svelte';
  import Split from '$lib/layout/Split.svelte';
  import BandsPanel from '$lib/presets/BandsPanel.svelte';
  import SettingsPanel from '$lib/presets/SettingsPanel.svelte';
  import SignalCatalog from '$lib/presets/SignalCatalog.svelte';
  import SourceDialog from '$lib/controls/SourceDialog.svelte';
  import FlowgraphDialog from '$lib/controls/FlowgraphDialog.svelte';
  import { defaultsFor, toSourceConfig } from '$lib/controls/optionsModel';
  import type { SourceConfig } from '$lib/api/source';
  import { demoAddInWorker } from '$lib/workers/demo-client';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { patchConsole } from '$lib/logs/store.svelte';
  import { connectServerLogs } from '$lib/logs/client';
  import { onMount } from 'svelte';

  let wasmStatus = $state<'pending' | 'ok' | string>('pending');
  let frameRate = $state(0);
  let lastFrameSize = $state(0);
  let showSource = $state(false);
  let showFlowgraph = $state(false);
  let leftTab = $state<'bands' | 'catalog' | 'settings'>('bands');

  async function runDemo() {
    try {
      const { sum } = await demoAddInWorker(1.5, 2.25);
      wasmStatus = Math.abs(sum - 3.75) < 1e-6 ? 'ok' : `wrong: ${sum}`;
    } catch (err) {
      wasmStatus = `error: ${err instanceof Error ? err.message : String(err)}`;
    }
  }

  onMount(() => {
    patchConsole();
    const disconnectLogs = connectServerLogs();
    void pipeline.init();
    return () => {
      disconnectLogs();
      pipeline.teardown();
    };
  });

  // Count frames per second against whichever client is current.
  $effect(() => {
    const c = pipeline.client;
    const fftSid = pipeline.uiSinks.fft?.stream_id;
    if (!c || fftSid === undefined) {
      frameRate = 0;
      return;
    }
    let counter = 0;
    let windowStart = performance.now();
    const unsub = c.subscribe(fftSid, (frame) => {
      counter += 1;
      lastFrameSize = frame.payload.length;
    });
    const timer = setInterval(() => {
      const now = performance.now();
      const dt = (now - windowStart) / 1000;
      frameRate = dt > 0 ? counter / dt : 0;
      counter = 0;
      windowStart = now;
    }, 1000);
    return () => {
      clearInterval(timer);
      unsub();
    };
  });

  $effect(() => {
    void runDemo();
  });

  // Pretty-print the active source for the header chip.
  let sourceLabel = $derived.by(() => {
    const s = pipeline.source;
    if (!s) return '';
    const axes = currentAxes(pipeline);
    const freq = axes ? `${(axes.center_freq_hz / 1e6).toFixed(3)} MHz` : '';
    if (s.type === 'SoapySource') return `soapy ${freq}`;
    if (s.type === 'FileSource') return 'file';
    if (s.type === 'SineSource') return `sine ${freq}`;
    return `${s.type} ${freq}`;
  });

  let startStopBusy = $state(false);
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

<div class="flex h-dvh w-dvw flex-col">
  <header class="flex items-center justify-between border-b border-slate-800 px-4 py-2">
    <div class="flex items-baseline gap-3">
      <h1 class="text-lg font-semibold">Ferrite</h1>
      <span class="text-xs text-[color:var(--color-muted)]">pre-alpha</span>
    </div>
    <div class="flex items-center gap-4 text-xs text-[color:var(--color-muted)]">
      <span>wasm: {wasmStatus}</span>
      <span>
        ws: {pipeline.wsStatus}
        {#if pipeline.wsStatus === 'open' && frameRate > 0}
          ({frameRate.toFixed(1)} fps, {lastFrameSize} B)
        {/if}
      </span>
      {#if sourceLabel}
        <span class="font-mono text-[11px]">{sourceLabel}</span>
      {/if}
      {#if pipeline.errorMessage}
        <span class="text-rose-400" title={pipeline.errorMessage}>error</span>
      {/if}
      <button
        type="button"
        class="rounded border px-2 py-0.5 text-xs font-semibold disabled:opacity-50"
        class:running={pipeline.status === 'running'}
        class:stopped={pipeline.status !== 'running'}
        disabled={startStopBusy || pipeline.phase === 'loading'}
        onclick={togglePipeline}
      >
        {pipeline.status === 'running' ? 'Stop' : 'Start'}
      </button>
      <button
        type="button"
        class="rounded border border-slate-700 px-2 py-0.5 text-xs text-[color:var(--color-fg)] hover:border-slate-600"
        onclick={() => (showSource = true)}
      >
        Source…
      </button>
      <button
        type="button"
        class="rounded border border-slate-700 px-2 py-0.5 text-xs text-[color:var(--color-fg)] hover:border-slate-600"
        onclick={() => (showFlowgraph = true)}
      >
        Flowgraph…
      </button>
    </div>
  </header>
  <div class="flex min-h-0 flex-1">
    <Split
      direction="row"
      defaultFraction={0.22}
      min={0.12}
      max={0.5}
      storageKey="ferrite.split.aside-main"
    >
      {#snippet a()}
        <aside class="flex h-full w-full flex-col">
          <Split direction="column" defaultFraction={0.5} storageKey="ferrite.split.aside-internal">
            {#snippet a()}
              <div class="flex h-full min-h-0 flex-col">
                <div class="flex border-b border-slate-800 text-[11px]">
                  <button
                    type="button"
                    class="flex-1 px-2 py-1 text-[color:var(--color-muted)] hover:bg-slate-900"
                    class:tab-active={leftTab === 'bands'}
                    onclick={() => (leftTab = 'bands')}>Bands</button
                  >
                  <button
                    type="button"
                    class="flex-1 px-2 py-1 text-[color:var(--color-muted)] hover:bg-slate-900"
                    class:tab-active={leftTab === 'catalog'}
                    onclick={() => (leftTab = 'catalog')}>Catalog</button
                  >
                  <button
                    type="button"
                    class="flex-1 px-2 py-1 text-[color:var(--color-muted)] hover:bg-slate-900"
                    class:tab-active={leftTab === 'settings'}
                    onclick={() => (leftTab = 'settings')}>Settings</button
                  >
                </div>
                <div class="min-h-0 flex-1">
                  {#if leftTab === 'bands'}
                    <BandsPanel />
                  {:else if leftTab === 'catalog'}
                    <SignalCatalog />
                  {:else}
                    <SettingsPanel />
                  {/if}
                </div>
              </div>
            {/snippet}
            {#snippet b()}
              <div class="h-full min-h-0">
                <LogPanel />
              </div>
            {/snippet}
          </Split>
        </aside>
      {/snippet}
      {#snippet b()}
        <div class="h-full min-w-0 w-full">
          {#if pipeline.client}
            <Workspace client={pipeline.client} />
          {:else}
            <div
              class="flex h-full items-center justify-center text-sm text-[color:var(--color-muted)]"
            >
              connecting…
            </div>
          {/if}
        </div>
      {/snippet}
    </Split>
  </div>
</div>

<SourceDialog
  bind:open={showSource}
  source={pipeline.source}
  onClose={() => (showSource = false)}
  onPickDevice={(caps) => {
    // No second dialog — defaults from optionsModel are live-editable
    // afterwards via the Settings → Input panel and spectrum header.
    const state = defaultsFor(caps);
    if (state) void pipeline.patchSource(toSourceConfig(caps, state));
  }}
  onApply={(cfg: SourceConfig) => void pipeline.patchSource(cfg)}
/>

<FlowgraphDialog bind:open={showFlowgraph} onClose={() => (showFlowgraph = false)} />

<style>
  .tab-active {
    color: var(--color-fg);
    background: rgba(125, 211, 252, 0.08);
    border-bottom: 1px solid #7dd3fc;
  }
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
