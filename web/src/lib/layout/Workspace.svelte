<script lang="ts">
  // Three-level layout. From outside in:
  //
  //   ┌────────────────────────────────────────────────────────────┐
  //   │ Toolbar  (HealthDots, source label, Start, Source…, etc.)  │
  //   ├──────────────────────────────────┬─────────────────────────┤
  //   │ Wide Spectrum                    │ Channel Spectrum        │
  //   ├──────────────────────────────────┤                         │
  //   │ Wide Waterfall                   ├─────────────────────────┤
  //   │                                  │ Channel Waterfall       │
  //   └──────────────────────────────────┴─────────────────────────┘
  //
  // The toolbar (AppToolbar) used to live inside the wide-Spectrum
  // panel header — that was fine when the workspace was a single
  // column, but with a narrow-channel column nested beside it the
  // toolbar visually "belonged" to only the left half. Lifting it
  // here makes it global. Sibling Splits:
  //
  //   - Outer horizontal split (wide | narrow), persisted under
  //     `ferrite.split.workspace-columns`.
  //   - Each column has its own vertical Split (spectrum | waterfall);
  //     the wide column keeps its existing storage key so users'
  //     existing splitter habits carry across.
  //
  // The narrow column is gated on (a) the runtime having injected a
  // `ui:fft_narrow` sink (only true when the active preset has a
  // Channelizer — `inject_narrow_fft.rs`) AND (b) the operator's
  // `client.workspace.narrowVisible` toggle.
  import Spectrum from '$lib/viz/Spectrum.svelte';
  import Waterfall from '$lib/viz/Waterfall.svelte';
  import NarrowSpectrum from '$lib/viz/NarrowSpectrum.svelte';
  import NarrowWaterfall from '$lib/viz/NarrowWaterfall.svelte';
  import DisplayControls from '$lib/viz/DisplayControls.svelte';
  import Split from '$lib/layout/Split.svelte';
  import AppToolbar from '$lib/layout/AppToolbar.svelte';
  import RfQuickBar from '$lib/layout/RfQuickBar.svelte';
  import ViewBridge from '$lib/layout/ViewBridge.svelte';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { activeAdvancedView } from '$lib/advanced/registry';
  import type { FrameClient } from '$lib/ws/client';

  interface Props {
    client: FrameClient;
  }

  let { client }: Props = $props();

  let hasNarrowSink = $derived(pipeline.uiSinks.fft_narrow !== undefined);
  let narrowVisible = $derived(clientControls.get('client.workspace.narrowVisible'));
  let showNarrow = $derived(hasNarrowSink && narrowVisible);

  // Advanced view replaces *only* the wide FFT/waterfall column when
  // toggled on (and the preset registers one). The Channel column is
  // independent — it still shows beside the advanced view if the
  // operator has it on.
  let advancedView = $derived(activeAdvancedView(pipeline.uiSinks));
  let showAdvanced = $derived(
    advancedView !== null && clientControls.get('client.workspace.advancedVisible'),
  );

  // Channel-detail header label: "Channel · 240 kHz @ 100.001 MHz" so
  // the operator can read the channel width + absolute centre without
  // mental arithmetic. Falls back to a bare "Channel" when axes
  // haven't resolved yet.
  let narrowHeaderLabel = $derived.by(() => {
    const axes = currentAxes(pipeline);
    const vfoBlock = Object.values(pipeline.blocks).find((b) =>
      b.spec.params.some((p) => p.key === 'freq_shift_hz'),
    );
    const vfo = (vfoBlock?.values as Record<string, unknown> | null | undefined) ?? null;
    if (!axes || !vfo) return 'Channel';
    const shift = typeof vfo.freq_shift_hz === 'number' ? vfo.freq_shift_hz : 0;
    const center = axes.center_freq_hz + shift;
    let bw: number | undefined;
    if (typeof vfo.output_rate_hz === 'number' && vfo.output_rate_hz > 0) {
      bw = vfo.output_rate_hz;
    } else if (typeof vfo.factor === 'number' && vfo.factor > 0) {
      bw = axes.sample_rate_hz / vfo.factor;
    }
    const bwStr = bw === undefined ? '' : ` · ${(bw / 1e3).toFixed(0)} kHz`;
    return `Channel${bwStr} @ ${(center / 1e6).toFixed(3)} MHz`;
  });
</script>

<div class="flex h-full w-full min-h-0 flex-col">
  <!-- Hierarchy reads top → bottom:
         1. AppToolbar       — status only (HealthDots, ws/fps, source
                                label, error chip).
         2. RfQuickBar       — operator-active controls (VFO nixie,
                                SDR centre, rate, LiveControls,
                                Start/Source/Flowgraph buttons).
         3. 2×2 panel grid   — Wide spectrum / Channel spectrum,
                                Wide waterfall / Channel waterfall.
                                Both columns start at the same Y so
                                they line up as a grid; RfQuickBar
                                lives above the columns rather than
                                inside the wide one for that reason.
         4. DisplayControls  — visual knobs (FFT size/window/smooth,
                                fade/peak/bands, spec + wf range,
                                zoom, pause, Channel toggle). One
                                strip below the grid steers both
                                renderers. -->
  <!-- Headless WS bridge: the AI's `ferrite-ctl view <pane>` round-trip
       lands here; opens `/ws/ui-views`, snapshots the registered canvas
       via the viewRegistry module, sends the PNG back. No DOM. -->
  <ViewBridge />

  <header class="toolbar-row">
    <AppToolbar />
  </header>
  <RfQuickBar />

  <div class="flex min-h-0 flex-1">
    {#if showNarrow}
      <Split direction="row" defaultFraction={0.65} storageKey="ferrite.split.workspace-columns">
        {#snippet a()}
          {@render (showAdvanced ? advancedColumn : wideColumn)()}
        {/snippet}
        {#snippet b()}
          {@render narrowColumn()}
        {/snippet}
      </Split>
    {:else}
      {@render (showAdvanced ? advancedColumn : wideColumn)()}
    {/if}
  </div>

  <DisplayControls />
</div>

{#snippet wideColumn()}
  <div class="flex h-full w-full min-h-0 flex-col">
    <Split direction="column" defaultFraction={0.4} storageKey="ferrite.split.spectrum-waterfall">
      {#snippet a()}
        <section class="flex h-full min-h-0 flex-col bg-[color:var(--color-bg)]">
          <header class="panel-head">
            <span>Spectrum</span>
          </header>
          <div class="min-h-0 flex-1">
            <Spectrum {client} />
          </div>
        </section>
      {/snippet}
      {#snippet b()}
        <section class="flex h-full min-h-0 flex-col bg-[color:var(--color-bg)]">
          <header class="panel-head">
            <span>Waterfall</span>
          </header>
          <div class="min-h-0 flex-1">
            <Waterfall {client} />
          </div>
        </section>
      {/snippet}
    </Split>
  </div>
{/snippet}

{#snippet advancedColumn()}
  {#if advancedView}
    {@const Advanced = advancedView.component}
    <Advanced />
  {/if}
{/snippet}

{#snippet narrowColumn()}
  <div class="flex h-full w-full min-h-0 flex-col border-l border-slate-800">
    <Split direction="column" defaultFraction={0.4} storageKey="ferrite.split.narrow-spec-wf">
      {#snippet a()}
        <section class="flex h-full min-h-0 flex-col bg-[color:var(--color-bg)]">
          <header class="panel-head">
            <span class="truncate" title={narrowHeaderLabel}>{narrowHeaderLabel}</span>
          </header>
          <div class="min-h-0 flex-1">
            <NarrowSpectrum {client} />
          </div>
        </section>
      {/snippet}
      {#snippet b()}
        <section class="flex h-full min-h-0 flex-col bg-[color:var(--color-bg)]">
          <header class="panel-head">
            <span>Channel waterfall</span>
          </header>
          <div class="min-h-0 flex-1">
            <NarrowWaterfall {client} />
          </div>
        </section>
      {/snippet}
    </Split>
  </div>
{/snippet}

<style>
  .toolbar-row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 4px 8px;
    border-bottom: 1px solid rgb(30 41 59);
    background: var(--color-bg);
  }
  .panel-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 3px 8px;
    border-bottom: 1px solid rgb(30 41 59);
    font-size: 11px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--color-muted);
  }
</style>
