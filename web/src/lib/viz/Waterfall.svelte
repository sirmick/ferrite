<script lang="ts">
  import { onMount } from 'svelte';
  import { pixelToFreqLinear, WaterfallRenderer, type WaterfallPalette } from './waterfall';
  import type { FrameClient } from '$lib/ws/client';
  import { PayloadType } from '$lib/ws/frame';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';

  interface Props {
    client: FrameClient;
    rows?: number;
  }

  let { client, rows = 512 }: Props = $props();

  // Toolbar state. Paused gates the ring-buffer upload so the user can
  // stare at a specific burst without it scrolling out of view. Palette
  // swaps the colour LUT in-place; no history loss either way.
  let paused = $state(false);
  let palette = $state<WaterfallPalette>('digi');

  let fftStreamId = $derived(pipeline.uiSinks.fft?.stream_id);

  let canvas: HTMLCanvasElement | undefined = $state();
  let wrap: HTMLDivElement | undefined = $state();

  let axes = $derived(currentAxes(pipeline));

  // Pointer drag state: `dragX` is the current pointer CSS-X relative to
  // the canvas. While null the cursor sits at geometric centre — the VFO
  // already lives at `center_freq_hz`, which the waterfall re-centres on.
  let dragging = $state(false);
  let dragX = $state<number | null>(null);

  function pointerFreq(clientX: number): number | null {
    if (!canvas || !axes) return null;
    const rect = canvas.getBoundingClientRect();
    return pixelToFreqLinear(
      clientX - rect.left,
      rect.width,
      axes.center_freq_hz,
      axes.sample_rate_hz,
    );
  }

  function onPointerDown(ev: PointerEvent) {
    if (!axes || !canvas) return;
    if (ev.button !== 0) return;
    (ev.currentTarget as HTMLElement).setPointerCapture(ev.pointerId);
    dragging = true;
    const rect = canvas.getBoundingClientRect();
    dragX = ev.clientX - rect.left;
  }

  function onPointerMove(ev: PointerEvent) {
    if (!dragging || !canvas) return;
    const rect = canvas.getBoundingClientRect();
    dragX = ev.clientX - rect.left;
  }

  function onPointerUp(ev: PointerEvent) {
    if (!dragging) return;
    (ev.currentTarget as HTMLElement).releasePointerCapture(ev.pointerId);
    dragging = false;
    const f = pointerFreq(ev.clientX);
    dragX = null;
    if (f !== null) void pipeline.patchSourceParams({ center_freq_hz: Math.round(f) });
  }

  function onPointerCancel(ev: PointerEvent) {
    if (!dragging) return;
    (ev.currentTarget as HTMLElement).releasePointerCapture(ev.pointerId);
    dragging = false;
    dragX = null;
  }

  let renderer: WaterfallRenderer | undefined;

  onMount(() => {
    if (!canvas) return;
    renderer = new WaterfallRenderer(canvas, { rows });
    const ro = new ResizeObserver(() => renderer?.resize());
    ro.observe(canvas);
    return () => {
      ro.disconnect();
      renderer?.destroy();
      renderer = undefined;
    };
  });

  $effect(() => {
    const sid = fftStreamId;
    if (sid === undefined) return;
    const unsub = client.subscribe(sid, (frame) => {
      if (frame.header.payloadType !== PayloadType.FftU8) return;
      if (paused) return;
      renderer?.pushRow(frame.payload);
    });
    return unsub;
  });

  $effect(() => {
    renderer?.setPalette(palette);
  });
</script>

<div bind:this={wrap} class="relative flex h-full w-full flex-col">
  <div
    class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-slate-800 bg-[color:var(--color-bg)] px-2 py-1 text-[11px] text-[color:var(--color-muted)]"
  >
    <button
      type="button"
      class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none hover:border-slate-500"
      class:bg-amber-900={paused}
      onclick={() => (paused = !paused)}
      title={paused ? 'resume scrolling' : 'freeze the current view'}
    >
      {paused ? '▶ resume' : '❚❚ pause'}
    </button>
    <label class="flex items-center gap-1" title="colour palette">
      <span>palette</span>
      <select
        class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
        bind:value={palette}
      >
        <option value="digi">digi</option>
        <option value="viridis">viridis</option>
      </select>
    </label>
  </div>
  <div class="relative min-h-0 flex-1">
    <canvas
      bind:this={canvas}
      class="block h-full w-full touch-none select-none"
      class:cursor-grabbing={dragging}
      class:cursor-grab={!dragging}
      onpointerdown={onPointerDown}
      onpointermove={onPointerMove}
      onpointerup={onPointerUp}
      onpointercancel={onPointerCancel}
      title="drag to retune"
    ></canvas>
    {#if axes}
      <div
        class="pointer-events-none absolute top-0 bottom-0 w-px"
        class:bg-sky-400={!dragging}
        class:bg-amber-400={dragging}
        style:left={dragX != null ? `${dragX}px` : '50%'}
        style:box-shadow={dragging
          ? '0 0 4px rgba(251, 191, 36, 0.8)'
          : '0 0 3px rgba(56, 189, 248, 0.6)'}
      ></div>
    {/if}
  </div>
</div>
