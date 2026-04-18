<script lang="ts">
  import { onMount } from 'svelte';
  import { WaterfallRenderer } from './waterfall';
  import type { FrameClient } from '$lib/ws/client';
  import { FFT_STREAM, PayloadType } from '$lib/ws/frame';

  interface Props {
    client: FrameClient;
    streamId?: number;
    rows?: number;
  }

  let { client, streamId = FFT_STREAM, rows = 512 }: Props = $props();

  let canvas: HTMLCanvasElement | undefined = $state();

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

<canvas bind:this={canvas} class="block h-full w-full"></canvas>
