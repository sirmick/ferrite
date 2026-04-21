<script lang="ts">
  // Fixed spectrum-over-waterfall canvas. Earlier iterations used
  // dockview to allow user-resizable panels, but the UX target is a
  // known-good hero layout — every "best SDR ever" app hard-pins the
  // spectrum above the waterfall so the eye can correlate a bin's
  // height with the same bin's colour beneath it. A draggable split
  // gains nothing here and costs us the mystery of where things went.
  //
  // The two panels are rendered into the same CSS flex column; the
  // Spectrum panel is a fixed-ish portion of the vertical space and
  // the Waterfall claims the rest. Neither child scrolls — both are
  // canvases that pin to their container's bounding box.
  import Spectrum from '$lib/viz/Spectrum.svelte';
  import Waterfall from '$lib/viz/Waterfall.svelte';
  import type { FrameClient } from '$lib/ws/client';

  interface Props {
    client: FrameClient;
  }

  let { client }: Props = $props();
</script>

<div class="flex h-full w-full min-h-0 flex-col">
  <section
    class="flex h-[40%] min-h-0 flex-col border-b border-slate-800 bg-[color:var(--color-bg)]"
  >
    <header class="panel-head">
      <span>Spectrum</span>
    </header>
    <div class="min-h-0 flex-1">
      <Spectrum {client} />
    </div>
  </section>
  <section class="flex h-[60%] min-h-0 flex-col bg-[color:var(--color-bg)]">
    <header class="panel-head">
      <span>Waterfall</span>
    </header>
    <div class="min-h-0 flex-1">
      <Waterfall {client} />
    </div>
  </section>
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
