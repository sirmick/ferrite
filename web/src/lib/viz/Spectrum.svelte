<script lang="ts">
  import { onMount } from 'svelte';
  import { SpectrumRenderer } from './spectrum';
  import type { FrameClient } from '$lib/ws/client';
  import { FFT_STREAM, PayloadType } from '$lib/ws/frame';

  interface Props {
    client: FrameClient;
    streamId?: number;
  }

  let { client, streamId = FFT_STREAM }: Props = $props();

  let canvas: HTMLCanvasElement | undefined = $state();

  onMount(() => {
    if (!canvas) return;
    const renderer = new SpectrumRenderer(canvas);
    const unsub = client.subscribe(streamId, (frame) => {
      if (frame.header.payloadType !== PayloadType.FftU8) return;
      renderer.setRow(frame.payload);
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
