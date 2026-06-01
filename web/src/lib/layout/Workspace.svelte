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
  // `client.workspace.rightPane` selection being `channel` (the toolbar's
  // Display → Right dropdown; `signals` swaps in the strongest-signal
  // list, `off` collapses the column).
  import Spectrum from '$lib/viz/Spectrum.svelte';
  import Waterfall from '$lib/viz/Waterfall.svelte';
  import NarrowSpectrum from '$lib/viz/NarrowSpectrum.svelte';
  import NarrowWaterfall from '$lib/viz/NarrowWaterfall.svelte';
  import DisplayControls from '$lib/viz/DisplayControls.svelte';
  import Split from '$lib/layout/Split.svelte';
  import AppToolbar from '$lib/layout/AppToolbar.svelte';
  import RfQuickBar from '$lib/layout/RfQuickBar.svelte';
  import ViewBridge from '$lib/layout/ViewBridge.svelte';
  import { pipeline } from '$lib/pipeline.svelte';
  import { clientControls } from '$lib/control/clientStore.svelte';
  import { activeAdvancedView } from '$lib/advanced/registry';
  import { browserRuntime } from '$lib/runner/browserRuntime.svelte';
  import type { FrameClient } from '$lib/ws/client';

  interface Props {
    client: FrameClient;
  }

  let { client }: Props = $props();

  // Right (channel-detail) column. Driven by the toolbar's Display →
  // Right dropdown (`client.workspace.rightPane`): `channel` shows the
  // narrow FFT/waterfall, `off` collapses it. (`signals` — the
  // strongest-signal list — lands with the SignalList block; until then
  // its option is hidden because no preset advertises a `ui:signals`
  // sink.) The whole column only exists when the preset has a
  // Channelizer (`ui:fft_narrow`).
  let hasNarrowSink = $derived(pipeline.uiSinks.fft_narrow !== undefined);
  let rightPane = $derived(clientControls.get('client.workspace.rightPane'));
  let showNarrow = $derived(hasNarrowSink && rightPane === 'channel');

  // Advanced view replaces *only* the wide FFT/waterfall column when
  // toggled on (and the preset registers one). The Channel column is
  // independent — it still shows beside the advanced view if the
  // operator has it on.
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
  let showAdvanced = $derived(
    advancedView !== null && clientControls.get('client.workspace.mainPane') === 'advanced',
  );

  // Channel-detail header used to read "Channel · 240 kHz @ 100.001 MHz"
  // but the bandwidth + centre were redundant — the orange VFO Nixie
  // in the top RfQuickBar already shows the centre, and the channel
  // bandwidth is fixed per preset (operator picks it via the preset
  // selector). Bare "Channel" keeps the panel-head terse.
  const narrowHeaderLabel = 'Channel';
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
    /* Vertical padding bumped from 4 → 7 so the row's intrinsic height
       comfortably hosts the tallest control (Start button + dropdowns
       sit ~24 px) and the smaller chips (HealthDots ~9 px) stay
       optically centred. Previous 4 px left the dots reading low — the
       flex `align-items: center` is correct geometrically, but with
       the row barely taller than the buttons, the small dots looked
       sunken. */
    padding: 7px 8px;
    border-bottom: 1px solid rgb(30 41 59);
    background: var(--color-bg);
  }
  /* .panel-head moved to app.css (shared with every pane view). */
</style>
