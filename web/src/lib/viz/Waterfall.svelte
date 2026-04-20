<script lang="ts">
  import { onMount } from 'svelte';
  import { pixelToFreqLinear, WaterfallRenderer } from './waterfall';
  import type { FrameClient } from '$lib/ws/client';
  import { FFT_STREAM, PayloadType } from '$lib/ws/frame';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';

  interface Props {
    client: FrameClient;
    streamId?: number;
    rows?: number;
  }

  let { client, streamId = FFT_STREAM, rows = 512 }: Props = $props();

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

  onMount(() => {
    if (!canvas) return;
    const renderer = new WaterfallRenderer(canvas, { rows });
    const unsub = client.subscribe(streamId, (frame) => {
      if (frame.header.payloadType !== PayloadType.FftU8) return;
      renderer.pushRow(frame.payload);
    });
    const ro = new ResizeObserver(() => renderer.resize());
    ro.observe(canvas);
    return () => {
      unsub();
      ro.disconnect();
      renderer.destroy();
    };
  });
</script>

<div bind:this={wrap} class="relative block h-full w-full">
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
