<script lang="ts">
  import { onMount } from 'svelte';
  import { SpectrumRenderer, type SpectrumStats } from './spectrum';
  import type { FrameClient } from '$lib/ws/client';
  import { FFT_STREAM, PayloadType } from '$lib/ws/frame';
  import { session } from '$lib/session.svelte';
  import { firstChannel, rangesToChoices } from '$lib/controls/optionsModel';
  import Nixie from '$lib/controls/Nixie.svelte';

  interface Props {
    client: FrameClient;
    streamId?: number;
  }

  let { client, streamId = FFT_STREAM }: Props = $props();

  let canvas: HTMLCanvasElement | undefined = $state();
  let renderer: SpectrumRenderer | undefined;

  let s = $derived(session.state);
  let ch = $derived(session.caps ? firstChannel(session.caps) : null);
  let rateChoices = $derived(ch ? rangesToChoices(ch.sample_rate_ranges_hz) : []);

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
  let autoFloor = $state(false);

  // Latest row stats, sampled by the renderer on every frame.
  let lastStats: SpectrumStats | null = null;

  function commitCenter(hz: number) {
    if (s && hz !== s.center_freq_hz) void session.patch({ center_freq_hz: hz });
  }

  function nudge(sign: 1 | -1) {
    if (!s) return;
    const hz = s.center_freq_hz + sign * stepHz;
    void session.patch({ center_freq_hz: hz });
  }

  function onSpanChange(ev: Event) {
    const v = Number((ev.target as HTMLSelectElement).value);
    if (!Number.isFinite(v) || !s || v === s.sample_rate_hz) return;
    void session.restart({ sample_rate_hz: v });
  }

  function onWheel(ev: WheelEvent) {
    if (!s) return;
    ev.preventDefault();
    nudge(ev.deltaY > 0 ? -1 : 1);
  }

  function onClick(ev: MouseEvent) {
    if (!renderer || !canvas || !s) return;
    const rect = canvas.getBoundingClientRect();
    const f = renderer.pixelToFreq(ev.clientX - rect.left);
    if (f === undefined) return;
    void session.patch({ center_freq_hz: Math.round(f) });
  }

  onMount(() => {
    if (!canvas) return;
    renderer = new SpectrumRenderer(canvas);
    renderer.onStats((st) => {
      lastStats = st;
    });
    const unsub = client.subscribe(streamId, (frame) => {
      if (frame.header.payloadType !== PayloadType.FftU8) return;
      renderer!.setRow(frame.payload);
    });
    const ro = new ResizeObserver(() => renderer?.resize());
    ro.observe(canvas);

    // Auto-floor loop — runs at 1 Hz regardless of framerate, adjusting
    // `floor_dbfs` / `ceil_dbfs` so the spectrum byte distribution fills
    // the plot area nicely. Target: p10 ≈ 10 % from bottom, p99 ≈ 90 %.
    const TARGET_P10 = 25; // byte value we want the 10th-percentile to sit at
    const TARGET_P99 = 230; // byte value we want the 99th-percentile to sit at
    const MAX_SHIFT_DB = 5; // clamp so one tick never slews more than this
    const autoTimer = setInterval(() => {
      if (!autoFloor || !s || !lastStats) return;
      const span = s.ceil_dbfs - s.floor_dbfs;
      if (span <= 0) return;
      const pxPerByte = span / 255;
      const floorShift = clamp(
        (lastStats.p10 - TARGET_P10) * pxPerByte,
        -MAX_SHIFT_DB,
        MAX_SHIFT_DB,
      );
      const ceilShift = clamp(
        (lastStats.p99 - TARGET_P99) * pxPerByte,
        -MAX_SHIFT_DB,
        MAX_SHIFT_DB,
      );
      if (Math.abs(floorShift) < 0.5 && Math.abs(ceilShift) < 0.5) return;
      const next = {
        floor_dbfs: clamp(s.floor_dbfs + floorShift, -150, -20),
        ceil_dbfs: clamp(s.ceil_dbfs + ceilShift, -50, 20),
      };
      void session.patch(next);
    }, 1000);

    return () => {
      clearInterval(autoTimer);
      unsub();
      ro.disconnect();
      renderer?.destroy();
      renderer = undefined;
    };
  });

  $effect(() => {
    if (!renderer) return;
    if (!s) {
      renderer.setAxes(undefined);
      return;
    }
    renderer.setAxes({
      centerHz: s.center_freq_hz,
      rateHz: s.sample_rate_hz,
      floorDbfs: s.floor_dbfs,
      ceilDbfs: s.ceil_dbfs,
    });
  });

  $effect(() => {
    renderer?.setFeatures({ fade, maxHold });
  });

  function patchNum(field: 'floor_dbfs' | 'ceil_dbfs' | 'alpha', ev: Event) {
    const v = Number((ev.target as HTMLInputElement).value);
    if (Number.isFinite(v)) void session.patch({ [field]: v });
  }

  function clamp(x: number, lo: number, hi: number): number {
    return Math.min(hi, Math.max(lo, x));
  }
</script>

<div class="flex h-full w-full flex-col">
  {#if s}
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
        <Nixie hz={s.center_freq_hz} onCommit={commitCenter} />
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

      <label class="flex items-center gap-1">
        <span>span</span>
        {#if rateChoices.length > 1}
          <select
            class="rounded border border-slate-800 bg-slate-900 px-1 py-0.5 text-slate-200"
            value={s.sample_rate_hz}
            onchange={onSpanChange}
          >
            {#each rateChoices as r (r)}
              <option value={r}>{(r / 1e6).toFixed(3)} MHz</option>
            {/each}
          </select>
          <span class="text-[10px] text-amber-400">restart</span>
        {:else}
          <span class="font-mono text-slate-300">{(s.sample_rate_hz / 1e6).toFixed(3)} MHz</span>
        {/if}
      </label>

      <div class="mx-1 h-4 border-l border-slate-800"></div>

      <label class="flex items-center gap-1">
        <span>floor</span>
        <input
          type="range"
          min="-150"
          max="-20"
          step="1"
          value={s.floor_dbfs}
          oninput={(e) => patchNum('floor_dbfs', e)}
          class="w-24 accent-sky-400"
          disabled={autoFloor}
        />
        <span class="w-8 font-mono text-slate-300">{s.floor_dbfs.toFixed(0)}</span>
      </label>
      <label class="flex items-center gap-1">
        <span>ceil</span>
        <input
          type="range"
          min="-50"
          max="20"
          step="1"
          value={s.ceil_dbfs}
          oninput={(e) => patchNum('ceil_dbfs', e)}
          class="w-24 accent-sky-400"
          disabled={autoFloor}
        />
        <span class="w-8 font-mono text-slate-300">{s.ceil_dbfs.toFixed(0)}</span>
      </label>
      <label class="flex items-center gap-1">
        <span>α</span>
        <input
          type="range"
          min="0"
          max="1"
          step="0.05"
          value={s.alpha}
          oninput={(e) => patchNum('alpha', e)}
          class="w-20 accent-sky-400"
        />
        <span class="w-8 font-mono text-slate-300">{s.alpha.toFixed(2)}</span>
      </label>

      <div class="mx-1 h-4 border-l border-slate-800"></div>

      <label class="flex items-center gap-1" title="auto-adjust floor/ceiling once per second">
        <input type="checkbox" bind:checked={autoFloor} />
        <span>auto</span>
      </label>
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
