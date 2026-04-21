<script lang="ts">
  // Spectrum-over-waterfall canvas. Both panes are canvases pinned to
  // their container — the user can drag the divider between them to
  // bias the layout (more spectrum height when staring at peak shapes,
  // more waterfall when watching history). The split fraction persists
  // across sessions via localStorage.
  import Spectrum from '$lib/viz/Spectrum.svelte';
  import Waterfall from '$lib/viz/Waterfall.svelte';
  import Split from '$lib/layout/Split.svelte';
  import type { FrameClient } from '$lib/ws/client';

  interface Props {
    client: FrameClient;
  }

  let { client }: Props = $props();
</script>

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

<style>
  .panel-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 2px 8px;
    border-bottom: 1px solid rgb(30 41 59);
    font-size: 9px;
    font-weight: 700;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--color-muted);
  }
</style>
