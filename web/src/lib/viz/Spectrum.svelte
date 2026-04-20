<script lang="ts">
  import { onMount } from 'svelte';
  import { SpectrumRenderer } from './spectrum';
  import type { FrameClient } from '$lib/ws/client';
  import { FFT_STREAM, PayloadType } from '$lib/ws/frame';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import Nixie from '$lib/controls/Nixie.svelte';

  interface Props {
    client: FrameClient;
    streamId?: number;
  }

  let { client, streamId = FFT_STREAM }: Props = $props();

  let canvas: HTMLCanvasElement | undefined = $state();
  let renderer: SpectrumRenderer | undefined;

  let axes = $derived(currentAxes(pipeline));

  const STEPS = [
    { label: '1 Hz', hz: 1 },
    { label: '10 Hz', hz: 10 },
    { label: '100 Hz', hz: 100 },
    { label: '1 kHz', hz: 1_000 },
    { label: '10 kHz', hz: 10_000 },
    { label: '100 kHz', hz: 100_000 },
    { label: '1 MHz', hz: 1_000_000 },
  ] as const;
  let stepIdx = $state(4);
  let stepHz = $derived(STEPS[stepIdx].hz);

  let fade = $state(false);
  let maxHold = $state(false);

  function commitCenter(hz: number) {
    if (axes && hz !== axes.center_freq_hz) {
      void pipeline.patchSourceParams({ center_freq_hz: hz });
    }
  }

  function nudge(sign: 1 | -1) {
    if (!axes) return;
    const hz = axes.center_freq_hz + sign * stepHz;
    void pipeline.patchSourceParams({ center_freq_hz: hz });
  }

  function onWheel(ev: WheelEvent) {
    if (!axes) return;
    ev.preventDefault();
    nudge(ev.deltaY > 0 ? -1 : 1);
  }

  function onClick(ev: MouseEvent) {
    if (!renderer || !canvas || !axes) return;
    const rect = canvas.getBoundingClientRect();
    const f = renderer.pixelToFreq(ev.clientX - rect.left);
    if (f === undefined) return;
    void pipeline.patchSourceParams({ center_freq_hz: Math.round(f) });
  }

  onMount(() => {
    if (!canvas) return;
    renderer = new SpectrumRenderer(canvas);
    const unsub = client.subscribe(streamId, (frame) => {
      if (frame.header.payloadType !== PayloadType.FftU8) return;
      renderer!.setRow(frame.payload);
    });
    const ro = new ResizeObserver(() => renderer?.resize());
    ro.observe(canvas);
    return () => {
      unsub();
      ro.disconnect();
      renderer?.destroy();
      renderer = undefined;
    };
  });

  // Display-axis and flags propagate to the renderer on every change.
  // Floor/ceil come from the server-side logmag block; until the FFT
  // flowgraph wiring lands we use a fixed visible range.
  const DEFAULT_FLOOR_DBFS = -100;
  const DEFAULT_CEIL_DBFS = 0;
  $effect(() => {
    if (!renderer) return;
    if (!axes) {
      renderer.setAxes(undefined);
      return;
    }
    renderer.setAxes({
      centerHz: axes.center_freq_hz,
      rateHz: axes.sample_rate_hz,
      floorDbfs: DEFAULT_FLOOR_DBFS,
      ceilDbfs: DEFAULT_CEIL_DBFS,
    });
  });

  $effect(() => {
    renderer?.setFeatures({ fade, maxHold });
  });
</script>

<div class="flex h-full w-full flex-col">
  {#if axes}
    <div
      class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-slate-800 bg-[color:var(--color-bg)] px-2 py-1 text-[11px] text-[color:var(--color-muted)]"
    >
      <div class="flex items-center gap-1">
        <button
          type="button"
          class="rounded border border-slate-700 px-1 leading-none hover:border-slate-500"
          onclick={() => nudge(-1)}
          aria-label="decrease centre frequency">−</button
        >
        <Nixie hz={axes.center_freq_hz} onCommit={commitCenter} />
        <button
          type="button"
          class="rounded border border-slate-700 px-1 leading-none hover:border-slate-500"
          onclick={() => nudge(1)}
          aria-label="increase centre frequency">+</button
        >
      </div>

      <label class="flex items-center gap-1">
        <span>step</span>
        <select
          class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
          bind:value={stepIdx}
        >
          {#each STEPS as s, i (i)}
            <option value={i}>{s.label}</option>
          {/each}
        </select>
      </label>

      <div class="flex items-center gap-1">
        <span>span</span>
        <span class="font-mono text-slate-300">{(axes.sample_rate_hz / 1e6).toFixed(3)} MHz</span>
      </div>

      <div class="mx-1 h-4 border-l border-slate-800"></div>

      <label class="flex items-center gap-1" title="fade trail of recent traces">
        <input type="checkbox" bind:checked={fade} />
        <span>fade</span>
      </label>
      <label class="flex items-center gap-1" title="running max-hold trace">
        <input type="checkbox" bind:checked={maxHold} />
        <span>max hold</span>
      </label>
      {#if maxHold}
        <button
          type="button"
          class="rounded border border-slate-700 px-1.5 py-0 text-[10px] leading-none hover:border-slate-500"
          onclick={() => renderer?.resetMaxHold()}
        >
          reset
        </button>
      {/if}
    </div>
  {/if}
  <canvas
    bind:this={canvas}
    onclick={onClick}
    onwheel={onWheel}
    class="block min-h-0 w-full flex-1 cursor-crosshair"
    title="click to tune · wheel to nudge by step"
  ></canvas>
</div>
