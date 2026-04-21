<script lang="ts">
  import { onMount } from 'svelte';
  import { SpectrumRenderer } from './spectrum';
  import type { FrameClient } from '$lib/ws/client';
  import { PayloadType } from '$lib/ws/frame';
  import { pipeline, currentAxes } from '$lib/pipeline.svelte';
  import Nixie from '$lib/controls/Nixie.svelte';

  interface Props {
    client: FrameClient;
  }

  let { client }: Props = $props();

  // stream_id for the preset's `ui:fft` sink — env_split allocates it
  // from 1000+ in doc order, so the server tells us which one to
  // subscribe to rather than the client guessing.
  let fftStreamId = $derived(pipeline.uiSinks.fft?.stream_id);

  let canvas: HTMLCanvasElement | undefined = $state();
  let renderer: SpectrumRenderer | undefined;

  let axes = $derived(currentAxes(pipeline));

  // The "VFO block" is whichever block in the composed preset exposes a
  // `freq_shift_hz` param — that's the channelizer baseband-shift knob
  // today. When present, it drives the orange Nixie; absent, we hide
  // the VFO controls so bareband presets stay uncluttered.
  let vfoBlock = $derived(
    Object.values(pipeline.blocks).find((b) =>
      b.spec.params.some((p) => p.key === 'freq_shift_hz'),
    ),
  );
  let vfoShiftHz = $derived.by(() => {
    if (!vfoBlock) return 0;
    const v = (vfoBlock.values as Record<string, unknown> | null)?.freq_shift_hz;
    return typeof v === 'number' ? v : 0;
  });
  let vfoAbsHz = $derived((axes?.center_freq_hz ?? 0) + vfoShiftHz);

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

  // Committing the VFO writes the offset (`freq_shift_hz = target −
  // center`) so the absolute listening frequency matches what the user
  // typed. Clamp to the visible span so the UI doesn't silently tune
  // outside the channelizer's passband.
  function commitVfo(hz: number) {
    if (!axes || !vfoBlock) return;
    const shift = hz - axes.center_freq_hz;
    const half = axes.sample_rate_hz / 2;
    const clamped = Math.max(-half, Math.min(half, shift));
    if (clamped !== vfoShiftHz) {
      void pipeline.setBlockParam(vfoBlock.id, 'freq_shift_hz', clamped);
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
      renderer?.setRow(frame.payload);
    });
    return unsub;
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

  // Vertical markers over the plot: green = SDR tuner LO, orange = VFO
  // (only when the preset exposes a `freq_shift_hz` knob). They drop
  // into the renderer so they redraw with every frame, not just on
  // axes change.
  $effect(() => {
    if (!renderer) return;
    renderer.setMarkers({
      sdrCenterHz: axes?.center_freq_hz,
      vfoHz: vfoBlock ? vfoAbsHz : undefined,
    });
  });
</script>

<div class="flex h-full w-full flex-col">
  {#if axes}
    <div
      class="flex flex-wrap items-center gap-x-3 gap-y-1 border-b border-slate-800 bg-[color:var(--color-bg)] px-2 py-1 text-[11px] text-[color:var(--color-muted)]"
    >
      {#if vfoBlock}
        <div class="flex items-center gap-1" title="VFO — what you're listening to">
          <span class="mr-0.5 text-[9px] uppercase tracking-wider text-orange-400/70">vfo</span>
          <Nixie hz={vfoAbsHz} onCommit={commitVfo} tone="orange" />
        </div>
      {/if}

      <div class="flex items-center gap-1" title="SDR centre — the RF tuner LO">
        <span class="mr-0.5 text-[9px] uppercase tracking-wider text-emerald-400/70">sdr</span>
        <button
          type="button"
          class="rounded border border-slate-700 px-1 leading-none hover:border-slate-500"
          onclick={() => nudge(-1)}
          aria-label="decrease centre frequency">−</button
        >
        <Nixie hz={axes.center_freq_hz} onCommit={commitCenter} tone="green" />
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
